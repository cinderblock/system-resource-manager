//! Looking at things that can't run `srmd`. Read-only by construction: every probe is
//! a connection or a GET.

use crate::metrics::{self, CpuTotals};
use srm_proto::config::{AssetConfig, ProbeConfig};
use srm_proto::{now_ms, Asset, Health};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Full enough to want attention, while everything still works.
const DISK_WARN_PERCENT: f64 = 90.0;
const MEMORY_WARN_PERCENT: f64 = 95.0;

pub struct Prober {
    http: reqwest::Client,
    /// Devices serve self-signed certificates almost without exception, and a
    /// liveness check sends nothing worth protecting.
    tolerant: reqwest::Client,
    cpu: Mutex<HashMap<String, CpuTotals>>,
}

struct Reading {
    health: Health,
    latency_ms: Option<f32>,
    metrics: BTreeMap<String, f64>,
    detail: Option<String>,
}

impl Reading {
    fn down(detail: impl ToString) -> Self {
        Self {
            health: Health::Down,
            latency_ms: None,
            metrics: BTreeMap::new(),
            detail: Some(detail.to_string()),
        }
    }

    fn up(started: Instant) -> Self {
        Self {
            health: Health::Up,
            latency_ms: Some(started.elapsed().as_secs_f32() * 1000.0),
            metrics: BTreeMap::new(),
            detail: None,
        }
    }
}

/// Whether a reachable machine still needs looking at.
fn assess(metrics: &BTreeMap<String, f64>) -> Option<String> {
    let mut problems = Vec::new();
    if metrics.get("disk_percent").is_some_and(|v| *v >= DISK_WARN_PERCENT) {
        problems.push(format!("disk {:.0}% full", metrics["disk_percent"]));
    }
    if metrics.get("memory_percent").is_some_and(|v| *v >= MEMORY_WARN_PERCENT) {
        problems.push(format!("memory {:.0}% used", metrics["memory_percent"]));
    }
    (!problems.is_empty()).then(|| problems.join(", "))
}

impl Prober {
    pub fn new() -> anyhow::Result<Self> {
        let build = || reqwest::Client::builder().timeout(TIMEOUT);
        Ok(Self {
            http: build().build()?,
            tolerant: build().danger_accept_invalid_certs(true).build()?,
            cpu: Mutex::default(),
        })
    }

    async fn tcp(host: &str, port: u16) -> Reading {
        let started = Instant::now();
        match tokio::time::timeout(TIMEOUT, TcpStream::connect((host, port))).await {
            Ok(Ok(_)) => Reading::up(started),
            Ok(Err(e)) => Reading::down(e),
            Err(_) => Reading::down("timed out"),
        }
    }

    async fn http(&self, url: &str) -> Reading {
        let started = Instant::now();
        match self.tolerant.get(url).send().await {
            Ok(response) if response.status().is_server_error() => Reading {
                health: Health::Degraded,
                detail: Some(format!("HTTP {}", response.status().as_u16())),
                ..Reading::up(started)
            },
            // A 401 from a NAS still proves the NAS is up.
            Ok(_) => Reading::up(started),
            Err(e) => Reading::down(e.without_url()),
        }
    }

    async fn node_exporter(&self, id: &str, probe: &ProbeConfig) -> Reading {
        let ProbeConfig::NodeExporter { url, basic_auth } = probe else {
            unreachable!("only called for this variant");
        };
        let started = Instant::now();
        let mut request = self.http.get(url);
        if let Some(auth) = basic_auth {
            request = request.basic_auth(&auth.username, Some(&auth.password));
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => return Reading::down(e.without_url()),
        };
        if !response.status().is_success() {
            return Reading {
                health: Health::Degraded,
                detail: Some(format!("HTTP {}", response.status().as_u16())),
                ..Reading::up(started)
            };
        }
        let text = match response.text().await {
            Ok(text) => text,
            Err(e) => return Reading::down(e.without_url()),
        };
        let reachable = Reading::up(started);
        let samples = metrics::parse(&text);
        let mut cpu = self.cpu.lock().expect("cpu totals lock");
        let previous = cpu.get(id).copied();
        let (gauges, totals) = metrics::node_exporter_gauges(&samples, previous);
        cpu.insert(id.to_string(), totals);
        drop(cpu);
        let warning = assess(&gauges);
        Reading {
            health: if warning.is_some() {
                Health::Degraded
            } else {
                Health::Up
            },
            metrics: gauges,
            detail: warning,
            ..reachable
        }
    }

    pub async fn check(&self, asset: &AssetConfig, observer: &str) -> Asset {
        let reading = match &asset.probe {
            ProbeConfig::Tcp { host, port } => Self::tcp(host, *port).await,
            ProbeConfig::Http { url } => self.http(url).await,
            probe @ ProbeConfig::NodeExporter { .. } => self.node_exporter(&asset.id, probe).await,
        };
        Asset {
            id: asset.id.clone(),
            name: asset.name.clone().unwrap_or_else(|| asset.id.clone()),
            kind: asset.kind,
            site: asset.site.clone(),
            health: reading.health,
            observer: observer.to_string(),
            checked_at: now_ms(),
            latency_ms: reading.latency_ms,
            metrics: reading.metrics,
            detail: reading.detail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_machine_has_nothing_to_say() {
        let gauges = BTreeMap::from([("disk_percent".to_string(), 50.0)]);
        assert_eq!(assess(&gauges), None);
    }

    #[test]
    fn every_problem_is_named() {
        let gauges = BTreeMap::from([
            ("disk_percent".to_string(), 95.0),
            ("memory_percent".to_string(), 97.0),
        ]);
        assert_eq!(assess(&gauges).unwrap(), "disk 95% full, memory 97% used");
    }
}
