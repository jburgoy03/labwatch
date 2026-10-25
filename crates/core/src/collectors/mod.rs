//! Data sources. Each one implements the `Collector` trait.

mod host;
mod services;
mod vpn;

pub use host::{CPU_WARMUP, HostCollector};
pub use services::ServiceCollector;
pub use vpn::VpnCollector;

use anyhow::Result;

use crate::status::Check;

/// Anything that can produce a list of checks.
///
/// This is Rust's version of an interface. `&mut self` because collectors
/// keep state between runs (sysinfo needs two readings to compute CPU usage).
/// In Phase 2 this becomes async so slow network checks don't block.
pub trait Collector {
    /// Short name shown as the section heading, e.g. "host".
    fn name(&self) -> &str;

    /// Runs every check this collector owns.
    fn collect(&mut self) -> Result<Vec<Check>>;
}
