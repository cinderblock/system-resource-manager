//! Prometheus text format, in both directions: reading `node_exporter`, and exposing
//! this node to the Prometheus that's already watching the fleet.

use srm_proto::NodeSnapshot;
use std::collections::BTreeMap;
use std::fmt::Write;

#[derive(Debug, PartialEq)]
pub struct Sample<'a> {
    pub name: &'a str,
    /// The raw text between the braces.
    pub labels: &'a str,
    pub value: f64,
}

pub fn parse(text: &str) -> Vec<Sample<'_>> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (series, rest) = match line.rfind('}') {
                Some(end) => (&line[..=end], &line[end + 1..]),
                None => line.split_once(' ')?,
            };
            let (name, labels) = match series.split_once('{') {
                Some((name, labels)) => (name, labels.trim_end_matches('}')),
                None => (series, ""),
            };
            Some(Sample {
                name,
                labels,
                value: rest.split_whitespace().next()?.parse().ok()?,
            })
        })
        .collect()
}

/// The running totals of a `node_exporter`, for working out a rate between scrapes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CpuTotals {
    pub idle: f64,
    pub all: f64,
}

/// What one scrape says about a machine.
pub fn node_exporter_gauges(
    samples: &[Sample],
    previous: Option<CpuTotals>,
) -> (BTreeMap<String, f64>, CpuTotals) {
    let mut gauges = BTreeMap::new();
    let one = |name: &str, label: &str| {
        samples
            .iter()
            .find(|s| s.name == name && s.labels.contains(label))
            .map(|s| s.value)
    };

    let mut totals = CpuTotals::default();
    for sample in samples
        .iter()
        .filter(|s| s.name == "node_cpu_seconds_total")
    {
        totals.all += sample.value;
        if sample.labels.contains(r#"mode="idle""#) {
            totals.idle += sample.value;
        }
    }
    if let Some(before) = previous {
        let all = totals.all - before.all;
        if all > 0.0 {
            let busy = 1.0 - (totals.idle - before.idle) / all;
            gauges.insert("cpu_percent".to_string(), (busy * 100.0).clamp(0.0, 100.0));
        }
    }

    if let (Some(available), Some(total)) = (
        one("node_memory_MemAvailable_bytes", ""),
        one("node_memory_MemTotal_bytes", ""),
    ) {
        if total > 0.0 {
            gauges.insert(
                "memory_percent".to_string(),
                (1.0 - available / total) * 100.0,
            );
        }
    }

    const ROOT: &str = r#"mountpoint="/""#;
    if let (Some(free), Some(size)) = (
        one("node_filesystem_avail_bytes", ROOT),
        one("node_filesystem_size_bytes", ROOT),
    ) {
        if size > 0.0 {
            gauges.insert("disk_percent".to_string(), (1.0 - free / size) * 100.0);
        }
    }

    if let Some(load) = one("node_load1", "") {
        gauges.insert("load1".to_string(), load);
    }
    if let (Some(now), Some(boot)) = (
        one("node_time_seconds", ""),
        one("node_boot_time_seconds", ""),
    ) {
        gauges.insert("uptime_seconds".to_string(), now - boot);
    }
    (gauges, totals)
}

fn escape(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// This node, for Prometheus. Per-pool rather than per-lease: lease labels are
/// unbounded, and that would be a time series for every build ever run.
pub fn expose(snapshot: &NodeSnapshot) -> String {
    let mut out = String::new();
    let system = &snapshot.system;
    let _ = writeln!(out, "srm_cpu_percent {}", system.cpu_percent);
    let _ = writeln!(out, "srm_memory_used_bytes {}", system.mem_used_bytes);
    let _ = writeln!(out, "srm_memory_total_bytes {}", system.mem_total_bytes);
    let unbrokered: f32 = snapshot.unbrokered.iter().map(|g| g.cores).sum();
    let _ = writeln!(out, "srm_unbrokered_cores {unbrokered}");
    for pool in &snapshot.pools {
        let name = escape(&pool.name);
        let cores: f32 = pool
            .holders
            .iter()
            .filter_map(|l| l.usage.as_ref())
            .map(|u| u.cores)
            .sum();
        let _ = writeln!(
            out,
            "srm_pool_capacity{{pool=\"{name}\"}} {}",
            pool.capacity
        );
        let _ = writeln!(out, "srm_pool_used{{pool=\"{name}\"}} {}", pool.used);
        let _ = writeln!(
            out,
            "srm_pool_waiting{{pool=\"{name}\"}} {}",
            pool.waiters.len()
        );
        let _ = writeln!(out, "srm_pool_cores{{pool=\"{name}\"}} {cores}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRAPE: &str = r#"
# HELP node_load1 1m load average.
# TYPE node_load1 gauge
node_load1 1.5
node_memory_MemAvailable_bytes 2.5e+09
node_memory_MemTotal_bytes 1e+10
node_filesystem_avail_bytes{device="/dev/sda1",fstype="ext4",mountpoint="/"} 2.5e+10
node_filesystem_size_bytes{device="/dev/sda1",fstype="ext4",mountpoint="/"} 1e+11
node_filesystem_avail_bytes{device="/dev/sdb1",fstype="ext4",mountpoint="/data"} 1
node_filesystem_size_bytes{device="/dev/sdb1",fstype="ext4",mountpoint="/data"} 2
node_cpu_seconds_total{cpu="0",mode="idle"} 100
node_cpu_seconds_total{cpu="0",mode="user"} 50
node_cpu_seconds_total{cpu="1",mode="idle"} 100
node_cpu_seconds_total{cpu="1",mode="user"} 50
node_boot_time_seconds 1000
node_time_seconds 4600
"#;

    #[test]
    fn parses_names_labels_and_values() {
        let samples = parse("# comment\n\na 1\nb{x=\"1\",y=\"}\"} 2.5 1234\n");
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].name, "a");
        assert_eq!(samples[1].value, 2.5);
        assert_eq!(samples[1].labels, r#"x="1",y="}""#);
    }

    #[test]
    fn junk_lines_are_skipped() {
        assert!(parse("<html>\nnot a metric\n").is_empty());
    }

    #[test]
    fn a_scrape_gives_gauges() {
        let (gauges, totals) = node_exporter_gauges(&parse(SCRAPE), None);
        assert_eq!(gauges["memory_percent"], 75.0);
        assert_eq!(
            gauges["disk_percent"], 75.0,
            "the root filesystem, not /data"
        );
        assert_eq!(gauges["load1"], 1.5);
        assert_eq!(gauges["uptime_seconds"], 3600.0);
        assert_eq!(
            totals,
            CpuTotals {
                idle: 200.0,
                all: 300.0
            }
        );
    }

    #[test]
    fn cpu_needs_two_scrapes_because_it_is_a_rate() {
        let (gauges, _) = node_exporter_gauges(&parse(SCRAPE), None);
        assert!(!gauges.contains_key("cpu_percent"));

        let before = CpuTotals {
            idle: 150.0,
            all: 200.0,
        };
        let (gauges, _) = node_exporter_gauges(&parse(SCRAPE), Some(before));
        assert_eq!(gauges["cpu_percent"], 50.0);
    }

    #[test]
    fn exposes_pools_with_escaped_names() {
        let snapshot = NodeSnapshot {
            pools: vec![srm_proto::Pool {
                name: "a\"b".into(),
                capacity: 4,
                used: 1,
                holders: vec![],
                waiters: vec![],
            }],
            ..Default::default()
        };
        let text = expose(&snapshot);
        assert!(
            text.contains(r#"srm_pool_capacity{pool="a\"b"} 4"#),
            "{text}"
        );
        assert_eq!(parse(&text).len(), 8);
    }
}
