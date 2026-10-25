//! Container states from the Docker API, through the socket proxy.

use anyhow::{Context, Result};
use bollard::{API_DEFAULT_VERSION, Docker, query_parameters::ListContainersOptionsBuilder};

use super::Collector;
use crate::{
    config::DockerConfig,
    status::{Check, Status},
};

/// Seconds to wait for the Docker API before giving up.
const TIMEOUT_SECS: u64 = 5;

pub struct DockerCollector {
    docker: Docker,
}

impl DockerCollector {
    pub fn new(config: &DockerConfig) -> Result<Self> {
        // Connecting doesn't touch the network yet; it just builds a client.
        let docker = Docker::connect_with_http(&config.url, TIMEOUT_SECS, API_DEFAULT_VERSION)
            .with_context(|| format!("invalid Docker URL {}", config.url))?;
        Ok(Self { docker })
    }
}

/// Grades one container from its state ("running", "exited", ...) and
/// Docker's human status text ("Up 2 weeks (healthy)", "Exited (1) 3 hours ago").
fn grade(state: &str, status_text: &str) -> Status {
    match state {
        "running" if status_text.contains("(unhealthy)") => Status::Critical,
        "running" if status_text.contains("(health: starting)") => Status::Warning,
        "running" => Status::Ok,
        "restarting" | "paused" | "removing" => Status::Warning,
        "exited" | "dead" => Status::Critical,
        _ => Status::Unknown,
    }
}

impl Collector for DockerCollector {
    fn name(&self) -> &str {
        "containers"
    }

    async fn collect(&mut self) -> Result<Vec<Check>> {
        // `all(true)` includes stopped containers, so a crashed one shows up
        // as CRIT instead of silently disappearing from the list.
        let options = ListContainersOptionsBuilder::default().all(true).build();
        let containers = self
            .docker
            .list_containers(Some(options))
            .await
            .context("Docker API unreachable (is the SSH tunnel or proxy running?)")?;

        let mut checks: Vec<Check> = containers
            .into_iter()
            .map(|container| {
                // Docker names start with "/", e.g. "/jellyfin".
                let name = container
                    .names
                    .and_then(|names| names.into_iter().next())
                    .map(|n| n.trim_start_matches('/').to_string())
                    .unwrap_or_else(|| "(unnamed)".to_string());

                let state = container.state.map(|s| s.to_string()).unwrap_or_default();
                let status_text = container.status.unwrap_or_default();

                Check::new(name, grade(&state, &status_text), status_text)
            })
            .collect();

        // Alphabetical, so the list doesn't reshuffle between runs.
        checks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(checks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_is_ok() {
        assert_eq!(grade("running", "Up 2 weeks"), Status::Ok);
    }

    #[test]
    fn unhealthy_is_critical() {
        assert_eq!(grade("running", "Up 3 hours (unhealthy)"), Status::Critical);
    }

    #[test]
    fn health_starting_is_warning() {
        assert_eq!(
            grade("running", "Up 5 seconds (health: starting)"),
            Status::Warning
        );
    }

    #[test]
    fn exited_is_critical() {
        assert_eq!(grade("exited", "Exited (1) 3 hours ago"), Status::Critical);
    }

    #[test]
    fn restarting_is_warning() {
        assert_eq!(
            grade("restarting", "Restarting (1) 2 seconds ago"),
            Status::Warning
        );
    }

    #[test]
    fn unknown_state_is_unknown() {
        assert_eq!(grade("", ""), Status::Unknown);
    }
}
