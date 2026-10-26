//! The incident views: the 24-hour strip under the banner and the full
//! `/incidents` log page.
//!
//! Everything that turns incidents into display values is a plain function
//! of `(incidents, now)`, so it can be tested without a database or a clock.

use askama::Template;
use axum::{extract::State, response::Response};
use labwatch_core::{
    history::{History, unix_now},
    incidents::Incident,
    status::Status,
};

use crate::{AppState, dashboard::render};

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// The log page shows this far back. Incidents are kept for 90 days.
const LOG_WINDOW: i64 = 30 * DAY;

/// A duration in the largest one or two units that matter:
/// "45 s", "14 min", "2 h 5 min", "3 d 4 h".
pub fn human_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (days, hours, mins) = (secs / DAY, secs % DAY / HOUR, secs % HOUR / MINUTE);

    match (days, hours, mins) {
        (0, 0, 0) => format!("{secs} s"),
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, 0, _) => format!("{d} d"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

/// How long ago a moment was, roughly: "just now", "14 min ago", "3 d ago".
pub fn ago(ts: i64, now: i64) -> String {
    let secs = now - ts;
    if secs < MINUTE {
        return "just now".into();
    }
    // Only the largest unit, so "3 h ago" rather than "3 h 12 min ago".
    let largest = human_duration(secs)
        .split(' ')
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    format!("{largest} ago")
}

/// The short label used for a status in text, e.g. in a tooltip.
fn status_word(status: Status) -> &'static str {
    match status {
        Status::Critical => "critical",
        Status::Warning => "warning",
        Status::Unknown => "unknown",
        Status::Ok => "ok",
    }
}

// ---- The 24-hour strip ----

/// One incident drawn on the strip, positioned as percentages of the day.
pub struct Mark {
    pub check: String,
    /// Handwritten-style label above an ongoing incident. Neighbours that
    /// started close together share one label, set on the last of them.
    pub note: Option<String>,
    pub left: f64,
    pub width: f64,
    pub status: Status,
    pub open: bool,
    /// Tooltip text, e.g. "Jellyfin · critical · 14 min · 3 h ago".
    pub label: String,
}

#[derive(Template)]
#[template(path = "timeline.html")]
pub struct TimelineTemplate {
    /// "All clear" styling when nothing happened in the last day.
    pub status: Status,
    pub summary: String,
    pub marks: Vec<Mark>,
}

impl TimelineTemplate {
    /// Builds the strip from incidents that touched the last 24 hours.
    pub fn new(incidents: &[Incident], now: i64) -> Self {
        let window_start = now - DAY;
        // `ended_at` is None for open incidents: the same check as
        // `incidents_since` in the query, repeated here so the function
        // is correct even if handed older incidents.
        let recent: Vec<&Incident> = incidents
            .iter()
            .filter(|i| i.ended_at.is_none_or(|end| end >= window_start))
            .collect();

        let marks = recent
            .iter()
            .map(|incident| {
                // Clamp the start to the window, so an incident that began
                // yesterday starts at the left edge.
                let start = incident.started_at.max(window_start);
                let end = incident.ended_at.unwrap_or(now).min(now);
                Mark {
                    check: incident.check.clone(),
                    note: None,
                    left: percent(start - window_start, DAY),
                    width: percent(end - start, DAY),
                    status: incident.worst_status,
                    open: incident.is_open(),
                    label: mark_label(incident, now),
                }
            })
            .collect();
        let marks = label_open_marks(marks);

        let worst = recent
            .iter()
            .map(|i| i.worst_status)
            .max()
            .unwrap_or(Status::Ok);

        Self {
            status: worst,
            summary: summary(&recent, now),
            marks,
        }
    }
}

/// Ongoing incidents that start within this share of the day of each
/// other share one label, so their names don't print on top of each other.
const NOTE_GROUP_PERCENT: f64 = 6.0;

/// Gives each cluster of ongoing incidents one label listing their checks,
/// e.g. "Jellyfin, Sonarr". Names that differ only in case (the Jellyfin
/// service and its jellyfin container) are listed once.
fn label_open_marks(mut marks: Vec<Mark>) -> Vec<Mark> {
    // Indices of the open marks, left to right.
    let mut open: Vec<usize> = (0..marks.len()).filter(|&i| marks[i].open).collect();
    open.sort_by(|&a, &b| marks[a].left.total_cmp(&marks[b].left));

    // `chunk_by` splits a slice wherever the closure returns false: here,
    // wherever the gap to the next mark is wider than the grouping distance.
    let groups: Vec<Vec<usize>> = open
        .chunk_by(|&a, &b| marks[b].left - marks[a].left <= NOTE_GROUP_PERCENT)
        .map(<[usize]>::to_vec)
        .collect();

    for group in groups {
        let mut names: Vec<String> = Vec::new();
        for &i in &group {
            let name = &marks[i].check;
            if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                names.push(name.clone());
            }
        }
        // A group is never empty, so `last` always finds a mark.
        if let Some(&last) = group.last() {
            marks[last].note = Some(names.join(", "));
        }
    }
    marks
}

fn percent(part: i64, whole: i64) -> f64 {
    (part as f64 / whole as f64 * 100.0).clamp(0.0, 100.0)
}

fn mark_label(incident: &Incident, now: i64) -> String {
    let when = if incident.is_open() {
        format!(
            "ongoing for {}",
            human_duration(incident.duration_secs(now))
        )
    } else {
        format!(
            "{}, {}",
            human_duration(incident.duration_secs(now)),
            ago(incident.started_at, now)
        )
    };
    format!(
        "{} · {} · {} · {}",
        incident.check,
        status_word(incident.worst_status),
        when,
        incident.detail
    )
}

/// The one-line summary beside the strip.
fn summary(recent: &[&Incident], now: i64) -> String {
    let open = recent.iter().filter(|i| i.is_open()).count();
    let longest = recent.iter().map(|i| i.duration_secs(now)).max();

    let count = match recent.len() {
        0 => return "Nothing has gone wrong".into(),
        1 => "1 incident".to_string(),
        n => format!("{n} incidents"),
    };

    match open {
        // `longest` is Some whenever `recent` isn't empty, which the match
        // above already guaranteed. `unwrap_or_default` keeps it panic-free.
        0 => format!(
            "{count}, longest {}",
            human_duration(longest.unwrap_or_default())
        ),
        n => format!("{count}, {n} still going"),
    }
}

/// Renders the strip, or `None` if the query or template failed. The
/// dashboard still works without it, so failures are logged, not fatal.
pub async fn render_strip(history: &History, now: i64) -> Option<String> {
    let incidents = match history.incidents_since(now - DAY).await {
        Ok(incidents) => incidents,
        Err(err) => {
            eprintln!("failed to load incidents: {err:#}");
            return None;
        }
    };
    TimelineTemplate::new(&incidents, now).render().ok()
}

// ---- The /incidents page ----

/// One row of the log, with every value already formatted for display.
pub struct LogRow {
    pub check: String,
    pub collector: String,
    pub status: Status,
    pub detail: String,
    pub started_at: i64,
    pub started_ago: String,
    pub duration: String,
    pub open: bool,
    pub interrupted: bool,
}

impl LogRow {
    fn new(incident: Incident, now: i64) -> Self {
        Self {
            duration: human_duration(incident.duration_secs(now)),
            started_ago: ago(incident.started_at, now),
            open: incident.is_open(),
            check: incident.check,
            collector: incident.collector,
            status: incident.worst_status,
            detail: incident.detail,
            started_at: incident.started_at,
            interrupted: incident.interrupted,
        }
    }
}

#[derive(Template)]
#[template(path = "incidents.html")]
pub struct IncidentsTemplate {
    pub version: &'static str,
    pub stylesheet: &'static str,
    pub rows: Vec<LogRow>,
    pub open: usize,
}

pub async fn incidents_page(State(state): State<AppState>) -> Response {
    let now = unix_now();
    let incidents = match state.history.incidents_since(now - LOG_WINDOW).await {
        Ok(incidents) => incidents,
        Err(err) => {
            eprintln!("failed to load incidents: {err:#}");
            Vec::new()
        }
    };

    let rows: Vec<LogRow> = incidents.into_iter().map(|i| LogRow::new(i, now)).collect();
    let open = rows.iter().filter(|r| r.open).count();

    render(&IncidentsTemplate {
        version: labwatch_core::version(),
        stylesheet: crate::dashboard::stylesheet_href(),
        rows,
        open,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 100 * DAY;

    fn incident(check: &str, started_ago: i64, lasted: Option<i64>, status: Status) -> Incident {
        let started_at = NOW - started_ago;
        Incident {
            id: 0,
            collector: "services".into(),
            check: check.into(),
            started_at,
            ended_at: lasted.map(|secs| started_at + secs),
            worst_status: status,
            detail: "unreachable".into(),
            interrupted: false,
        }
    }

    #[test]
    fn durations_use_one_or_two_units() {
        assert_eq!(human_duration(45), "45 s");
        assert_eq!(human_duration(14 * MINUTE), "14 min");
        assert_eq!(human_duration(2 * HOUR), "2 h");
        assert_eq!(human_duration(2 * HOUR + 5 * MINUTE), "2 h 5 min");
        assert_eq!(human_duration(3 * DAY + 4 * HOUR + 59), "3 d 4 h");
        assert_eq!(human_duration(-5), "0 s");
    }

    #[test]
    fn ago_keeps_only_the_largest_unit() {
        assert_eq!(ago(NOW - 10, NOW), "just now");
        assert_eq!(ago(NOW - 14 * MINUTE, NOW), "14 min ago");
        assert_eq!(ago(NOW - (3 * HOUR + 12 * MINUTE), NOW), "3 h ago");
        assert_eq!(ago(NOW - 2 * DAY, NOW), "2 d ago");
    }

    #[test]
    fn empty_day_is_all_clear() {
        let strip = TimelineTemplate::new(&[], NOW);
        assert_eq!(strip.summary, "Nothing has gone wrong");
        assert_eq!(strip.status, Status::Ok);
        assert!(strip.marks.is_empty());
    }

    #[test]
    fn summary_counts_and_finds_the_longest() {
        let incidents = [
            incident("Sonarr", 5 * HOUR, Some(14 * MINUTE), Status::Warning),
            incident("Radarr", 2 * HOUR, Some(3 * MINUTE), Status::Critical),
        ];
        let strip = TimelineTemplate::new(&incidents, NOW);
        assert_eq!(strip.summary, "2 incidents, longest 14 min");
        assert_eq!(strip.status, Status::Critical);
    }

    #[test]
    fn ongoing_incidents_lead_the_summary() {
        let incidents = [incident("Jellyfin", 20 * MINUTE, None, Status::Critical)];
        let strip = TimelineTemplate::new(&incidents, NOW);
        assert_eq!(strip.summary, "1 incident, 1 still going");
        assert!(strip.marks[0].open);
        // An open incident runs to the right edge.
        let mark = &strip.marks[0];
        assert!((mark.left + mark.width - 100.0).abs() < 0.01);
    }

    #[test]
    fn marks_are_placed_as_a_share_of_the_day() {
        // Started 12 h ago and lasted 6 h: middle of the strip, a quarter wide.
        let incidents = [incident(
            "Sonarr",
            12 * HOUR,
            Some(6 * HOUR),
            Status::Warning,
        )];
        let mark = &TimelineTemplate::new(&incidents, NOW).marks[0];
        assert!((mark.left - 50.0).abs() < 0.01);
        assert!((mark.width - 25.0).abs() < 0.01);
    }

    #[test]
    fn incidents_from_yesterday_are_clamped_to_the_left_edge() {
        // Started 30 h ago, ended 20 h ago: only the last 4 h of it show.
        let incidents = [incident(
            "Sonarr",
            30 * HOUR,
            Some(10 * HOUR),
            Status::Warning,
        )];
        let mark = &TimelineTemplate::new(&incidents, NOW).marks[0];
        assert_eq!(mark.left, 0.0);
        assert!((mark.width - percent(4 * HOUR, DAY)).abs() < 0.01);
    }

    #[test]
    fn incidents_that_ended_before_the_window_are_left_out() {
        let incidents = [incident("Sonarr", 3 * DAY, Some(HOUR), Status::Warning)];
        assert!(TimelineTemplate::new(&incidents, NOW).marks.is_empty());
    }

    #[test]
    fn ongoing_neighbours_share_one_label() {
        let incidents = [
            incident("Jellyfin", 10 * MINUTE, None, Status::Critical),
            incident("jellyfin", 9 * MINUTE, None, Status::Critical),
            incident("Sonarr", 8 * MINUTE, None, Status::Warning),
            incident("Radarr", 12 * HOUR, None, Status::Warning),
            incident("old", 5 * HOUR, Some(MINUTE), Status::Warning),
        ];
        let marks = TimelineTemplate::new(&incidents, NOW).marks;
        let notes: Vec<&str> = marks.iter().filter_map(|m| m.note.as_deref()).collect();
        // One label for the cluster at the right edge, one for Radarr on its
        // own, none for the incident that's over.
        assert_eq!(notes, vec!["Jellyfin, Sonarr", "Radarr"]);
    }

    #[test]
    fn labels_describe_the_incident() {
        let closed = incident("Sonarr", 3 * HOUR, Some(14 * MINUTE), Status::Warning);
        assert_eq!(
            mark_label(&closed, NOW),
            "Sonarr · warning · 14 min, 3 h ago · unreachable"
        );
        let open = incident("Jellyfin", 20 * MINUTE, None, Status::Critical);
        assert_eq!(
            mark_label(&open, NOW),
            "Jellyfin · critical · ongoing for 20 min · unreachable"
        );
    }
}
