//! The data model every client sees, whether it is reading the local pipe or a
//! remote node's HTTP face.
//!
//! The nouns:
//!
//! - A **Node** is a machine running `srmd`.
//! - A **Pool** is a named thing with a capacity that can be leased. `cpu` is the
//!   default one; a capacity of 1 makes any other pool an exclusive lock.
//! - A **Lease** is a claim on a pool, for as long as a connection stays open.
//! - An **Asset** is anything on the network that can't run `srmd` itself, seen
//!   through a probe.
//!
//! Timestamps are milliseconds since the Unix epoch. Clients decide how to show
//! them.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The pool used when a request doesn't name one.
pub const DEFAULT_POOL: &str = "cpu";

pub type LeaseId = u64;
pub type Millis = i64;

pub fn now_ms() -> Millis {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        .unwrap_or(0)
}

/// What a client asks for. Everything past `amount` is there so a person looking at
/// the queue can tell who is asking and why.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LeaseRequest {
    #[serde(default = "default_pool")]
    pub pool: String,
    pub amount: u32,
    pub label: String,
    /// The process that holds the lease. Its tree is what gets accounted when the
    /// client can't report an exact process set.
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// `CLAUDE_CODE_SESSION_ID`, when the claimant is an agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default)]
    pub low: bool,
}

fn default_pool() -> String {
    DEFAULT_POOL.to_string()
}

/// A lease as shown to clients: either waiting or held.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    pub id: LeaseId,
    pub pool: String,
    pub amount: u32,
    pub label: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default)]
    pub low: bool,
    pub requested_at: Millis,
    /// `None` while queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<Millis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// What a lease's processes are actually spending.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    /// 1.0 is one logical core, fully busy.
    pub cores: f32,
    pub peak_cores: f32,
    pub rss_bytes: u64,
    pub peak_rss_bytes: u64,
    pub processes: u32,
    pub cpu_seconds: f64,
    /// Busiest first, capped.
    #[serde(default)]
    pub top: Vec<ProcessSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProcessSummary {
    pub pid: u32,
    /// A useful name: `rustc srm_proto` rather than `rustc.exe`.
    pub name: String,
    pub cores: f32,
    pub rss_bytes: u64,
}

/// Heavy work running outside any lease.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProcessGroup {
    pub root_pid: u32,
    pub name: String,
    pub cores: f32,
    pub rss_bytes: u64,
    pub processes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Pool {
    pub name: String,
    pub capacity: u32,
    pub used: u32,
    pub holders: Vec<Lease>,
    /// In queue order.
    pub waiters: Vec<Lease>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SystemStats {
    /// 0–100, across all logical cores.
    pub cpu_percent: f32,
    pub cores_physical: u32,
    pub cores_logical: u32,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeInfo {
    /// Stable across restarts and renames. Generated on first run.
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    pub os: String,
    pub version: String,
    pub started_at: Millis,
}

/// One node's view of itself.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeSnapshot {
    pub node: NodeInfo,
    pub taken_at: Millis,
    pub system: SystemStats,
    pub pools: Vec<Pool>,
    pub unbrokered: Vec<ProcessGroup>,
    /// Assets this node probes.
    pub assets: Vec<Asset>,
}

impl NodeSnapshot {
    pub fn pool(&self, name: &str) -> Option<&Pool> {
        self.pools.iter().find(|p| p.name == name)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Server,
    Nas,
    Camera,
    Router,
    Switch,
    AccessPoint,
    Service,
    #[default]
    Other,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Up,
    Degraded,
    Down,
    #[default]
    Unknown,
}

impl Health {
    /// Which of two observations of one asset to believe. Reachable from anywhere
    /// means it is up; a single failed path is more likely that path.
    fn rank(self) -> u8 {
        match self {
            Health::Up => 3,
            Health::Degraded => 2,
            Health::Down => 1,
            Health::Unknown => 0,
        }
    }
}

/// Something on the network, as seen by a probe.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Asset {
    /// Configured, so that two nodes probing one device name it the same.
    pub id: String,
    pub name: String,
    pub kind: AssetKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    pub health: Health,
    /// Which node took this observation.
    pub observer: String,
    pub checked_at: Millis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f32>,
    /// Free-form gauges a probe found: `cpu_percent`, `disk_percent`, …
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metrics: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Collapse observations of the same asset from several nodes into one each: the
/// healthiest wins, then the freshest.
pub fn merge_assets<I: IntoIterator<Item = Asset>>(observations: I) -> Vec<Asset> {
    let mut merged: BTreeMap<String, Asset> = BTreeMap::new();
    for asset in observations {
        match merged.get_mut(&asset.id) {
            None => {
                merged.insert(asset.id.clone(), asset);
            }
            Some(best) => {
                let key = |a: &Asset| (a.health.rank(), a.checked_at);
                if key(&asset) > key(best) {
                    *best = asset;
                }
            }
        }
    }
    merged.into_values().collect()
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerSource {
    Config,
    Mdns,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerState {
    Online,
    Unreachable,
    /// It answered, but refused our token. Worth telling apart: this is a
    /// configuration fault, not a network one.
    Unauthorized,
}

/// A remote node, as this node last saw it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Peer {
    pub url: String,
    pub source: PeerSource,
    pub state: PeerState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<Millis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The last good snapshot, kept while the peer is unreachable so a client can
    /// show how stale it is rather than nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<NodeSnapshot>,
}

/// Everything a monitoring client wants: this node, and what it knows of the rest.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Fleet {
    pub local: NodeSnapshot,
    pub peers: Vec<Peer>,
    /// Across all nodes, one entry per asset.
    pub assets: Vec<Asset>,
}

/// Measured spend for one label, across finished leases.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HistoryRow {
    pub pool: String,
    pub label: String,
    pub runs: u64,
    pub avg_amount: f64,
    /// Average cores used while held. Compare with `avg_amount`.
    pub avg_cores: f64,
    pub peak_cores: f64,
    pub peak_rss_bytes: u64,
    pub avg_held_ms: f64,
    pub avg_wait_ms: f64,
    pub last_at: Millis,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(id: &str, health: Health, at: Millis, by: &str) -> Asset {
        Asset {
            id: id.into(),
            health,
            checked_at: at,
            observer: by.into(),
            ..Default::default()
        }
    }

    #[test]
    fn merge_prefers_up_over_a_fresher_failure() {
        let merged = merge_assets([
            seen("nas", Health::Up, 100, "office"),
            seen("nas", Health::Down, 200, "home"),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].observer, "office");
    }

    #[test]
    fn merge_takes_the_freshest_among_equals() {
        let merged = merge_assets([
            seen("nas", Health::Up, 100, "office"),
            seen("nas", Health::Up, 200, "home"),
            seen("cam", Health::Down, 50, "office"),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[1].observer, "home");
    }
}
