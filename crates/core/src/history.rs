//! Stores check results in SQLite and serves them back for charts.
//!
//! Raw samples are kept for about a day; older data lives on as hourly
//! averages for 30 days. `rollup` does both the averaging and the cleanup.

use std::{
    path::Path,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::status::{Section, Status};

const HOUR: i64 = 60 * 60;
const DAY: i64 = 24 * HOUR;

/// Samples older than this are deleted. One hour more than a day, so the
/// oldest hour is still complete when it gets rolled up.
const SAMPLE_RETENTION: i64 = DAY + HOUR;
const HOURLY_RETENTION: i64 = 30 * DAY;

/// Current Unix time in seconds.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Which history a chart wants.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub enum Range {
    /// Raw samples from the last 24 hours.
    #[default]
    #[serde(rename = "24h")]
    Day,
    /// Hourly averages from the last 7 days.
    #[serde(rename = "7d")]
    Week,
}

/// One point on a chart.
#[derive(Debug, Clone, Serialize)]
pub struct Point {
    pub ts: i64,
    pub status: Status,
    pub value: Option<f64>,
}

/// Cheap to clone: `SqlitePool` is a handle to a shared pool of connections.
#[derive(Clone)]
pub struct History {
    pool: SqlitePool,
}

impl History {
    /// Opens (or creates) the database file and runs any new migrations.
    pub async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // Write-ahead logging lets readers and the writer work at once.
            .journal_mode(SqliteJournalMode::Wal);

        Self::connect(options, 4)
            .await
            .with_context(|| format!("failed to open database {}", path.display()))
    }

    /// A throwaway in-memory database, for tests. One connection, because
    /// every in-memory connection would otherwise get its own empty database.
    pub async fn in_memory() -> Result<Self> {
        Self::connect(SqliteConnectOptions::from_str("sqlite::memory:")?, 1).await
    }

    async fn connect(options: SqliteConnectOptions, max_connections: u32) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await?;

        // `migrate!` embeds every file in ./migrations into the binary at
        // compile time, then applies the ones this database hasn't seen.
        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(Self { pool })
    }

    /// Saves every check in a section as one sample each, in a single
    /// transaction so a section is never half-written.
    pub async fn record(&self, ts: i64, section: &Section) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        for check in &section.checks {
            let status = check.status.rank();
            // `query!` checks this SQL against the real schema at compile
            // time: a wrong column name or type is a build error.
            sqlx::query!(
                "INSERT INTO samples (ts, collector, check_name, status, value)
                 VALUES (?, ?, ?, ?, ?)",
                ts,
                section.collector,
                check.name,
                status,
                check.value,
            )
            // `&mut *tx`: `tx` is a Transaction that derefs to a connection.
            // `*tx` gets the connection, `&mut` borrows it for this query.
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Chart points for one check.
    pub async fn series(
        &self,
        collector: &str,
        check_name: &str,
        range: Range,
        now: i64,
    ) -> Result<Vec<Point>> {
        let points = match range {
            Range::Day => {
                let since = now - DAY;
                sqlx::query!(
                    "SELECT ts, status, value FROM samples
                     WHERE collector = ? AND check_name = ? AND ts >= ?
                     ORDER BY ts",
                    collector,
                    check_name,
                    since,
                )
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|row| Point {
                    ts: row.ts,
                    status: Status::from_rank(row.status),
                    value: row.value,
                })
                .collect()
            }
            Range::Week => {
                let since = now - 7 * DAY;
                sqlx::query!(
                    "SELECT hour, worst_status, avg_value FROM hourly
                     WHERE collector = ? AND check_name = ? AND hour >= ?
                     ORDER BY hour",
                    collector,
                    check_name,
                    since,
                )
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|row| Point {
                    ts: row.hour,
                    status: Status::from_rank(row.worst_status),
                    value: row.avg_value,
                })
                .collect()
            }
        };

        Ok(points)
    }

    /// Averages the last day's complete hours into `hourly`, then deletes
    /// data past its retention. Safe to run repeatedly: recomputing an hour
    /// overwrites it with the same numbers.
    pub async fn rollup(&self, now: i64) -> Result<()> {
        let current_hour = now - now % HOUR;
        let since = current_hour - DAY;
        let sample_cutoff = now - SAMPLE_RETENTION;
        let hourly_cutoff = now - HOURLY_RETENTION;

        let mut tx = self.pool.begin().await?;

        sqlx::query!(
            "INSERT INTO hourly
                 (hour, collector, check_name, worst_status,
                  avg_value, min_value, max_value, sample_count)
             SELECT ts - ts % 3600, collector, check_name, MAX(status),
                    AVG(value), MIN(value), MAX(value), COUNT(*)
             FROM samples
             WHERE ts >= ? AND ts < ?
             GROUP BY ts - ts % 3600, collector, check_name
             ON CONFLICT (collector, check_name, hour) DO UPDATE SET
                 worst_status = excluded.worst_status,
                 avg_value    = excluded.avg_value,
                 min_value    = excluded.min_value,
                 max_value    = excluded.max_value,
                 sample_count = excluded.sample_count",
            since,
            current_hour,
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query!("DELETE FROM samples WHERE ts < ?", sample_cutoff)
            .execute(&mut *tx)
            .await?;

        sqlx::query!("DELETE FROM hourly WHERE hour < ?", hourly_cutoff)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Check;

    fn cpu(value: f64, status: Status) -> Section {
        Section::new(
            "host",
            vec![Check::new("CPU", status, "").with_value(value)],
        )
    }

    // `#[tokio::test]` gives each async test its own runtime.
    #[tokio::test]
    async fn records_and_reads_back_samples() {
        let history = History::in_memory().await.unwrap();
        let now = 1_000_000;

        history
            .record(now - 10, &cpu(20.0, Status::Ok))
            .await
            .unwrap();
        history
            .record(now, &cpu(90.0, Status::Warning))
            .await
            .unwrap();

        let points = history
            .series("host", "CPU", Range::Day, now)
            .await
            .unwrap();

        assert_eq!(points.len(), 2);
        assert_eq!(points[1].value, Some(90.0));
        assert_eq!(points[1].status, Status::Warning);
    }

    #[tokio::test]
    async fn rollup_averages_an_hour_and_keeps_worst_status() {
        let history = History::in_memory().await.unwrap();
        let hour = 1_000 * HOUR;

        history
            .record(hour + 60, &cpu(10.0, Status::Ok))
            .await
            .unwrap();
        history
            .record(hour + 120, &cpu(30.0, Status::Critical))
            .await
            .unwrap();

        // Run it during the following hour, so `hour` is complete.
        let now = hour + HOUR + 30;
        history.rollup(now).await.unwrap();

        let points = history
            .series("host", "CPU", Range::Week, now)
            .await
            .unwrap();

        assert_eq!(points.len(), 1);
        assert_eq!(points[0].ts, hour);
        assert_eq!(points[0].value, Some(20.0));
        assert_eq!(points[0].status, Status::Critical);
    }

    #[tokio::test]
    async fn rollup_deletes_old_samples() {
        let history = History::in_memory().await.unwrap();
        let now = 100 * DAY;

        history
            .record(now - 2 * DAY, &cpu(50.0, Status::Ok))
            .await
            .unwrap();
        history.rollup(now).await.unwrap();

        // Look back further than 24h to prove the row is really gone.
        let points = history
            .series("host", "CPU", Range::Day, now - DAY)
            .await
            .unwrap();
        assert!(points.is_empty());
    }
}
