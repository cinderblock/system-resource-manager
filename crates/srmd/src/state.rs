//! The daemon's shared state, and the operations that keep its parts in step.

use crate::broker::{Broker, Grant, Outcome};
use crate::history::History;
use crate::sampler::{self, Proc, Sampler, UnbrokeredTracker};
use srm_proto::config::Config;
use srm_proto::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{oneshot, watch, Notify};

/// Shorter than this, and a final report at exit would give a meaningless spike.
const MIN_RATE_WINDOW_SECS: f64 = 0.5;

/// What the sampler knows about a held lease, beyond what clients see.
#[derive(Default)]
pub struct Track {
    /// From the client, when it can see its own processes exactly.
    reported_pids: Vec<u32>,
    reported_cpu: Option<f64>,
    /// Where the current rate is being measured from.
    rate_base: Option<(Instant, f64)>,
    /// Cores, between the client's last two reports.
    reported_rate: Option<f32>,
    integrated_cpu: f64,
    peak_cores: f32,
    peak_rss: u64,
}

pub struct State {
    pub broker: Broker,
    pending: HashMap<LeaseId, oneshot::Sender<Grant>>,
    tracks: HashMap<LeaseId, Track>,
    pub system: SystemStats,
    pub unbrokered: Vec<ProcessGroup>,
    pub peers: Vec<Peer>,
    pub assets: Vec<Asset>,
}

pub struct Shared {
    pub state: Mutex<State>,
    pub node: NodeInfo,
    pub history: History,
    pub fleet: watch::Sender<Arc<Fleet>>,
    /// The latest config, for loops that read it each round.
    pub config: watch::Sender<Config>,
    /// Wakes the publisher.
    pub changed: Notify,
    /// Wakes the sampler early, when work starts.
    pub activity: Notify,
    /// Someone started watching, so stale peers and probes should refresh now.
    pub watcher_arrived: Notify,
    pub shutdown: Notify,
}

pub enum Claim {
    Queued {
        lease: LeaseId,
        amount: u32,
        position: u32,
        free: u32,
        capacity: u32,
        holders: Vec<Lease>,
        grant: oneshot::Receiver<Grant>,
    },
    Granted(Grant),
}

/// The repository a working directory belongs to, for showing who's asking.
fn repo_of(cwd: Option<&str>) -> Option<String> {
    let mut dir = Path::new(cwd?);
    loop {
        if dir.join(".git").exists() {
            return dir.file_name().map(|n| n.to_string_lossy().into_owned());
        }
        dir = dir.parent()?;
    }
}

impl Shared {
    pub fn new(config: &Config, node: NodeInfo, history: History) -> Self {
        let pools = config.pools(Sampler::physical_cores());
        let grace = config.broker.backfill_grace_secs as Millis * 1000;
        let (fleet, _) = watch::channel(Arc::new(Fleet::default()));
        Self {
            state: Mutex::new(State {
                broker: Broker::new(pools, grace),
                pending: HashMap::new(),
                tracks: HashMap::new(),
                system: SystemStats::default(),
                unbrokered: Vec::new(),
                peers: Vec::new(),
                assets: Vec::new(),
            }),
            node,
            history,
            fleet,
            config: watch::Sender::new(config.clone()),
            changed: Notify::new(),
            activity: Notify::new(),
            watcher_arrived: Notify::new(),
            shutdown: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("state lock")
    }

    fn touched(&self) {
        self.changed.notify_one();
        self.activity.notify_one();
    }

    pub fn acquire(&self, request: LeaseRequest) -> Result<Claim, String> {
        let repo = repo_of(request.cwd.as_deref());
        let mut state = self.lock();
        let outcome = state.broker.request(request, repo, now_ms())?;
        let claim = match outcome {
            Outcome::Granted(grant) => {
                state.tracks.insert(grant.lease, Track::default());
                Claim::Granted(grant)
            }
            Outcome::Queued(q) => {
                let (send, grant) = oneshot::channel();
                state.pending.insert(q.lease, send);
                Claim::Queued {
                    lease: q.lease,
                    amount: q.amount,
                    position: q.position,
                    free: q.free,
                    capacity: q.capacity,
                    holders: q.holders,
                    grant,
                }
            }
        };
        drop(state);
        self.touched();
        Ok(claim)
    }

    pub fn adopt(
        &self,
        request: LeaseRequest,
        requested_at: Millis,
        granted_at: Millis,
    ) -> Result<Grant, String> {
        let repo = repo_of(request.cwd.as_deref());
        let mut state = self.lock();
        let grant = state
            .broker
            .adopt(request, repo, requested_at, granted_at)?;
        state.tracks.insert(grant.lease, Track::default());
        drop(state);
        self.touched();
        Ok(grant)
    }

    pub fn report(&self, lease: LeaseId, pids: Vec<u32>, cpu_seconds: f64) {
        if let Some(track) = self.lock().tracks.get_mut(&lease) {
            let now = Instant::now();
            match track.rate_base {
                None => track.rate_base = Some((now, cpu_seconds)),
                Some((at, base)) => {
                    let elapsed = now.duration_since(at).as_secs_f64();
                    if elapsed >= MIN_RATE_WINDOW_SECS {
                        track.reported_rate =
                            Some(((cpu_seconds - base) / elapsed).max(0.0) as f32);
                        track.rate_base = Some((now, cpu_seconds));
                    }
                }
            }
            track.reported_pids = pids;
            track.reported_cpu = Some(cpu_seconds);
        }
    }

    /// End a lease, because its connection closed.
    pub fn finish(self: &Arc<Self>, lease: LeaseId) {
        let now = now_ms();
        let (finished, usage) = {
            let mut state = self.lock();
            state.pending.remove(&lease);
            let track = state.tracks.remove(&lease);
            let (finished, grants) = state.broker.release(lease, now);
            deliver(&mut state, grants);
            let usage = finished
                .as_ref()
                .and_then(|l| l.usage.clone())
                .unwrap_or_default();
            // The client's own total is exact; the integral is a fallback.
            let usage = match track {
                Some(track) => Usage {
                    cpu_seconds: track.reported_cpu.unwrap_or(track.integrated_cpu),
                    peak_cores: track.peak_cores,
                    peak_rss_bytes: track.peak_rss,
                    ..usage
                },
                None => usage,
            };
            (finished, usage)
        };
        self.touched();
        if let Some(lease) = finished {
            let shared = self.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = shared.history.record(&lease, &usage, now) {
                    tracing::warn!("recording history: {e:#}");
                }
            });
        }
    }

    pub fn reconfigure(&self, config: &Config) {
        let pools: BTreeMap<_, _> = config.pools(Sampler::physical_cores());
        let grace = config.broker.backfill_grace_secs as Millis * 1000;
        let mut state = self.lock();
        let grants = state.broker.reconfigure(pools, grace, now_ms());
        deliver(&mut state, grants);
        drop(state);
        self.config.send_replace(config.clone());
        self.touched();
    }

    pub fn set_peers(&self, peers: Vec<Peer>) {
        self.lock().peers = peers;
        self.changed.notify_one();
    }

    pub fn set_assets(&self, assets: Vec<Asset>) {
        self.lock().assets = assets;
        self.changed.notify_one();
    }

    /// Fold a process sample into the leases and the unbrokered list.
    pub fn absorb(
        &self,
        stats: SystemStats,
        procs: &[Proc],
        unbrokered: &mut UnbrokeredTracker,
        elapsed_secs: f64,
    ) {
        let by_pid: HashMap<u32, usize> =
            procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
        let children = sampler::index_children(procs);
        let mut leased = HashSet::new();

        let mut state = self.lock();
        let holders: Vec<(LeaseId, u32)> = state.broker.holders().map(|l| (l.id, l.pid)).collect();

        for (id, root) in holders {
            let track = state.tracks.entry(id).or_default();
            // The tree as well as the reported list: a client's job can't hold anything
            // that left it, and a tree can't see past a parent that has exited.
            let mut members = sampler::tree(root, procs, &children, &by_pid);
            members.extend(
                track
                    .reported_pids
                    .iter()
                    .filter_map(|pid| by_pid.get(pid).copied()),
            );
            members.sort_unstable();
            members.dedup();
            leased.extend(members.iter().map(|i| procs[*i].pid));
            let (sampled, rss_bytes, top) = sampler::summarize(&members, procs);

            // A job's counter includes processes that lived and died between samples,
            // which a sum over the running ones can't see. Measured across the client's
            // own reports: against the sampler's clock, a tick landing between two reports
            // would read as nothing at all.
            let cores = track.reported_rate.unwrap_or(sampled);
            track.integrated_cpu += cores as f64 * elapsed_secs;
            track.peak_cores = track.peak_cores.max(cores);
            track.peak_rss = track.peak_rss.max(rss_bytes);
            let usage = Usage {
                cores,
                peak_cores: track.peak_cores,
                rss_bytes,
                peak_rss_bytes: track.peak_rss,
                processes: members.len() as u32,
                cpu_seconds: track.reported_cpu.unwrap_or(track.integrated_cpu),
                top,
            };
            if let Some(lease) = state.broker.lease_mut(id) {
                lease.usage = Some(usage);
            }
        }

        state.unbrokered = unbrokered.update(procs, &leased);
        state.system = stats;
        drop(state);
        self.changed.notify_one();
    }

    pub fn is_idle(&self) -> bool {
        self.lock().broker.is_idle() && self.fleet.receiver_count() == 0
    }

    pub fn node_snapshot(&self) -> NodeSnapshot {
        let state = self.lock();
        NodeSnapshot {
            node: self.node.clone(),
            taken_at: now_ms(),
            system: state.system.clone(),
            pools: state.broker.snapshot(),
            unbrokered: state.unbrokered.clone(),
            assets: state.assets.clone(),
        }
    }

    pub fn fleet_now(&self) -> Fleet {
        let local = self.node_snapshot();
        let peers = self.lock().peers.clone();
        let remote = peers
            .iter()
            .filter_map(|p| p.snapshot.as_ref())
            .flat_map(|s| s.assets.iter().cloned());
        let assets = merge_assets(local.assets.iter().cloned().chain(remote));
        Fleet {
            local,
            peers,
            assets,
        }
    }
}

fn deliver(state: &mut State, grants: Vec<Grant>) {
    for grant in grants {
        state.tracks.insert(grant.lease, Track::default());
        if let Some(waiting) = state.pending.remove(&grant.lease) {
            // A closed receiver means the client just left, and its own cleanup is
            // about to release this.
            let _ = waiting.send(grant);
        }
    }
}
