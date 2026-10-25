//! Data sources. Each one implements the `Collector` trait.

mod host;
mod services;
mod vpn;

pub use host::{CPU_WARMUP, HostCollector};
pub use services::ServiceCollector;
pub use vpn::VpnCollector;

use std::future::Future;

use anyhow::Result;

use crate::status::Check;

/// Anything that can produce a list of checks.
///
/// `collect` returns a future instead of being a plain `async fn` so we can
/// promise it's `Send`: safe to move to another thread. tokio needs that to
/// run collectors as background tasks. Implementations can still just
/// write `async fn collect(&mut self)`.
pub trait Collector: Send {
    /// Short name shown as the section heading, e.g. "host".
    fn name(&self) -> &str;

    /// Runs every check this collector owns.
    fn collect(&mut self) -> impl Future<Output = Result<Vec<Check>>> + Send;
}
