//! The system resource manager daemon.
//!
//! Started by whichever client first finds it missing, and left running. See
//! `README.md` for the design.

mod broker;
mod history;
mod listener;
mod sampler;
mod server;
mod state;

use anyhow::{Context, Result};
use history::History;
use listener::Listener;
use sampler::{Sampler, UnbrokeredTracker};
use srm_proto::config::{self, Config};
use srm_proto::{now_ms, NodeInfo};
use state::Shared;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Sampling costs a little CPU on a machine this exists to protect, so it only runs
/// quickly when someone benefits.
const ACTIVE_INTERVAL: Duration = Duration::from_secs(2);
const IDLE_INTERVAL: Duration = Duration::from_secs(5);
/// `sysinfo` needs this much between refreshes to get a meaningful CPU reading.
const MIN_INTERVAL: Duration = Duration::from_millis(500);
/// Bursts of lease changes publish once.
const PUBLISH_DEBOUNCE: Duration = Duration::from_millis(50);
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

fn init_logging(home: &Path) -> Result<()> {
    let path = home.join("srmd.log");
    let keep = std::fs::metadata(&path)
        .map(|m| m.len() < MAX_LOG_BYTES)
        .unwrap_or(false);
    let file = std::fs::File::options()
        .create(true)
        .write(true)
        .append(keep)
        .truncate(!keep)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .with_writer(std::sync::Mutex::new(file))
        .init();
    Ok(())
}

/// Stable across restarts, so a renamed machine stays the same node to its peers.
fn node_id(home: &Path) -> Result<String> {
    let path = home.join("node-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, &id).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

fn node_info(config: &Config, home: &Path) -> Result<NodeInfo> {
    Ok(NodeInfo {
        id: node_id(home)?,
        name: config
            .node
            .name
            .clone()
            .or_else(|| sysinfo::System::host_name().ok())
            .unwrap_or_else(|| "unknown".to_string()),
        site: config.node.site.clone(),
        os: sysinfo::System::long_os_version().unwrap_or_else(|_| std::env::consts::OS.to_string()),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: now_ms(),
    })
}

async fn run_sampler(shared: Arc<Shared>) {
    let mut sampler = match Sampler::new() {
        // The broker doesn't depend on this. Losing insight mustn't stop leases.
        Err(e) => return tracing::error!("no sampler: {e}"),
        Ok(sampler) => Some(sampler),
    };
    let mut unbrokered = Some(UnbrokeredTracker::default());
    let mut last = Instant::now();
    loop {
        let interval = if shared.is_idle() {
            IDLE_INTERVAL
        } else {
            ACTIVE_INTERVAL
        };
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shared.activity.notified() => {
                tokio::time::sleep(MIN_INTERVAL).await;
            }
        }

        let elapsed = last.elapsed().as_secs_f64();
        last = Instant::now();
        let mut owned = sampler.take().expect("sampler is returned below");
        let mut tracker = unbrokered.take().expect("tracker is returned below");
        let worker = shared.clone();
        // Reading the process table is a burst of syscalls, so it stays off the
        // threads serving clients.
        let joined = tokio::task::spawn_blocking(move || {
            let (stats, procs) = owned.sample();
            worker.absorb(stats, &procs, &mut tracker, elapsed);
            (owned, tracker)
        })
        .await;
        match joined {
            Ok((owned, tracker)) => {
                sampler = Some(owned);
                unbrokered = Some(tracker);
            }
            Err(e) => {
                tracing::error!("sampler stopped: {e}");
                return;
            }
        }
    }
}

async fn run_publisher(shared: Arc<Shared>) {
    loop {
        shared.changed.notified().await;
        tokio::time::sleep(PUBLISH_DEBOUNCE).await;
        if shared.fleet.receiver_count() > 0 {
            shared.fleet.send_replace(Arc::new(shared.fleet_now()));
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--version") {
        println!("srmd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let home = config::home();
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;

    let endpoint = config::endpoint();
    let Some(mut listener) = Listener::bind(&endpoint)? else {
        return Ok(()); // Someone else won the race to start, which is all we wanted.
    };

    init_logging(&home)?;
    let config = Config::load()?;
    let node = node_info(&config, &home)?;
    tracing::info!(endpoint, name = node.name, "srmd {} starting", node.version);

    let history = History::open(&home.join("history.sqlite"))?;
    let shared = Arc::new(Shared::new(&config, node, history));

    tokio::spawn(run_sampler(shared.clone()));
    tokio::spawn(run_publisher(shared.clone()));

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    tokio::spawn(server::serve(stream, shared.clone()));
                }
                Err(e) => tracing::warn!("{e:#}"),
            },
            _ = shared.shutdown.notified() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    tracing::info!("stopping");
    Ok(())
}
