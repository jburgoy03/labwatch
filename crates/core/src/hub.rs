//! The hub ties the store (latest results) to a broadcast channel
//! (notifications that a result changed).

use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

use crate::{status::Section, store::Store};

/// How many unread updates a slow subscriber can fall behind by before it
/// starts missing old ones. Plenty for a handful of collectors.
const CHANNEL_CAPACITY: usize = 64;

/// Cheap to clone: both fields are handles to shared data, so every clone
/// talks to the same store and the same channel.
///
/// How the store is shared across threads:
/// - `Arc` (atomically reference-counted) lets many tasks own the same store;
///   it's freed when the last owner drops it.
/// - `RwLock` allows many readers or one writer at a time.
///
/// Rust won't compile code that shares mutable data across threads without
/// something like this. In C# you could share a field and hope; here the
/// type system makes you choose.
#[derive(Clone)]
pub struct Hub {
    store: Arc<RwLock<Store>>,
    updates: broadcast::Sender<Section>,
}

impl Hub {
    pub fn new(slots: usize) -> Self {
        // `channel` returns a sender and a first receiver. We drop the
        // receiver (`_`): subscribers make their own with `subscribe()`.
        let (updates, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            store: Arc::new(RwLock::new(Store::with_slots(slots))),
            updates,
        }
    }

    /// Saves a collector's latest result and tells every subscriber.
    pub fn publish(&self, slot: usize, section: Section) {
        // The write lock is held only for this statement: the guard that
        // `write()` returns unlocks automatically when it's dropped.
        self.store
            .write()
            .expect("store lock poisoned")
            .set(slot, section.clone());

        // `send` only fails when nobody is subscribed, which is fine:
        // there's just no one to tell. `let _ =` says "ignore on purpose".
        let _ = self.updates.send(section);
    }

    /// The latest result from every collector that has reported.
    pub fn sections(&self) -> Vec<Section> {
        self.store.read().expect("store lock poisoned").sections()
    }

    /// A new receiver that gets every section published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Section> {
        self.updates.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_updates_store_and_notifies_subscribers() {
        let hub = Hub::new(1);
        let mut first = hub.subscribe();
        let mut second = hub.subscribe();

        hub.publish(0, Section::new("host", vec![]));

        assert_eq!(hub.sections().len(), 1);
        // Every subscriber gets its own copy.
        assert_eq!(first.try_recv().unwrap().collector, "host");
        assert_eq!(second.try_recv().unwrap().collector, "host");
    }

    #[test]
    fn publish_without_subscribers_still_updates_store() {
        let hub = Hub::new(1);
        hub.publish(0, Section::new("host", vec![]));
        assert_eq!(hub.sections().len(), 1);
    }

    #[test]
    fn late_subscriber_misses_earlier_updates() {
        let hub = Hub::new(1);
        hub.publish(0, Section::new("early", vec![]));

        let mut late = hub.subscribe();
        assert!(late.try_recv().is_err());
    }
}
