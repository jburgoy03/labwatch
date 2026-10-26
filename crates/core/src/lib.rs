//! Core library for Labwatch: collectors, data models and storage.

pub mod collectors;
pub mod config;
pub mod history;
pub mod hub;
pub mod incidents;
pub mod scheduler;
pub mod status;
pub mod store;
pub mod transitions;

/// Returns the Labwatch version from Cargo.toml.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!version().is_empty());
    }
}
