//! Everything past the local pipe: the read-only face other nodes pull from, pulling
//! from them, discovery, and probes.
//!
//! Nothing here runs unless the config asks for it. Serving is `network.enabled`;
//! pulling and probing happen when peers or assets are configured, so a machine can
//! watch the fleet without accepting a single connection.
//!
//! State is pulled, not pushed. mDNS says a node exists; whoever is interested asks it
//! how it's doing. mDNS stops at the subnet, so the static peer list is what reaches
//! other sites and the tailnet.

use crate::metrics;
use crate::probe::Prober;
use crate::state::Shared;
use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use srm_proto::config::{BasicAuth, Config};
use srm_proto::{now_ms, NodeSnapshot, Peer, PeerSource, PeerState};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinSet;

const TOKEN_HEADER: &str = "x-srm-token";
const PULL_TIMEOUT: Duration = Duration::from_secs(5);
/// How much less often to poll when no client is watching.
const UNWATCHED_BACKOFF: u32 = 6;
pub const MDNS_SERVICE: &str = "_srmd._tcp.local.";

/// A shared secret for one fleet, generated the first time it's needed. Copy
/// `~/.srm/network-token` to the other nodes to let them in.
fn load_token(home: &Path, create: bool) -> Result<Option<String>> {
    let path = home.join("network-token");
    match std::fs::read_to_string(&path) {
        Ok(token) => Ok(Some(token.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            std::fs::write(&path, &token)
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(Some(token))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn same(a: &str, b: &str) -> bool {
    // Compared in full whatever the first difference, so timing says nothing.
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

/// Own header first, because a reverse proxy's basic auth occupies `Authorization`.
/// A bearer token also works, since that's what Prometheus can send.
fn presented(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        })
}

async fn require_token(State(token): State<Arc<String>>, request: Request, next: Next) -> Response {
    match presented(request.headers()) {
        Some(given) if same(given, &token) => next.run(request).await,
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn snapshot(State(shared): State<Arc<Shared>>) -> Json<NodeSnapshot> {
    Json(shared.node_snapshot())
}

async fn prometheus(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        metrics::expose(&shared.node_snapshot()),
    )
}

async fn serve(shared: Arc<Shared>, addr: SocketAddr, token: String) -> Result<()> {
    let app = Router::new()
        .route("/v1/snapshot", get(snapshot))
        .route("/metrics", get(prometheus))
        .layer(middleware::from_fn_with_state(Arc::new(token), require_token))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("serving on {addr}");
    axum::serve(listener, app).await.context("serving")
}

/// A peer found by mDNS, keyed by its service name.
#[derive(Clone, Debug, PartialEq)]
struct Discovered {
    id: Option<String>,
    urls: Vec<String>,
}

type Found = Arc<Mutex<BTreeMap<String, Discovered>>>;

/// Most useful first. A routable address beats a link-local one, and IPv4 beats IPv6
/// because a URL with a link-local scope is its own problem.
fn rank(ip: &IpAddr) -> Option<u8> {
    match ip {
        IpAddr::V4(v4) if v4.is_loopback() => None,
        IpAddr::V4(v4) if v4.is_link_local() => Some(2),
        IpAddr::V4(_) => Some(0),
        IpAddr::V6(v6) if v6.is_loopback() || v6.is_unicast_link_local() => None,
        IpAddr::V6(_) => Some(1),
    }
}

fn urls_for(addresses: impl IntoIterator<Item = IpAddr>, port: u16) -> Vec<String> {
    let mut ranked: Vec<(u8, IpAddr)> = addresses
        .into_iter()
        .filter_map(|ip| rank(&ip).map(|r| (r, ip)))
        .collect();
    ranked.sort();
    ranked.dedup();
    ranked
        .into_iter()
        .map(|(_, ip)| format!("http://{}", SocketAddr::new(ip, port)))
        .collect()
}

pub fn announce(daemon: &ServiceDaemon, id: &str, name: &str, port: u16) -> Result<()> {
    let service = ServiceInfo::new(
        MDNS_SERVICE,
        name,
        &format!("{name}.local."),
        // No explicit address: this tracks interfaces as they come and go.
        "",
        port,
        &[("id", id), ("version", env!("CARGO_PKG_VERSION"))][..],
    )?
    .enable_addr_auto();
    daemon.register(service).context("announcing")
}

fn browse(daemon: &ServiceDaemon, own_id: String, found: Found) -> Result<()> {
    let events = daemon.browse(MDNS_SERVICE).context("browsing")?;
    std::thread::Builder::new()
        .name("mdns-browse".into())
        .spawn(move || {
            while let Ok(event) = events.recv() {
                match event {
                    ServiceEvent::ServiceResolved(service) => {
                        let id = service.get_property_val_str("id").map(str::to_string);
                        if id.as_deref() == Some(&own_id) {
                            continue;
                        }
                        let addresses = service.get_addresses().iter().map(|a| a.to_ip_addr());
                        let urls = urls_for(addresses, service.get_port());
                        if !urls.is_empty() {
                            let name = service.get_fullname().to_string();
                            found
                                .lock()
                                .expect("discovery lock")
                                .insert(name, Discovered { id, urls });
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, name) => {
                        found.lock().expect("discovery lock").remove(&name);
                    }
                    _ => {}
                }
            }
        })?;
    Ok(())
}

struct Target {
    key: String,
    urls: Vec<String>,
    source: PeerSource,
    token: Option<String>,
    basic_auth: Option<BasicAuth>,
}

fn targets(config: &Config, found: &Found, token: &Option<String>) -> Vec<Target> {
    let mut list: Vec<Target> = config
        .peers
        .iter()
        .map(|peer| Target {
            key: peer.url.clone(),
            urls: vec![peer.url.clone()],
            source: PeerSource::Config,
            token: peer.token.clone().or_else(|| token.clone()),
            basic_auth: peer.basic_auth.clone(),
        })
        .collect();
    list.extend(found.lock().expect("discovery lock").iter().map(|(key, peer)| Target {
        key: key.clone(),
        urls: peer.urls.clone(),
        source: PeerSource::Mdns,
        token: token.clone(),
        basic_auth: None,
    }));
    list
}

async fn pull(http: &reqwest::Client, base: &str, target: &Target) -> Result<NodeSnapshot, (PeerState, String)> {
    let mut request = http.get(format!("{}/v1/snapshot", base.trim_end_matches('/')));
    if let Some(token) = &target.token {
        request = request.header(TOKEN_HEADER, token);
    }
    if let Some(auth) = &target.basic_auth {
        request = request.basic_auth(&auth.username, Some(&auth.password));
    }
    let unreachable = |e: reqwest::Error| (PeerState::Unreachable, e.without_url().to_string());
    let response = request.send().await.map_err(unreachable)?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err((PeerState::Unauthorized, format!("HTTP {}", status.as_u16())));
    }
    if !status.is_success() {
        return Err((PeerState::Unreachable, format!("HTTP {}", status.as_u16())));
    }
    response.json().await.map_err(unreachable)
}

/// Try each address until one answers.
async fn check_peer(http: reqwest::Client, target: Target, previous: Option<Peer>) -> Peer {
    let mut failure = (PeerState::Unreachable, "no address".to_string());
    for url in &target.urls {
        match pull(&http, url, &target).await {
            Ok(snapshot) => {
                return Peer {
                    url: url.clone(),
                    source: target.source,
                    state: PeerState::Online,
                    last_seen: Some(now_ms()),
                    error: None,
                    snapshot: Some(snapshot),
                }
            }
            Err(e) => failure = e,
        }
    }
    Peer {
        url: target.urls.first().cloned().unwrap_or(target.key),
        source: target.source,
        state: failure.0,
        last_seen: previous.as_ref().and_then(|p| p.last_seen),
        error: Some(failure.1),
        snapshot: previous.and_then(|p| p.snapshot),
    }
}

/// One node reached two ways (configured, and seen on the LAN) is one peer. So is
/// ourselves, reached through a list shared across the fleet.
fn deduplicate(peers: Vec<Peer>, own_id: &str) -> Vec<Peer> {
    let mut by_node: HashMap<String, Peer> = HashMap::new();
    let mut unknown = Vec::new();
    for peer in peers {
        let Some(id) = peer.snapshot.as_ref().map(|s| s.node.id.clone()) else {
            unknown.push(peer);
            continue;
        };
        if id == own_id {
            continue;
        }
        match by_node.get(&id) {
            Some(existing) if existing.source == PeerSource::Config => {}
            _ => {
                by_node.insert(id, peer);
            }
        }
    }
    let mut all: Vec<Peer> = by_node.into_values().chain(unknown).collect();
    all.sort_by(|a, b| a.url.cmp(&b.url));
    all
}

async fn wait_for_next_round(shared: &Shared, poll: Duration) {
    let interval = if shared.fleet.receiver_count() > 0 {
        poll
    } else {
        poll * UNWATCHED_BACKOFF
    };
    tokio::select! {
        _ = tokio::time::sleep(interval) => {}
        _ = shared.watcher_arrived.notified() => {}
    }
}

async fn run_peers(shared: Arc<Shared>, found: Found, token: Option<String>) -> Result<()> {
    let http = reqwest::Client::builder().timeout(PULL_TIMEOUT).build()?;
    let own_id = shared.node.id.clone();
    let mut known: HashMap<String, Peer> = HashMap::new();
    loop {
        let config = shared.config.borrow().clone();
        let mut pulls = JoinSet::new();
        for target in targets(&config, &found, &token) {
            let key = target.key.clone();
            let previous = known.get(&key).cloned();
            let http = http.clone();
            pulls.spawn(async move { (key, check_peer(http, target, previous).await) });
        }
        let mut current = HashMap::new();
        while let Some(result) = pulls.join_next().await {
            if let Ok((key, peer)) = result {
                current.insert(key, peer);
            }
        }
        known = current;
        shared.set_peers(deduplicate(known.values().cloned().collect(), &own_id));
        wait_for_next_round(&shared, Duration::from_secs(config.network.poll_secs.max(1))).await;
    }
}

async fn run_probes(shared: Arc<Shared>) -> Result<()> {
    let prober = Arc::new(Prober::new()?);
    loop {
        let config = shared.config.borrow().clone();
        // At the same time, so one powered-off device costs one timeout rather than
        // delaying every check after it.
        let mut checks = JoinSet::new();
        for asset in config.assets.clone() {
            let prober = prober.clone();
            let observer = shared.node.name.clone();
            checks.spawn(async move { prober.check(&asset, &observer).await });
        }
        let mut assets = Vec::with_capacity(config.assets.len());
        while let Some(result) = checks.join_next().await {
            assets.extend(result.ok());
        }
        assets.sort_by(|a, b| a.id.cmp(&b.id));
        shared.set_assets(assets);
        wait_for_next_round(&shared, Duration::from_secs(config.network.poll_secs.max(1))).await;
    }
}

fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// Start whatever the config asks for. A failure here is logged rather than fatal: the
/// local broker is the important part, and it needs none of this.
pub fn start(shared: Arc<Shared>, config: &Config, home: &Path) {
    let found: Found = Arc::default();
    let network = &config.network;

    let token = match load_token(home, network.enabled) {
        Ok(token) => token,
        Err(e) => {
            tracing::error!("network token: {e:#}");
            None
        }
    };

    if network.enabled {
        match (network.listen.parse::<SocketAddr>(), token.clone()) {
            (Ok(addr), Some(token)) => {
                let serving = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(serving, addr, token).await {
                        tracing::error!("{e:#}");
                    }
                });
                if network.mdns {
                    start_mdns(&shared, addr, found.clone());
                }
            }
            (Err(e), _) => tracing::error!("network.listen {:?}: {e}", network.listen),
            (_, None) => tracing::error!("no network token, so not serving"),
        }
    }

    // Always running, so a later reload can add peers and assets without a restart.
    let peers = shared.clone();
    tokio::spawn(async move {
        if let Err(e) = run_peers(peers, found, token).await {
            tracing::error!("peers: {e:#}");
        }
    });
    let probes = shared;
    tokio::spawn(async move {
        if let Err(e) = run_probes(probes).await {
            tracing::error!("probes: {e:#}");
        }
    });
}

fn start_mdns(shared: &Shared, addr: SocketAddr, found: Found) {
    let result = (|| -> Result<()> {
        let daemon = ServiceDaemon::new().context("starting mDNS")?;
        // Nobody can reach a loopback listener, so don't invite them to try.
        if !is_loopback(&addr) {
            announce(&daemon, &shared.node.id, &shared.node.name, addr.port())?;
        }
        browse(&daemon, shared.node.id.clone(), found)?;
        std::mem::forget(daemon); // Runs on its own thread for the life of the process.
        Ok(())
    })();
    if let Err(e) = result {
        tracing::error!("mDNS: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use srm_proto::NodeInfo;

    fn peer(url: &str, id: Option<&str>, source: PeerSource) -> Peer {
        Peer {
            url: url.into(),
            source,
            state: PeerState::Online,
            last_seen: None,
            error: None,
            snapshot: id.map(|id| NodeSnapshot {
                node: NodeInfo {
                    id: id.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
        }
    }

    #[test]
    fn tokens_compare_exactly() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
        assert!(!same("", "a"));
    }

    #[test]
    fn urls_prefer_routable_addresses_and_drop_useless_ones() {
        let addresses = ["fe80::1", "127.0.0.1", "169.254.1.1", "2600::1", "10.0.0.2"]
            .map(|a| a.parse().unwrap());
        assert_eq!(
            urls_for(addresses, 7654),
            [
                "http://10.0.0.2:7654",
                "http://[2600::1]:7654",
                "http://169.254.1.1:7654"
            ]
        );
    }

    #[test]
    fn one_node_reached_two_ways_is_one_peer_and_config_wins() {
        let peers = vec![
            peer("http://mdns", Some("a"), PeerSource::Mdns),
            peer("http://config", Some("a"), PeerSource::Config),
        ];
        let merged = deduplicate(peers, "me");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].url, "http://config");
    }

    #[test]
    fn a_shared_peer_list_doesnt_make_us_our_own_peer() {
        let peers = vec![peer("http://me", Some("me"), PeerSource::Config)];
        assert!(deduplicate(peers, "me").is_empty());
    }

    #[test]
    fn unreached_peers_stay_listed() {
        let peers = vec![peer("http://gone", None, PeerSource::Config)];
        assert_eq!(deduplicate(peers, "me").len(), 1);
    }

    /// Touches the real network: a few seconds of multicast on every interface, so it
    /// doesn't run by default. `cargo test -p srmd -- --ignored mdns`
    #[test]
    #[ignore]
    fn mdns_finds_a_node_on_the_link() {
        let announcer = ServiceDaemon::new().unwrap();
        announce(&announcer, "the-other-node", "srm-test-node", 17654).unwrap();

        let listener = ServiceDaemon::new().unwrap();
        let found: Found = Arc::default();
        browse(&listener, "me".to_string(), found.clone()).unwrap();

        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            let found = found.lock().unwrap();
            if let Some(entry) = found.values().find(|d| d.id.as_deref() == Some("the-other-node")) {
                assert!(!entry.urls.is_empty());
                assert!(entry.urls[0].ends_with(":17654"), "{:?}", entry.urls);
                let _ = announcer.shutdown();
                let _ = listener.shutdown();
                return;
            }
        }
        panic!("never discovered");
    }
}
