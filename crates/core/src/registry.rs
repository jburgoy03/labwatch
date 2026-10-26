//! The list of collectors the program runs, built in one place.
//!
//! Before this, `serve` and `snapshot` each built every collector by hand,
//! listed their names for the hub and numbered their slots. Each new
//! integration meant editing both. Now the caller adds collectors to a
//! `Registry` once, and the registry hands out names and slots itself.
//!
//! The awkward part is storing different collector types in one `Vec`.
//! In C# you'd write `List<ICollector>`. Rust's equivalent is
//! `Vec<Box<dyn Collector>>`, but `Collector` can't be used behind `dyn`:
//! its `collect` returns `impl Future`, a different concrete type for every
//! collector, and a trait object needs one fixed signature for its vtable.
//! `AnyCollector` below bridges the two.

use std::{future::Future, pin::Pin, time::Duration};

use anyhow::Result;
use tokio::task::{JoinHandle, JoinSet};

use crate::{
    collectors::Collector,
    hub::Hub,
    scheduler,
    status::{Check, Section},
};

/// A boxed future: "some future that yields this, allocated on the heap".
/// Boxing gives every collector's future the same type, which is what a
/// trait object needs. It's close to how every C# `async` method returns
/// the same `Task<T>` whatever its body does.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The object-safe twin of `Collector`: same methods, but `collect` returns
/// a boxed future, so `dyn DynCollector` is allowed. Private, because
/// nothing outside this module needs to know it exists.
///
/// Its methods have their own names (`dyn_name`, not `name`) because
/// `AnyCollector` ends up implementing both traits, and identical names
/// would make every `.name()` call ambiguous.
trait DynCollector: Send {
    fn dyn_name(&self) -> &str;
    fn collect_boxed(&mut self) -> BoxFuture<'_, Result<Vec<Check>>>;
}

/// A blanket impl: every type that implements `Collector` automatically
/// implements `DynCollector` too. C# can't express this directly; the
/// nearest is an adapter class wrapping any `ICollector`, except here the
/// compiler writes the adapter for each type.
impl<C: Collector> DynCollector for C {
    fn dyn_name(&self) -> &str {
        Collector::name(self)
    }

    fn collect_boxed(&mut self) -> BoxFuture<'_, Result<Vec<Check>>> {
        Box::pin(self.collect())
    }
}

/// Any collector, with its concrete type erased. Because it implements
/// `Collector` itself, it works anywhere a collector does, including
/// `scheduler::spawn`.
pub struct AnyCollector(Box<dyn DynCollector>);

impl AnyCollector {
    /// `'static`: the collector owns all its data, so it can live in a
    /// background task for the life of the program.
    pub fn new(collector: impl Collector + 'static) -> Self {
        Self(Box::new(collector))
    }
}

impl Collector for AnyCollector {
    fn name(&self) -> &str {
        self.0.dyn_name()
    }

    async fn collect(&mut self) -> Result<Vec<Check>> {
        self.0.collect_boxed().await
    }
}

/// One registered collector and how often it runs.
struct Entry {
    collector: AnyCollector,
    every: Duration,
}

/// Every collector the program runs, in display order. A collector's
/// position in the list is its slot in the hub.
#[derive(Default)]
pub struct Registry {
    entries: Vec<Entry>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a collector that runs every `every`. Returns `&mut Self` so
    /// calls can chain, like a C# fluent builder.
    pub fn add(&mut self, collector: impl Collector + 'static, every: Duration) -> &mut Self {
        self.entries.push(Entry {
            collector: AnyCollector::new(collector),
            every,
        });
        self
    }

    /// Adds a collector only if it's configured. Most integrations are
    /// optional config sections, so this saves an `if let` per call site.
    pub fn add_optional(
        &mut self,
        collector: Option<impl Collector + 'static>,
        every: Duration,
    ) -> &mut Self {
        if let Some(collector) = collector {
            self.add(collector, every);
        }
        self
    }

    /// Collector names in display order, for `Hub::new`.
    pub fn names(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.collector.name().to_string())
            .collect()
    }

    /// Starts every collector on its own interval, publishing to the slot
    /// that matches its position, so the slots always agree with `names`.
    ///
    /// Takes `self` by value: the collectors move into their tasks, and
    /// the registry is used up. The compiler then stops anyone starting the
    /// same collectors twice.
    pub fn spawn_all(self, hub: &Hub) -> Vec<JoinHandle<()>> {
        self.entries
            .into_iter()
            .enumerate()
            .map(|(slot, entry)| scheduler::spawn(entry.collector, entry.every, slot, hub.clone()))
            .collect()
    }

    /// Runs every collector once, all at the same time, and returns their
    /// sections in display order. Stops at the first collector error.
    pub async fn collect_once(self) -> Result<Vec<Section>> {
        let mut tasks = JoinSet::new();

        for (index, entry) in self.entries.into_iter().enumerate() {
            let mut collector = entry.collector;
            tasks.spawn(async move {
                let checks = collector.collect().await?;
                // `anyhow::Ok` pins the error type for the `?` above.
                anyhow::Ok((index, Section::new(collector.name(), checks)))
            });
        }

        // `join_all` waits for every task. Each yields `Result<(usize, Section)>`,
        // and collecting into `Result<Vec<_>>` stops at the first `Err`.
        let mut results = tasks
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        // Tasks finish in any order; sort back into display order.
        results.sort_by_key(|(index, _)| *index);
        Ok(results.into_iter().map(|(_, section)| section).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Status;

    /// A stand-in collector with a fixed name and result.
    struct Fake {
        name: &'static str,
        fail: bool,
    }

    fn fake(name: &'static str) -> Fake {
        Fake { name, fail: false }
    }

    impl Collector for Fake {
        fn name(&self) -> &str {
            self.name
        }

        async fn collect(&mut self) -> Result<Vec<Check>> {
            if self.fail {
                anyhow::bail!("{} is broken", self.name);
            }
            Ok(vec![Check::new("check", Status::Ok, self.name)])
        }
    }

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn names_follow_the_order_collectors_were_added() {
        let mut registry = Registry::new();
        registry
            .add(fake("host"), SECOND)
            .add(fake("services"), SECOND);

        assert_eq!(registry.names(), ["host", "services"]);
    }

    #[test]
    fn unconfigured_optional_collectors_are_skipped() {
        let mut registry = Registry::new();
        registry
            .add(fake("host"), SECOND)
            .add_optional(None::<Fake>, SECOND)
            .add_optional(Some(fake("vpn")), SECOND);

        assert_eq!(registry.names(), ["host", "vpn"]);
    }

    #[tokio::test]
    async fn collect_once_returns_sections_in_display_order() {
        let mut registry = Registry::new();
        registry
            .add(fake("host"), SECOND)
            .add(fake("services"), SECOND)
            .add(fake("vpn"), SECOND);

        let sections = registry.collect_once().await.unwrap();
        let names: Vec<&str> = sections.iter().map(|s| s.collector.as_str()).collect();

        assert_eq!(names, ["host", "services", "vpn"]);
    }

    #[tokio::test]
    async fn collect_once_reports_a_failing_collector() {
        let mut registry = Registry::new();
        registry.add(fake("host"), SECOND).add(
            Fake {
                name: "vpn",
                fail: true,
            },
            SECOND,
        );

        let err = registry.collect_once().await.unwrap_err();
        assert!(err.to_string().contains("vpn is broken"));
    }

    #[tokio::test]
    async fn spawned_collectors_publish_to_their_own_slots() {
        let mut registry = Registry::new();
        registry.add(fake("host"), SECOND).add(fake("vpn"), SECOND);

        let hub = Hub::new(registry.names());
        let mut updates = hub.subscribe();
        let tasks = registry.spawn_all(&hub);

        // Both collectors run once after the CPU warmup. Wait for both.
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(5), updates.recv())
                .await
                .expect("collector didn't report in time")
                .unwrap();
        }

        // Each section landed in the slot matching its name.
        let sections = hub.sections();
        assert_eq!(sections[0].checks[0].detail, "host");
        assert_eq!(sections[1].checks[0].detail, "vpn");

        tasks.iter().for_each(JoinHandle::abort);
    }
}
