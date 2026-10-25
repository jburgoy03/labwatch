//! Checks Gluetun's public IP through its control server.

use std::{net::IpAddr, time::Duration};

use anyhow::Result;
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use super::Collector;
use crate::{
    config::VpnConfig,
    status::{Check, Status},
};

const CHECK_NAME: &str = "VPN exit IP";

/// The parts of Gluetun's `/v1/publicip/ip` response we use. Fields we
/// don't list are ignored, and `#[serde(default)]` turns missing ones
/// into empty strings instead of errors.
#[derive(Debug, Deserialize)]
struct PublicIpResponse {
    #[serde(default)]
    public_ip: String,
    #[serde(default)]
    city: String,
    #[serde(default)]
    country: String,
}

pub struct VpnCollector {
    client: Client,
    config: VpnConfig,
}

impl VpnCollector {
    pub fn new(config: VpnConfig) -> Result<Self> {
        let client = Client::builder().timeout(Duration::from_secs(5)).build()?;
        Ok(Self { client, config })
    }

    async fn check(&self) -> Check {
        let url = format!(
            "{}/v1/publicip/ip",
            self.config.gluetun_url.trim_end_matches('/')
        );

        let mut request = self.client.get(&url);
        if let Some(key) = &self.config.api_key {
            request = request.header("X-API-Key", key);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(err) if err.is_timeout() => {
                return Check::new(CHECK_NAME, Status::Critical, "control server timed out");
            }
            Err(_) => {
                return Check::new(CHECK_NAME, Status::Critical, "control server unreachable");
            }
        };

        if response.status() == StatusCode::UNAUTHORIZED {
            return Check::new(CHECK_NAME, Status::Critical, "API key rejected");
        }

        match response.json::<PublicIpResponse>().await {
            Ok(body) => evaluate(&body, self.config.expected_ip),
            Err(err) => Check::new(
                CHECK_NAME,
                Status::Critical,
                format!("unexpected response: {err}"),
            ),
        }
    }
}

/// Grades the response. A pure function so the logic is testable.
fn evaluate(body: &PublicIpResponse, expected: Option<IpAddr>) -> Check {
    // `.parse::<IpAddr>()` returns a Result; `.ok()` turns it into an Option.
    let Some(actual) = body.public_ip.parse::<IpAddr>().ok() else {
        return Check::new(CHECK_NAME, Status::Critical, "no public IP (tunnel down?)");
    };

    let place = match (body.city.is_empty(), body.country.is_empty()) {
        (false, false) => format!(" ({}, {})", body.city, body.country),
        (true, false) => format!(" ({})", body.country),
        _ => String::new(),
    };

    match expected {
        Some(expected) if expected != actual => Check::new(
            CHECK_NAME,
            Status::Warning,
            format!("{actual}{place}, expected {expected}"),
        ),
        _ => Check::new(CHECK_NAME, Status::Ok, format!("{actual}{place}")),
    }
}

impl Collector for VpnCollector {
    fn name(&self) -> &str {
        "vpn"
    }

    async fn collect(&mut self) -> Result<Vec<Check>> {
        Ok(vec![self.check().await])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(ip: &str) -> PublicIpResponse {
        PublicIpResponse {
            public_ip: ip.to_string(),
            city: "Chicago".to_string(),
            country: "United States".to_string(),
        }
    }

    #[test]
    fn matching_ip_is_ok() {
        let check = evaluate(&response("203.0.113.7"), "203.0.113.7".parse().ok());
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("Chicago"));
    }

    #[test]
    fn changed_ip_is_warning() {
        let check = evaluate(&response("198.51.100.9"), "203.0.113.7".parse().ok());
        assert_eq!(check.status, Status::Warning);
    }

    #[test]
    fn no_expected_ip_is_ok() {
        let check = evaluate(&response("203.0.113.7"), None);
        assert_eq!(check.status, Status::Ok);
    }

    #[test]
    fn empty_ip_is_critical() {
        let check = evaluate(&response(""), None);
        assert_eq!(check.status, Status::Critical);
    }

    #[test]
    fn parses_gluetun_json() {
        // Real response shape, including fields we don't use.
        let json = r#"{"public_ip":"203.0.113.7","region":"Illinois","country":"United States","city":"Chicago"}"#;
        let body: PublicIpResponse = serde_json::from_str(json).unwrap();
        assert_eq!(body.public_ip, "203.0.113.7");
        assert_eq!(body.city, "Chicago");
    }
}
