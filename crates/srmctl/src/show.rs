//! Plain-text views for the terminal.

use srm_proto::{Fleet, HistoryRow, Lease, NodeSnapshot, PeerState, Pool};
use std::fmt::Write;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

fn bytes(n: u64) -> String {
    let gib = n as f64 / GIB;
    if gib >= 1.0 {
        format!("{gib:.1} GiB")
    } else {
        format!("{:.0} MiB", gib * 1024.0)
    }
}

fn duration(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs / 60 % 60),
    }
}

fn lease_line(out: &mut String, lease: &Lease, now: i64) {
    let since = lease.granted_at.unwrap_or(lease.requested_at);
    let _ = write!(
        out,
        "    {:>2}  {}  pid {}  {}",
        lease.amount,
        lease.label,
        lease.pid,
        duration(now - since)
    );
    if let Some(repo) = &lease.repo {
        let _ = write!(out, "  [{repo}]");
    }
    let _ = writeln!(out);
    if let Some(usage) = &lease.usage {
        let _ = writeln!(
            out,
            "        using {:.1} cores (peak {:.1}), {}, {} processes",
            usage.cores,
            usage.peak_cores,
            bytes(usage.rss_bytes),
            usage.processes
        );
        for process in usage.top.iter().filter(|p| p.cores >= 0.05).take(3) {
            let _ = writeln!(out, "          {:>4.1}  {}", process.cores, process.name);
        }
    }
}

fn pool(out: &mut String, pool: &Pool, now: i64) {
    let _ = writeln!(
        out,
        "  {}: {}/{} in use",
        pool.name, pool.used, pool.capacity
    );
    for lease in &pool.holders {
        lease_line(out, lease, now);
    }
    if !pool.waiters.is_empty() {
        let _ = writeln!(out, "  queued:");
        for lease in &pool.waiters {
            lease_line(out, lease, now);
        }
    }
}

fn node(out: &mut String, snapshot: &NodeSnapshot) {
    let system = &snapshot.system;
    let _ = writeln!(
        out,
        "{}: CPU {:.0}% of {} cores, {} of {} memory",
        snapshot.node.name,
        system.cpu_percent,
        system.cores_logical,
        bytes(system.mem_used_bytes),
        bytes(system.mem_total_bytes),
    );
    let now = snapshot.taken_at;
    for entry in &snapshot.pools {
        pool(out, entry, now);
    }
    if !snapshot.unbrokered.is_empty() {
        let _ = writeln!(out, "  outside the budget:");
        for group in &snapshot.unbrokered {
            let _ = writeln!(
                out,
                "    {:.1} cores  {}  pid {}, {} processes, {}",
                group.cores,
                group.name,
                group.root_pid,
                group.processes,
                bytes(group.rss_bytes)
            );
        }
    }
}

pub fn fleet(fleet: &Fleet) -> String {
    let mut out = String::new();
    node(&mut out, &fleet.local);
    for peer in &fleet.peers {
        match (peer.state, &peer.snapshot) {
            (PeerState::Online, Some(snapshot)) => {
                out.push('\n');
                node(&mut out, snapshot);
            }
            _ => {
                let _ = writeln!(out, "\n{}: {:?}", peer.url, peer.state);
                if let Some(error) = &peer.error {
                    let _ = writeln!(out, "  {error}");
                }
            }
        }
    }
    if !fleet.assets.is_empty() {
        let _ = writeln!(out, "\nassets:");
        for asset in &fleet.assets {
            let _ = write!(out, "  {:?}  {}", asset.health, asset.name);
            if let Some(latency) = asset.latency_ms {
                let _ = write!(out, "  {latency:.0} ms");
            }
            if let Some(detail) = &asset.detail {
                let _ = write!(out, "  {detail}");
            }
            let _ = writeln!(out, "  (from {})", asset.observer);
        }
    }
    out
}

pub fn history(rows: &[HistoryRow]) -> String {
    let mut out = String::from("claimed  used  peak  runs  avg held  avg wait  label\n");
    for row in rows {
        let _ = writeln!(
            out,
            "{:>7.1}  {:>4.1}  {:>4.1}  {:>4}  {:>8}  {:>8}  {}{}",
            row.avg_amount,
            row.avg_cores,
            row.peak_cores,
            row.runs,
            duration(row.avg_held_ms as i64),
            duration(row.avg_wait_ms as i64),
            row.label,
            if row.pool == srm_proto::DEFAULT_POOL {
                String::new()
            } else {
                format!("  ({})", row.pool)
            },
        );
    }
    out
}
