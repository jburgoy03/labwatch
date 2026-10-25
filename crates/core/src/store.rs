//! The latest result from every collector. Shared through `Hub`.

use crate::status::Section;

/// One section per collector, in display order. Each starts as a pending
/// placeholder and is replaced when that collector finishes its first run,
/// so every collector has a card on the dashboard from the start.
#[derive(Debug, Default)]
pub struct Store {
    sections: Vec<Section>,
}

impl Store {
    pub fn new(collectors: Vec<String>) -> Self {
        Self {
            sections: collectors.into_iter().map(Section::pending).collect(),
        }
    }

    pub fn set(&mut self, slot: usize, section: Section) {
        if let Some(entry) = self.sections.get_mut(slot) {
            *entry = section;
        }
    }

    /// Every collector's latest section, in display order.
    pub fn sections(&self) -> Vec<Section> {
        self.sections.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Status;

    fn store() -> Store {
        Store::new(vec!["host".into(), "vpn".into()])
    }

    #[test]
    fn starts_with_pending_sections_in_order() {
        let sections = store().sections();
        let names: Vec<&str> = sections.iter().map(|s| s.collector.as_str()).collect();

        assert_eq!(names, ["host", "vpn"]);
        assert!(sections.iter().all(|s| s.status == Status::Unknown));
    }

    #[test]
    fn set_replaces_a_slot() {
        let mut store = store();
        store.set(1, Section::new("vpn", vec![]));
        assert_eq!(store.sections()[1].status, Status::Ok);
    }

    #[test]
    fn out_of_range_slot_is_ignored() {
        let mut store = store();
        store.set(5, Section::new("nope", vec![]));
        assert_eq!(store.sections().len(), 2);
    }
}
