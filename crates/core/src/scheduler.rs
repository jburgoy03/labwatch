//! Runs each collector forever on its own interval as a background task.

use std::time::Duration;

use tokio::{
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval_at},
};

use crate::{
    collectors::{CPU_WARMUP, Collector},
    hub::Hub,
    status::{Check, Section, Status},
};

/// Starts a task that runs `collector` every `every` and publishes its
/// result to `slot` of the hub.
///
/// The bounds explain themselves:
/// - `C: Collector` it must be a collector,
/// - `'static` it owns all its data (no borrowed references), because the
///   task may run for the life of the program.
///
/// `Send` comes from the `Collector` trait itself.
pub fn spawn<C>(mut collector: C, every: Duration, slot: usize, hub: Hub) -> JoinHandle<()>
where
    C: Collector + 'static,
{
    tokio::spawn(async move {
        // Start after the CPU warmup so the first host reading is accurate.
        let mut ticker = interval_at(Instant::now() + CPU_WARMUP, every);
        // If a run takes longer than the interval, don't fire a burst of
        // catch-up runs afterward.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;

            let name = collector.name().to_string();
            let section = match collector.collect().await {
                Ok(checks) => Section::new(&name, checks),
                // A failing collector shows up on the dashboard instead of
                // killing the task. `{err:#}` prints the whole context chain.
                Err(err) => Section::new(
                    &name,
                    vec![Check::new(
                        "collector",
                        Status::Critical,
                        format!("{err:#}"),
                    )],
                ),
            };

            hub.publish(slot, section);
        }
    })
}
