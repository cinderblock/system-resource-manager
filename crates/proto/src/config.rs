//! Where things live, and the config file.
//!
//! Everything is under `~/.srm`, on every platform. `SRM_HOME` and `SRM_ENDPOINT`
//! move it, which is how the tests and a second local daemon run alongside a real
//! one.

use crate::model::{AssetKind, DEFAULT_POOL};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    match std::env::var_os("SRM_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::home_dir().unwrap_or_default().join(".srm"),
    }
}

pub fn config_path() -> PathBuf {
    home().join("config.toml")
}

/// Where `srmd` listens: a pipe name on Windows, a socket path elsewhere.
pub fn endpoint() -> String {
    if let Ok(endpoint) = std::env::var("SRM_ENDPOINT") {
        return endpoint;
    }
    if cfg!(windows) {
        r"\\.\pipe\srmd".to_string()
    } else {
        home().join("srmd.sock").to_string_lossy().into_owned()
    }
}

/// Leave two physical cores for the person using the machine. Anything measured is
/// better than this guess: set a number in the config.
pub fn default_cpu_capacity(physical_cores: u32) -> u32 {
    physical_cores.saturating_sub(2).max(2)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub node: NodeConfig,
    pub broker: BrokerConfig,
    /// Named pools. `cpu` always exists; declare it here to set its capacity.
    pub resources: BTreeMap<String, ResourceConfig>,
    pub network: NetworkConfig,
    #[serde(rename = "peer")]
    pub peers: Vec<PeerConfig>,
    #[serde(rename = "asset")]
    pub assets: Vec<AssetConfig>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct NodeConfig {
    /// Defaults to the hostname.
    pub name: Option<String>,
    pub site: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BrokerConfig {
    /// How long the head of a queue lets smaller requests pass it before it starts
    /// reserving slots for itself.
    pub backfill_grace_secs: u64,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            backfill_grace_secs: 30,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResourceConfig {
    pub capacity: u32,
}

/// The network face. Off unless asked for: the local broker needs none of this.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub enabled: bool,
    /// Bind the tailnet address, which brings encryption and identity with it.
    pub listen: String,
    /// Announce on, and browse, the local link.
    pub mdns: bool,
    /// How often to pull each peer and run each probe.
    pub poll_secs: u64,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:7654".to_string(),
            mdns: true,
            poll_secs: 10,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    /// `http://build-box.tailnet.example:7654`, or an HTTPS URL behind a
    /// reverse proxy.
    pub url: String,
    /// Defaults to this node's own token.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub basic_auth: Option<BasicAuth>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AssetConfig {
    pub id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub kind: AssetKind,
    pub site: Option<String>,
    pub probe: ProbeConfig,
}

/// The extension point for reaching things that can't run `srmd`. Every variant is
/// read-only.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProbeConfig {
    /// Does it accept a connection? Enough for a camera's RTSP port or a NAS's SMB.
    Tcp { host: String, port: u16 },
    /// Does it answer? Below 500 counts as up; authentication failures still prove
    /// the device is alive.
    Http { url: String },
    /// Scrape a `node_exporter` directly, for gauges with no install on the server.
    NodeExporter {
        url: String,
        #[serde(default)]
        basic_auth: Option<BasicAuth>,
    },
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    /// A missing file is a fine answer: every field has a default.
    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn pools(&self, physical_cores: u32) -> BTreeMap<String, u32> {
        let mut pools: BTreeMap<String, u32> = self
            .resources
            .iter()
            .map(|(name, r)| (name.clone(), r.capacity.max(1)))
            .collect();
        pools
            .entry(DEFAULT_POOL.to_string())
            .or_insert_with(|| default_cpu_capacity(physical_cores));
        pools
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_a_complete_config() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.network.enabled);
        assert_eq!(config.pools(6)["cpu"], 4);
    }

    #[test]
    fn the_cpu_default_never_drops_below_two() {
        assert_eq!(default_cpu_capacity(2), 2);
        assert_eq!(default_cpu_capacity(0), 2);
    }

    #[test]
    fn a_full_config_parses() {
        let config: Config = toml::from_str(
            r#"
            [node]
            site = "Workshop"

            [resources.cpu]
            capacity = 4
            [resources.camera-warehouse]
            capacity = 1

            [network]
            enabled = true
            listen = "100.64.0.1:7654"

            [[peer]]
            url = "http://build-box:7654"

            [[peer]]
            url = "https://server.example.com/srm"
            basic_auth = { username = "srm", password = "x" }

            [[asset]]
            id = "warehouse-cam"
            kind = "camera"
            probe = { type = "tcp", host = "10.0.0.5", port = 554 }

            [[asset]]
            id = "storage"
            kind = "server"
            probe = { type = "node_exporter", url = "https://server.example.com/metrics" }
            "#,
        )
        .unwrap();
        assert_eq!(config.pools(6)["camera-warehouse"], 1);
        assert_eq!(config.peers.len(), 2);
        assert_eq!(config.assets[0].kind, AssetKind::Camera);
    }

    #[test]
    fn a_typo_is_an_error_rather_than_a_silent_default() {
        assert!(toml::from_str::<Config>("[network]\nenable = true").is_err());
    }
}
