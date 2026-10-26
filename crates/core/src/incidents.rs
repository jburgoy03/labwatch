//! The incident log: when each check broke, how bad it got and when it
//! recovered. Fed by [`Transition`]s, stored next to the history.

use anyhow::Result;
use serde::Serialize;

use crate::{
    history::{DAY, History},
    status::Status,
    transitions::{Transition, is_problem},
};

/// Closed incidents older than this are deleted.
const INCIDENT_RETENTION: i64 = 90 * DAY;

/// One problem with one check, from the moment it broke until it recovered.
#[derive(Debug, Clone, Serialize)]
pub struct Incident {
    pub id: i64,
    pub collector: String,
    pub check: String,
    pub started_at: i64,
    /// `None` while the incident is still open.
    pub ended_at: Option<i64>,
    /// Warning, or Critical if it got that bad at any point.
    pub worst_status: Status,
    /// What the check said when it first broke, e.g. "timed out after 5s".
    pub detail: String,
    /// Labwatch restarted while this was open, so `ended_at` is when we
    /// stopped watching, not necessarily when it recovered.
    pub interrupted: bool,
}

impl Incident {
    pub fn is_open(&self) -> bool {
        self.ended_at.is_none()
    }

    /// How long it lasted, or has lasted so far if still open.
    pub fn duration_secs(&self, now: i64) -> i64 {
        self.ended_at.unwrap_or(now) - self.started_at
    }
}

/// What a transition means for the incident log.
#[derive(Debug, PartialEq)]
enum Action {
    /// Fine to a problem: open an incident.
    Open,
    /// Problem to fine: close it.
    Close,
    /// Problem to a different problem (Warning <-> Critical): same incident.
    Update,
    /// Fine to fine (OK <-> Unknown): nothing to log.
    Ignore,
}

impl Action {
    /// A plain function on two booleans, so the rule is easy to test.
    fn for_change(change: &Transition) -> Self {
        match (is_problem(change.from), is_problem(change.to)) {
            (false, true) => Action::Open,
            (true, false) => Action::Close,
            (true, true) => Action::Update,
            (false, false) => Action::Ignore,
        }
    }
}

/// The incident queries live in their own module but extend `History`,
/// so they share its connection pool. Rust lets you add `impl` blocks for
/// a type anywhere in the crate that defines it, a bit like a C# partial class.
impl History {
    /// Records one transition in the incident log.
    pub async fn apply_transition(&self, ts: i64, change: &Transition) -> Result<()> {
        let worst = change.to.rank();

        match Action::for_change(change) {
            Action::Open => self.open_incident(ts, change).await?,
            Action::Close => {
                sqlx::query!(
                    "UPDATE incidents SET ended_at = ?
                     WHERE collector = ? AND check_name = ? AND ended_at IS NULL",
                    ts,
                    change.collector,
                    change.check,
                )
                .execute(&self.pool)
                .await?;
            }
            Action::Update => {
                // MAX keeps Critical if it drops back to Warning.
                let updated = sqlx::query!(
                    "UPDATE incidents SET worst_status = MAX(worst_status, ?)
                     WHERE collector = ? AND check_name = ? AND ended_at IS NULL",
                    worst,
                    change.collector,
                    change.check,
                )
                .execute(&self.pool)
                .await?
                .rows_affected();

                // No open incident to update, e.g. it was closed as
                // interrupted by a restart. Start a fresh one instead.
                if updated == 0 {
                    self.open_incident(ts, change).await?;
                }
            }
            Action::Ignore => {}
        }

        Ok(())
    }

    async fn open_incident(&self, ts: i64, change: &Transition) -> Result<()> {
        let worst = change.to.rank();
        sqlx::query!(
            "INSERT INTO incidents (collector, check_name, started_at, worst_status, detail)
             VALUES (?, ?, ?, ?, ?)",
            change.collector,
            change.check,
            ts,
            worst,
            change.detail,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Closes every incident left open by a previous run. Called once at
    /// startup: after a restart we can't know whether those checks
    /// recovered while we were down. If one is still broken, `Transitions`
    /// reports it again and a new incident opens.
    pub async fn close_interrupted_incidents(&self, ts: i64) -> Result<u64> {
        let closed = sqlx::query!(
            "UPDATE incidents SET ended_at = ?, interrupted = 1 WHERE ended_at IS NULL",
            ts,
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(closed)
    }

    /// Incidents that touched the window from `since` to now: still open,
    /// or ended after `since`. Newest first.
    pub async fn incidents_since(&self, since: i64) -> Result<Vec<Incident>> {
        let incidents = sqlx::query!(
            "SELECT id, collector, check_name, started_at, ended_at,
                    worst_status, detail, interrupted
             FROM incidents
             WHERE ended_at IS NULL OR ended_at >= ?
             ORDER BY started_at DESC",
            since,
        )
        .fetch_all(&self.pool)
        .await?
        // Map database rows into our own type. The row struct is generated
        // by `query!` from the SELECT, with types read from the schema.
        .into_iter()
        .map(|row| Incident {
            id: row.id,
            collector: row.collector,
            check: row.check_name,
            started_at: row.started_at,
            ended_at: row.ended_at,
            worst_status: Status::from_rank(row.worst_status),
            detail: row.detail,
            interrupted: row.interrupted != 0,
        })
        .collect();

        Ok(incidents)
    }

    /// Deletes closed incidents past their retention. Run with the hourly rollup.
    pub async fn prune_incidents(&self, now: i64) -> Result<()> {
        let cutoff = now - INCIDENT_RETENTION;
        sqlx::query!(
            "DELETE FROM incidents WHERE ended_at IS NOT NULL AND ended_at < ?",
            cutoff,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Status::{Critical as C, Ok as O, Unknown as U, Warning as W};

    fn change(check: &str, from: Status, to: Status) -> Transition {
        Transition {
            collector: "services".into(),
            check: check.into(),
            from,
            to,
            detail: format!("{check} {to}"),
        }
    }

    #[test]
    fn transitions_map_to_actions() {
        assert_eq!(Action::for_change(&change("a", O, C)), Action::Open);
        assert_eq!(Action::for_change(&change("a", U, W)), Action::Open);
        assert_eq!(Action::for_change(&change("a", C, O)), Action::Close);
        assert_eq!(Action::for_change(&change("a", W, C)), Action::Update);
        assert_eq!(Action::for_change(&change("a", O, U)), Action::Ignore);
    }

    #[tokio::test]
    async fn open_then_close() {
        let history = History::in_memory().await.unwrap();

        history
            .apply_transition(100, &change("Jellyfin", O, C))
            .await
            .unwrap();
        let open = history.incidents_since(0).await.unwrap();
        assert_eq!(open.len(), 1);
        assert!(open[0].is_open());
        assert_eq!(open[0].detail, "Jellyfin CRIT");
        assert_eq!(open[0].duration_secs(160), 60);

        history
            .apply_transition(400, &change("Jellyfin", C, O))
            .await
            .unwrap();
        let closed = history.incidents_since(0).await.unwrap();
        assert_eq!(closed[0].ended_at, Some(400));
        assert_eq!(closed[0].duration_secs(9_999), 300);
    }

    #[tokio::test]
    async fn worst_status_survives_de_escalation() {
        let history = History::in_memory().await.unwrap();

        history
            .apply_transition(100, &change("Sonarr", O, W))
            .await
            .unwrap();
        history
            .apply_transition(200, &change("Sonarr", W, C))
            .await
            .unwrap();
        history
            .apply_transition(300, &change("Sonarr", C, W))
            .await
            .unwrap();

        let incidents = history.incidents_since(0).await.unwrap();
        assert_eq!(incidents.len(), 1, "escalation is the same incident");
        assert_eq!(incidents[0].worst_status, C);
        assert!(incidents[0].is_open());
    }

    #[tokio::test]
    async fn restart_closes_open_incidents_as_interrupted() {
        let history = History::in_memory().await.unwrap();
        history
            .apply_transition(100, &change("Radarr", O, C))
            .await
            .unwrap();

        assert_eq!(history.close_interrupted_incidents(500).await.unwrap(), 1);

        let incidents = history.incidents_since(0).await.unwrap();
        assert!(incidents[0].interrupted);
        assert_eq!(incidents[0].ended_at, Some(500));

        // Still broken after the restart: a W -> C change with nothing open
        // starts a fresh incident rather than being lost.
        history
            .apply_transition(600, &change("Radarr", W, C))
            .await
            .unwrap();
        let incidents = history.incidents_since(0).await.unwrap();
        assert_eq!(incidents.len(), 2);
        assert!(incidents[0].is_open(), "newest first");
    }

    #[tokio::test]
    async fn only_one_open_incident_per_check() {
        let history = History::in_memory().await.unwrap();
        history
            .apply_transition(100, &change("Jellyfin", O, C))
            .await
            .unwrap();
        // The partial unique index refuses a second open row.
        assert!(
            history
                .apply_transition(200, &change("Jellyfin", O, C))
                .await
                .is_err()
        );
        // A different check is fine.
        history
            .apply_transition(200, &change("Sonarr", O, C))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn incidents_since_skips_old_closed_ones() {
        let history = History::in_memory().await.unwrap();
        history
            .apply_transition(100, &change("old", O, C))
            .await
            .unwrap();
        history
            .apply_transition(200, &change("old", C, O))
            .await
            .unwrap();
        history
            .apply_transition(1_000, &change("new", O, W))
            .await
            .unwrap();

        let recent = history.incidents_since(500).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].check, "new");
    }

    #[tokio::test]
    async fn prune_keeps_open_incidents() {
        let history = History::in_memory().await.unwrap();
        history
            .apply_transition(0, &change("closed", O, C))
            .await
            .unwrap();
        history
            .apply_transition(10, &change("closed", C, O))
            .await
            .unwrap();
        history
            .apply_transition(0, &change("open", O, C))
            .await
            .unwrap();

        history.prune_incidents(100 * DAY).await.unwrap();

        let left = history.incidents_since(0).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].check, "open");
    }
}
