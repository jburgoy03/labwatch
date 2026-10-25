//! Loading and validating `config.toml`.

use std::{fs, net::IpAddr, path::Path, path::PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// The whole config file. Each field maps to a `[section]` in the TOML.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `[server]` is optional; missing means "use the defaults".
    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub host: HostConfig,

    #[serde(default)]
    pub intervals: IntervalConfig,

    #[serde(default)]
    pub storage: StorageConfig,

    /// `[vpn]` is truly optional: `None` turns the VPN check off.
    pub vpn: Option<VpnConfig>,

    /// `[docker]` is optional too: `None` turns container checks off.
    pub docker: Option<DockerConfig>,

    /// Each `[[services]]` block becomes one entry.
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Address the web server listens on, e.g. "0.0.0.0:3000".
    pub bind: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:3000".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    /// Mount points to report disk usage and health for.
    pub mounts: Vec<PathBuf>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            mounts: vec![PathBuf::from("/")],
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// SQLite database file for history. Created if it doesn't exist.
    pub path: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("labwatch.db"),
        }
    }
}

/// How often each collector runs, in seconds.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntervalConfig {
    pub host_secs: u64,
    pub services_secs: u64,
    pub vpn_secs: u64,
    pub docker_secs: u64,
}

impl Default for IntervalConfig {
    fn default() -> Self {
        Self {
            host_secs: 5,
            services_secs: 30,
            vpn_secs: 60,
            docker_secs: 15,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VpnConfig {
    /// Gluetun's control server, e.g. "http://192.168.1.10:8000".
    pub gluetun_url: String,

    /// The exit IP you expect. Serde parses this into an `IpAddr`,
    /// so a typo like "1.2.3" fails when the config loads.
    pub expected_ip: Option<IpAddr>,

    /// API key from Gluetun's auth config (`/gluetun/auth/config.toml`).
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerConfig {
    /// The socket proxy, e.g. "http://127.0.0.1:2375".
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub url: String,

    /// Seconds to wait before marking the service down.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    5
}

impl Config {
    /// Reads and parses a config file from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;

        Self::parse(&text).with_context(|| format!("invalid config file {}", path.display()))
    }

    /// Parses config from a TOML string. Split out from `load` so tests
    /// don't need files on disk.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_uses_defaults() {
        let config = Config::parse("").unwrap();

        assert_eq!(config.server.bind, "127.0.0.1:3000");
        assert_eq!(config.host.mounts, vec![PathBuf::from("/")]);
        assert_eq!(config.intervals.services_secs, 30);
        assert!(config.vpn.is_none());
        assert!(config.services.is_empty());
    }

    #[test]
    fn parses_full_config() {
        let text = r#"
            [server]
            bind = "0.0.0.0:3000"

            [host]
            mounts = ["/", "/mnt/media"]

            [vpn]
            gluetun_url = "http://192.168.1.10:8000"
            expected_ip = "203.0.113.7"

            [[services]]
            name = "Jellyfin"
            url = "http://192.168.1.10:8096/health"

            [[services]]
            name = "Sonarr"
            url = "http://192.168.1.10:8989"
            timeout_secs = 10
        "#;

        let config = Config::parse(text).unwrap();

        assert_eq!(config.server.bind, "0.0.0.0:3000");
        assert_eq!(config.host.mounts.len(), 2);

        let vpn = config.vpn.unwrap();
        assert_eq!(vpn.expected_ip, Some("203.0.113.7".parse().unwrap()));

        assert_eq!(config.services.len(), 2);
        assert_eq!(config.services[0].timeout_secs, 5);
        assert_eq!(config.services[1].timeout_secs, 10);
    }

    #[test]
    fn rejects_unknown_fields() {
        let result = Config::parse("[server]\nbnd = \"0.0.0.0:3000\"");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_invalid_ip() {
        let text = "[vpn]\ngluetun_url = \"http://x:8000\"\nexpected_ip = \"1.2.3\"";
        assert!(Config::parse(text).is_err());
    }
}
