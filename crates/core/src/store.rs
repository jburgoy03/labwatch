//! The latest results from every collector, shared between the background
//! collector tasks (writers) and the web server (readers).

use std::sync::{Arc, RwLock};

use crate::status::Section;

/// One slot per collector, in display order. A slot is `None` until that
/// collector finishes its first run.
#[derive(Debug, Default)]
pub struct Store {
    slots: Vec<Option<Section>>,
}

/// How the store is shared across threads:
/// - `Arc` (atomically reference-counted) lets many tasks own the same store;
///   it's freed when the last owner drops it.
/// - `RwLock` allows many readers or one writer at a time.
///
/// Rust won't compile code that shares mutable data across threads without
/// something like this. In C# you could share a field and hope; here the
/// type system makes you choose.
pub type SharedStore = Arc<RwLock<Store>>;

impl Store {
    pub fn with_slots(count: usize) -> Self {
        Self {
            slots: vec![None; count],
        }
    }

    pub fn set(&mut self, slot: usize, section: Section) {
        if let Some(entry) = self.slots.get_mut(slot) {
            *entry = Some(section);
        }
    }

    /// Every section that has reported at least once, in slot order.
    pub fn sections(&self) -> Vec<Section> {
        // `flatten` skips the `None`s and unwraps the `Some`s.
        self.slots.iter().flatten().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_keep_slot_order_and_skip_empty() {
        let mut store = Store::with_slots(3);
        store.set(2, Section::new("vpn", vec![]));
        store.set(0, Section::new("host", vec![]));

        let names: Vec<String> = store.sections().into_iter().map(|s| s.collector).collect();
        assert_eq!(names, ["host", "vpn"]);
    }

    #[test]
    fn set_replaces_previous_result() {
        let mut store = Store::with_slots(1);
        store.set(0, Section::new("first", vec![]));
        store.set(0, Section::new("second", vec![]));

        assert_eq!(store.sections()[0].collector, "second");
    }

    #[test]
    fn out_of_range_slot_is_ignored() {
        let mut store = Store::with_slots(1);
        store.set(5, Section::new("nope", vec![]));
        assert!(store.sections().is_empty());
    }
}
