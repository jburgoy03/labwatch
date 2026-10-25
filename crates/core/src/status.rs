//! The shared result types every collector reports.

use std::fmt;

/// Health of one check.
///
/// The variants are declared from best to worst, and `PartialOrd`/`Ord`
/// derive their ordering from that order. So `Status::Ok < Status::Critical`,
/// and the worst of a list is just `.max()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Ok,
    /// We couldn't tell, e.g. no temperature sensors on this machine.
    Unknown,
    Warning,
    Critical,
}

impl Status {
    /// Grades a value against warning and critical thresholds.
    pub fn from_thresholds(value: f64, warn: f64, crit: f64) -> Self {
        if value >= crit {
            Status::Critical
        } else if value >= warn {
            Status::Warning
        } else {
            Status::Ok
        }
    }
}

/// `Display` is Rust's `ToString()` override: it controls `{}` formatting.
impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Status::Ok => "OK",
            Status::Unknown => "UNKNOWN",
            Status::Warning => "WARN",
            Status::Critical => "CRIT",
        };
        // `pad` (not `write_str`) so width specifiers like `{:<5}` work.
        f.pad(label)
    }
}

/// One line on the dashboard: a name, its status and a human-readable detail.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    pub fn new(name: impl Into<String>, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status,
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_grade_values() {
        assert_eq!(Status::from_thresholds(50.0, 80.0, 95.0), Status::Ok);
        assert_eq!(Status::from_thresholds(80.0, 80.0, 95.0), Status::Warning);
        assert_eq!(Status::from_thresholds(99.0, 80.0, 95.0), Status::Critical);
    }

    #[test]
    fn worst_status_is_the_max() {
        let statuses = [Status::Ok, Status::Critical, Status::Warning];
        assert_eq!(statuses.iter().max(), Some(&Status::Critical));
    }
}
