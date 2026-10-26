//! Turns the stream of collector results into status *changes*.
//!
//! Collectors report the full picture every few seconds. Most of the time
//! nothing has changed, so the interesting question is "what's different?".
//! `Transitions` remembers each check's last confirmed status and reports a
//! [`Transition`] only when a check settles on a new one.
//!
//! The incident log and phone alerts both build on this, so they agree on
//! exactly when something broke and when it recovered.

use std::collections::HashMap;

use crate::status::{Section, Status};

/// How many runs in a row a new status must hold before it counts.
/// With 2, one slow HTTP response doesn't become an incident.
pub const DEFAULT_CONFIRM_RUNS: u32 = 2;

/// A check that settled on a new status, e.g. Jellyfin going OK -> CRIT.
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub collector: String,
    pub check: String,
    pub from: Status,
    pub to: Status,
    /// The check's detail at the moment it changed, e.g. "timed out after 5s".
    pub detail: String,
}

impl Transition {
    /// True when the check went from fine to a problem (Warning or worse).
    /// Escalations like Warning -> Critical are not a new problem.
    pub fn is_problem_start(&self) -> bool {
        !is_problem(self.from) && is_problem(self.to)
    }

    /// True when the check went from a problem back to fine.
    pub fn is_recovery(&self) -> bool {
        is_problem(self.from) && !is_problem(self.to)
    }
}

/// Warning and Critical are problems. Unknown isn't: it means "can't tell",
/// like a machine with no temperature sensors, and shouldn't page anyone.
pub fn is_problem(status: Status) -> bool {
    status >= Status::Warning
}

/// What we remember about one check between runs.
#[derive(Debug)]
struct Tracked {
    /// The status we last reported.
    confirmed: Status,
    /// A different status we've seen but not yet confirmed, and how many
    /// runs in a row it has held.
    candidate: Option<(Status, u32)>,
}

/// Remembers every check's status and reports confirmed changes.
///
/// Not shared between tasks: each subscriber (incident log, alerts) owns
/// its own `Transitions`, so no lock is needed.
#[derive(Debug)]
pub struct Transitions {
    confirm_runs: u32,
    /// Keyed by (collector, check name). Check names are only unique within
    /// a collector: "host" and "docker" could both have a "CPU" check.
    checks: HashMap<(String, String), Tracked>,
}

impl Default for Transitions {
    fn default() -> Self {
        Self::new(DEFAULT_CONFIRM_RUNS)
    }
}

impl Transitions {
    /// `confirm_runs` of 1 reports every change immediately.
    pub fn new(confirm_runs: u32) -> Self {
        Self {
            // `max(1)`: 0 would mean "confirmed before it's seen", which
            // makes no sense, so treat it as 1.
            confirm_runs: confirm_runs.max(1),
            checks: HashMap::new(),
        }
    }

    /// Feeds in a collector's latest result and returns any checks that
    /// settled on a new status this run.
    ///
    /// A check seen for the first time is assumed to have been OK. So a
    /// service that's already down when Labwatch starts still produces an
    /// OK -> CRIT transition, once confirmed, rather than going unnoticed.
    pub fn observe(&mut self, section: &Section) -> Vec<Transition> {
        let mut changes = Vec::new();

        for check in &section.checks {
            let key = (section.collector.clone(), check.name.clone());

            // `entry().or_insert_with()` is like C#'s
            // `TryGetValue`, then `Add` if missing, in one lookup.
            let tracked = self.checks.entry(key).or_insert_with(|| Tracked {
                confirmed: Status::Ok,
                candidate: None,
            });

            // `if let` runs the block only when `step` returns `Some`,
            // and unpacks the tuple inside it in the same line.
            if let Some((from, to)) = step(tracked, check.status, self.confirm_runs) {
                changes.push(Transition {
                    collector: section.collector.clone(),
                    check: check.name.clone(),
                    from,
                    to,
                    detail: check.detail.clone(),
                });
            }
        }

        changes
    }

    /// The confirmed status of one check, if we've seen it.
    pub fn status_of(&self, collector: &str, check: &str) -> Option<Status> {
        self.checks
            .get(&(collector.to_string(), check.to_string()))
            .map(|tracked| tracked.confirmed)
    }
}

/// Advances one check by one observation. When a new status is confirmed,
/// returns `(from, to)` and updates `tracked.confirmed`.
///
/// A free function rather than a method so it can be tested on its own and
/// doesn't need to borrow the whole `Transitions` map.
fn step(tracked: &mut Tracked, observed: Status, confirm_runs: u32) -> Option<(Status, Status)> {
    if observed == tracked.confirmed {
        // Back to normal before the change was confirmed: forget it.
        tracked.candidate = None;
        return None;
    }

    // Count this run toward the candidate. A different candidate than last
    // time (CRIT, then WARN) restarts the count.
    let runs = match tracked.candidate {
        Some((status, runs)) if status == observed => runs + 1,
        _ => 1,
    };

    if runs >= confirm_runs {
        let from = tracked.confirmed;
        tracked.confirmed = observed;
        tracked.candidate = None;
        Some((from, observed))
    } else {
        tracked.candidate = Some((observed, runs));
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Check;

    fn section(checks: &[(&str, Status)]) -> Section {
        Section::new(
            "services",
            checks
                .iter()
                .map(|(name, status)| Check::new(*name, *status, format!("{name} detail")))
                .collect(),
        )
    }

    /// Runs one check through a sequence of statuses and returns the
    /// transitions each run produced.
    fn run(confirm_runs: u32, statuses: &[Status]) -> Vec<Vec<(Status, Status)>> {
        let mut transitions = Transitions::new(confirm_runs);
        statuses
            .iter()
            .map(|status| {
                transitions
                    .observe(&section(&[("Jellyfin", *status)]))
                    .into_iter()
                    .map(|t| (t.from, t.to))
                    .collect()
            })
            .collect()
    }

    use Status::{Critical as C, Ok as O, Warning as W};

    #[test]
    fn steady_ok_reports_nothing() {
        assert!(run(2, &[O, O, O]).iter().all(Vec::is_empty));
    }

    #[test]
    fn one_bad_run_is_ignored() {
        assert!(run(2, &[O, C, O, O]).iter().all(Vec::is_empty));
    }

    #[test]
    fn two_bad_runs_confirm_the_change() {
        let runs = run(2, &[O, C, C, C]);
        assert_eq!(runs[2], vec![(O, C)]);
        // Once confirmed it isn't reported again.
        assert!(runs[3].is_empty());
    }

    #[test]
    fn recovery_also_needs_confirming() {
        let runs = run(2, &[C, C, O, C, O, O]);
        assert_eq!(runs[1], vec![(O, C)]);
        // A single OK in the middle of an outage doesn't end it.
        assert!(runs[2].is_empty());
        assert_eq!(runs[5], vec![(C, O)]);
    }

    #[test]
    fn switching_candidates_restarts_the_count() {
        // CRIT then WARN: neither held for 2 runs yet.
        let runs = run(2, &[C, W, W]);
        assert!(runs[0].is_empty() && runs[1].is_empty());
        assert_eq!(runs[2], vec![(O, W)]);
    }

    #[test]
    fn escalation_is_reported() {
        let runs = run(1, &[W, C]);
        assert_eq!(runs[0], vec![(O, W)]);
        assert_eq!(runs[1], vec![(W, C)]);
    }

    #[test]
    fn confirm_runs_of_zero_acts_like_one() {
        assert_eq!(run(0, &[C])[0], vec![(O, C)]);
    }

    #[test]
    fn checks_are_tracked_independently() {
        let mut transitions = Transitions::new(1);
        let changes = transitions.observe(&section(&[("Jellyfin", C), ("Sonarr", O)]));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].check, "Jellyfin");
        assert_eq!(changes[0].detail, "Jellyfin detail");
        assert_eq!(transitions.status_of("services", "Sonarr"), Some(O));
        assert_eq!(transitions.status_of("services", "Radarr"), None);
    }

    #[test]
    fn same_check_name_in_two_collectors_is_separate() {
        let mut transitions = Transitions::new(1);
        let host = Section::new("host", vec![Check::new("CPU", C, "")]);
        let docker = Section::new("docker", vec![Check::new("CPU", O, "")]);
        assert_eq!(transitions.observe(&host).len(), 1);
        assert!(transitions.observe(&docker).is_empty());
    }

    #[test]
    fn problem_start_and_recovery_classification() {
        let t = |from, to| Transition {
            collector: String::new(),
            check: String::new(),
            from,
            to,
            detail: String::new(),
        };
        assert!(t(O, C).is_problem_start());
        assert!(!t(W, C).is_problem_start()); // escalation, same problem
        assert!(t(C, O).is_recovery());
        assert!(!t(C, W).is_recovery()); // better, but still a problem
        // Unknown isn't a problem: no page for missing temperature sensors.
        assert!(!t(O, Status::Unknown).is_problem_start());
    }
}
