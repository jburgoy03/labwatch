//! HTTP health checks for the services listed in config.

use std::time::{Duration, Instant};

use anyhow::Result;
use reqwest::{Client, StatusCode};
use tokio::task::JoinSet;

use super::Collector;
use crate::{
    config::ServiceConfig,
    status::{Check, Status},
};

/// Responses slower than this are flagged as a warning.
const SLOW_MS: u128 = 1000;

pub struct ServiceCollector {
    client: Client,
    services: Vec<ServiceConfig>,
}

impl ServiceCollector {
    pub fn new(services: Vec<ServiceConfig>) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("labwatch/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client, services })
    }
}

/// Checks one service. Takes owned values (not `&self`) so it can run as
/// its own tokio task: a spawned task may outlive the caller, so the
/// borrow checker won't let it hold references.
async fn check(client: Client, service: ServiceConfig) -> Check {
    let started = Instant::now();
    let result = client
        .get(&service.url)
        .timeout(Duration::from_secs(service.timeout_secs))
        .send()
        .await;

    let elapsed_ms = started.elapsed().as_millis();

    // `match` with guards (`if ...`) picks the first arm that fits.
    let (status, detail) = match &result {
        Ok(response) => classify(response.status(), elapsed_ms),
        Err(err) if err.is_timeout() => (
            Status::Critical,
            format!("timed out after {}s", service.timeout_secs),
        ),
        Err(err) if err.is_connect() => (Status::Critical, "unreachable".to_string()),
        Err(err) => (Status::Critical, err.to_string()),
    };

    let check = Check::new(service.name, status, detail);

    // Only a real response has a meaningful response time to chart.
    if result.is_ok() {
        check.with_value(elapsed_ms as f64)
    } else {
        check
    }
}

/// Turns an HTTP response into a status. Pulled out of `check` so it can
/// be unit tested without a real server.
fn classify(code: StatusCode, elapsed_ms: u128) -> (Status, String) {
    let timing = format!("{elapsed_ms} ms");

    if code.is_server_error() {
        return (Status::Critical, format!("HTTP {}", code.as_u16()));
    }

    // 401/403 still prove the service is up; it just wants a login or API key.
    let auth_required = code == StatusCode::UNAUTHORIZED || code == StatusCode::FORBIDDEN;

    if code.is_client_error() && !auth_required {
        return (Status::Warning, format!("HTTP {}, {timing}", code.as_u16()));
    }

    let status = if elapsed_ms > SLOW_MS {
        Status::Warning
    } else {
        Status::Ok
    };

    let detail = if auth_required {
        format!("up (auth required), {timing}")
    } else {
        format!("up, {timing}")
    };

    (status, detail)
}

impl Collector for ServiceCollector {
    fn name(&self) -> &str {
        "services"
    }

    /// Checks every service at the same time, so total time is the slowest
    /// service rather than the sum of all of them.
    async fn collect(&mut self) -> Result<Vec<Check>> {
        let mut tasks = JoinSet::new();

        for (index, service) in self.services.iter().enumerate() {
            // Cloning a reqwest Client is cheap: it's a handle to a shared
            // connection pool, like copying a reference.
            let client = self.client.clone();
            let service = service.clone();
            // `async move` moves `client` and `service` into the task.
            tasks.spawn(async move { (index, check(client, service).await) });
        }

        // Tasks finish in any order; collect then sort back to config order.
        let mut results = tasks.join_all().await;
        results.sort_by_key(|(index, _)| *index);

        Ok(results.into_iter().map(|(_, check)| check).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_success_is_ok() {
        let (status, _) = classify(StatusCode::OK, 40);
        assert_eq!(status, Status::Ok);
    }

    #[test]
    fn slow_success_is_warning() {
        let (status, _) = classify(StatusCode::OK, 2500);
        assert_eq!(status, Status::Warning);
    }

    #[test]
    fn auth_required_counts_as_up() {
        let (status, detail) = classify(StatusCode::UNAUTHORIZED, 40);
        assert_eq!(status, Status::Ok);
        assert!(detail.contains("auth required"));
    }

    #[test]
    fn not_found_is_warning() {
        let (status, _) = classify(StatusCode::NOT_FOUND, 40);
        assert_eq!(status, Status::Warning);
    }

    #[test]
    fn server_error_is_critical() {
        let (status, _) = classify(StatusCode::BAD_GATEWAY, 40);
        assert_eq!(status, Status::Critical);
    }
}
