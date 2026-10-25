//! CPU, memory, temperature and disk checks for the machine we run on.

use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use sysinfo::{Components, Disks, System};

use super::Collector;
use crate::status::{Check, Status};

// Thresholds are constants for now; they move into config.toml later.
const CPU_WARN: f64 = 80.0;
const CPU_CRIT: f64 = 95.0;
const MEM_WARN: f64 = 85.0;
const MEM_CRIT: f64 = 95.0;
const DISK_WARN: f64 = 85.0;
const DISK_CRIT: f64 = 95.0;
const TEMP_WARN: f64 = 80.0;
const TEMP_CRIT: f64 = 90.0;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// How long to wait between creating a `HostCollector` and the first
/// `collect`, so the CPU reading has two samples far enough apart.
pub const CPU_WARMUP: Duration = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL;

/// This machine's network name, e.g. "jaredburgoyne". Falls back to
/// "this machine" if the OS won't say.
pub fn host_name() -> String {
    System::host_name().unwrap_or_else(|| "this machine".to_string())
}

pub struct HostCollector {
    system: System,
    mounts: Vec<PathBuf>,
}

impl HostCollector {
    pub fn new(mounts: Vec<PathBuf>) -> Self {
        let mut system = System::new();
        // CPU usage is a difference between two readings, so take the
        // first one now and the second when `collect` runs.
        system.refresh_cpu_usage();
        Self { system, mounts }
    }

    fn cpu_check(&mut self) -> Check {
        self.system.refresh_cpu_usage();
        let usage = f64::from(self.system.global_cpu_usage());
        let status = Status::from_thresholds(usage, CPU_WARN, CPU_CRIT);
        Check::new("CPU", status, format!("{usage:.1}% used")).with_value(usage)
    }

    fn memory_check(&mut self) -> Check {
        self.system.refresh_memory();
        let total = self.system.total_memory() as f64;
        let used = self.system.used_memory() as f64;
        let percent = if total > 0.0 {
            used / total * 100.0
        } else {
            0.0
        };
        let status = Status::from_thresholds(percent, MEM_WARN, MEM_CRIT);
        Check::new(
            "Memory",
            status,
            format!(
                "{percent:.1}% used ({:.1} of {:.1} GiB)",
                used / GIB,
                total / GIB
            ),
        )
        .with_value(percent)
    }

    /// `None` when the machine has no temperature sensors (common on
    /// Windows), so the check is left out instead of showing UNKNOWN forever.
    fn temperature_check(&self) -> Option<Check> {
        let components = Components::new_with_refreshed_list();
        // `filter_map` keeps only the sensors that returned a reading.
        let hottest = components
            .iter()
            .filter_map(|c| c.temperature())
            .map(f64::from)
            .fold(None, |max: Option<f64>, t| {
                Some(max.map_or(t, |m| m.max(t)))
            });

        // `Option::map` runs the closure only when there's a value.
        hottest.map(|t| {
            Check::new(
                "Temperature",
                Status::from_thresholds(t, TEMP_WARN, TEMP_CRIT),
                format!("hottest sensor {t:.0} °C"),
            )
            .with_value(t)
        })
    }

    fn disk_checks(&self) -> Vec<Check> {
        let disks = Disks::new_with_refreshed_list();

        self.mounts
            .iter()
            .map(|mount| {
                let name = format!("Disk {}", mount.display());

                let Some(disk) = disks.iter().find(|d| d.mount_point() == mount.as_path()) else {
                    return Check::new(name, Status::Critical, "not mounted");
                };

                if disk.is_read_only() {
                    return Check::new(name, Status::Critical, "mounted read-only");
                }

                let total = disk.total_space() as f64;
                let free = disk.available_space() as f64;
                let used_percent = if total > 0.0 {
                    (total - free) / total * 100.0
                } else {
                    0.0
                };

                Check::new(
                    name,
                    Status::from_thresholds(used_percent, DISK_WARN, DISK_CRIT),
                    format!("{used_percent:.1}% used ({:.0} GiB free)", free / GIB),
                )
                .with_value(used_percent)
            })
            .collect()
    }
}

impl Collector for HostCollector {
    fn name(&self) -> &str {
        "host"
    }

    // sysinfo calls are fast and synchronous, so this async fn never
    // actually awaits anything. That's fine: it still fits the trait.
    async fn collect(&mut self) -> Result<Vec<Check>> {
        let mut checks = vec![self.cpu_check(), self.memory_check()];
        // `extend` with an Option adds the check only if it's `Some`.
        checks.extend(self.temperature_check());
        checks.extend(self.disk_checks());
        Ok(checks)
    }
}
