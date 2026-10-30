//! What the machine is actually doing.
//!
//! `Sampler` reads the process table; everything below it is pure, so attribution
//! and classification can be tested without a real machine.

use srm_proto::{ProcessGroup, ProcessSummary, SystemStats};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System,
    UpdateKind,
};

/// How many processes a lease lists.
const TOP_PROCESSES: usize = 6;
/// A tree spending less than this isn't worth anyone's attention.
const UNBROKERED_MIN_CORES: f32 = 1.0;
/// Samples in a row it must stay that busy, so a momentary spike doesn't flash up.
const UNBROKERED_MIN_STREAK: u32 = 2;

/// Tools that are worth reporting when they run outside a lease. Interactive programs
/// are absent on purpose: a browser or a game is the person using their machine.
const BUILD_TOOLS: &[&str] = &[
    "bazel",
    "bun",
    "c++",
    "cargo",
    "cc",
    "cc1",
    "cc1plus",
    "cl",
    "clang",
    "clang++",
    "cmake",
    "deno",
    "dotnet",
    "esbuild",
    "ffmpeg",
    "g++",
    "gcc",
    "go",
    "gradle",
    "java",
    "ld",
    "lld",
    "lld-link",
    "link",
    "make",
    "msbuild",
    "ninja",
    "node",
    "npm",
    "pnpm",
    "pytest",
    "python",
    "python3",
    "rust-analyzer",
    "rustc",
    "tsc",
    "vite",
    "webpack",
];

#[derive(Clone, Debug, Default)]
pub struct Proc {
    pub pid: u32,
    pub parent: Option<u32>,
    /// Lower-case, no `.exe`.
    pub name: String,
    pub cmd: Vec<String>,
    pub cwd: Option<String>,
    pub cores: f32,
    pub rss_bytes: u64,
    pub started: u64,
}

pub struct Sampler {
    system: System,
}

impl Sampler {
    pub fn new() -> Result<Self, sysinfo::Error> {
        let system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_usage())
                .with_memory(MemoryRefreshKind::nothing().with_ram()),
        )?;
        Ok(Self { system })
    }

    pub fn physical_cores() -> u32 {
        System::physical_core_count().unwrap_or(0) as u32
    }

    pub fn sample(&mut self) -> (SystemStats, Vec<Proc>) {
        self.system
            .refresh_cpu_specifics(CpuRefreshKind::nothing().with_usage());
        self.system
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        // Command lines and working directories are read once per process and cached:
        // they need a handle and a walk through the target's memory.
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_cwd(UpdateKind::OnlyIfNotSet),
        );

        let stats = SystemStats {
            cpu_percent: self.system.global_cpu_usage(),
            cores_physical: Self::physical_cores(),
            cores_logical: self.system.cpus().len() as u32,
            mem_used_bytes: self.system.used_memory(),
            mem_total_bytes: self.system.total_memory(),
        };

        let procs = self
            .system
            .processes()
            .values()
            .map(|p| Proc {
                pid: p.pid().as_u32(),
                parent: p.parent().map(|pid| pid.as_u32()),
                name: normalize_name(&p.name().to_string_lossy()),
                cmd: p
                    .cmd()
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
                cwd: p.cwd().map(|dir| dir.to_string_lossy().into_owned()),
                cores: p.cpu_usage() / 100.0,
                rss_bytes: p.memory(),
                started: p.start_time(),
            })
            .collect();
        (stats, procs)
    }
}

fn normalize_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

/// A name a person can use. The executable alone says nothing when there are forty
/// `rustc`s and a dozen `node`s.
pub fn describe(proc: &Proc) -> String {
    let args = proc.cmd.get(1..).unwrap_or_default();
    let value_after = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let script = || {
        args.iter()
            .find(|a| !a.starts_with('-'))
            .and_then(|a| Path::new(a).file_name())
            .map(|f| f.to_string_lossy().into_owned())
    };
    let detail = match proc.name.as_str() {
        "rustc" => value_after("--crate-name"),
        "node" | "bun" | "deno" | "python" | "python3" => script(),
        "chrome" | "msedge" | "chromium" | "chrome-headless-shell" | "firefox" | "webkit" => args
            .iter()
            .find_map(|a| a.strip_prefix("--type="))
            .map(str::to_string),
        _ => None,
    };
    match detail {
        Some(detail) => format!("{} {detail}", proc.name),
        None => proc.name.clone(),
    }
}

pub fn index_children(procs: &[Proc]) -> HashMap<u32, Vec<usize>> {
    let started: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.started)).collect();
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, proc) in procs.iter().enumerate() {
        // Windows reuses PIDs and never updates a parent ID, so a "parent" younger
        // than its child is a stranger who inherited the number.
        if let Some(parent) = proc.parent.filter(|p| *p != proc.pid) {
            if started.get(&parent).is_some_and(|s| *s <= proc.started) {
                children.entry(parent).or_default().push(i);
            }
        }
    }
    children
}

/// `root` and all its descendants that are still running.
pub fn tree(
    root: u32,
    procs: &[Proc],
    children: &HashMap<u32, Vec<usize>>,
    by_pid: &HashMap<u32, usize>,
) -> Vec<usize> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(i) = by_pid.get(&pid) {
            found.push(*i);
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().map(|i| procs[*i].pid));
        }
    }
    found
}

pub fn summarize(indices: &[usize], procs: &[Proc]) -> (f32, u64, Vec<ProcessSummary>) {
    let cores = indices.iter().map(|i| procs[*i].cores).sum();
    let rss = indices.iter().map(|i| procs[*i].rss_bytes).sum();
    let mut busiest: Vec<&Proc> = indices.iter().map(|i| &procs[*i]).collect();
    busiest.sort_by(|a, b| {
        b.cores
            .total_cmp(&a.cores)
            .then(b.rss_bytes.cmp(&a.rss_bytes))
    });
    let top = busiest
        .into_iter()
        .take(TOP_PROCESSES)
        .map(|p| ProcessSummary {
            pid: p.pid,
            name: describe(p),
            cores: p.cores,
            rss_bytes: p.rss_bytes,
        })
        .collect();
    (cores, rss, top)
}

/// Notices build-tool trees that stay busy outside any lease.
#[derive(Default)]
pub struct UnbrokeredTracker {
    streaks: HashMap<u32, u32>,
}

impl UnbrokeredTracker {
    pub fn update(&mut self, procs: &[Proc], leased: &HashSet<u32>) -> Vec<ProcessGroup> {
        let by_pid: HashMap<u32, usize> =
            procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
        let children = index_children(procs);
        let is_tool = |p: &Proc| BUILD_TOOLS.contains(&p.name.as_str());

        // The topmost tool of each tree, so `cargo` -> `rustc` -> `link` is one thing.
        let mut roots = HashSet::new();
        for proc in procs
            .iter()
            .filter(|p| is_tool(p) && !leased.contains(&p.pid))
        {
            let mut root = proc;
            let mut steps = 0;
            while let Some(parent) = root
                .parent
                .and_then(|pid| by_pid.get(&pid))
                .map(|i| &procs[*i])
                .filter(|parent| {
                    is_tool(parent)
                        && !leased.contains(&parent.pid)
                        && parent.started <= root.started
                })
            {
                root = parent;
                steps += 1;
                if steps > 64 {
                    break; // Reused PIDs can form a cycle.
                }
            }
            roots.insert(root.pid);
        }

        let mut groups = Vec::new();
        let mut streaks = HashMap::new();
        for root in roots {
            let members: Vec<usize> = tree(root, procs, &children, &by_pid)
                .into_iter()
                .filter(|i| !leased.contains(&procs[*i].pid))
                .collect();
            let (cores, rss_bytes, _) = summarize(&members, procs);
            if cores < UNBROKERED_MIN_CORES {
                continue;
            }
            let streak = self.streaks.get(&root).copied().unwrap_or(0) + 1;
            streaks.insert(root, streak);
            if streak >= UNBROKERED_MIN_STREAK {
                let top = &procs[by_pid[&root]];
                groups.push(ProcessGroup {
                    root_pid: root,
                    name: describe(top),
                    cores,
                    rss_bytes,
                    processes: members.len() as u32,
                    cwd: top.cwd.clone(),
                });
            }
        }
        self.streaks = streaks;
        groups.sort_by(|a, b| b.cores.total_cmp(&a.cores));
        groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent: Option<u32>, name: &str, cores: f32) -> Proc {
        Proc {
            pid,
            parent,
            name: name.into(),
            cores,
            started: pid as u64,
            ..Default::default()
        }
    }

    fn tool_tree() -> Vec<Proc> {
        vec![
            proc(1, None, "explorer", 0.0),
            proc(2, Some(1), "pwsh", 0.0),
            proc(3, Some(2), "cargo", 0.1),
            proc(4, Some(3), "rustc", 1.0),
            proc(5, Some(3), "rustc", 1.0),
            proc(6, Some(4), "link", 0.5),
        ]
    }

    fn pid_map(procs: &[Proc]) -> HashMap<u32, usize> {
        procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect()
    }

    #[test]
    fn rustc_is_named_by_its_crate() {
        let mut p = proc(1, None, "rustc", 0.0);
        p.cmd = ["rustc", "--edition=2021", "--crate-name", "srm_proto"]
            .map(String::from)
            .to_vec();
        assert_eq!(describe(&p), "rustc srm_proto");
    }

    #[test]
    fn scripts_are_named_past_interpreter_flags() {
        let mut p = proc(1, None, "node", 0.0);
        p.cmd = [
            "node",
            "--max-old-space-size=4096",
            r"C:\x\vite.js",
            "build",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(describe(&p), "node vite.js");
    }

    #[test]
    fn browser_workers_are_named_by_role() {
        let mut p = proc(1, None, "chrome", 0.0);
        p.cmd = ["chrome", "--type=renderer", "--foo"]
            .map(String::from)
            .to_vec();
        assert_eq!(describe(&p), "chrome renderer");
    }

    #[test]
    fn exe_suffix_and_case_are_normalized() {
        assert_eq!(normalize_name("RustC.EXE"), "rustc");
    }

    #[test]
    fn a_tree_collects_every_descendant() {
        let procs = tool_tree();
        let found = tree(3, &procs, &index_children(&procs), &pid_map(&procs));
        let (cores, _, top) = summarize(&found, &procs);
        assert_eq!(found.len(), 4);
        assert!((cores - 2.6).abs() < 1e-6);
        assert_eq!(top[0].cores, 1.0);
    }

    #[test]
    fn a_reused_parent_pid_doesnt_adopt_strangers() {
        let mut procs = tool_tree();
        // PID 3 "started" after its children: the real parent died and the number
        // came back as something unrelated.
        procs[2].started = 100;
        assert_eq!(
            tree(3, &procs, &index_children(&procs), &pid_map(&procs)).len(),
            1
        );
    }

    #[test]
    fn unbrokered_needs_a_sustained_run() {
        let procs = tool_tree();
        let mut tracker = UnbrokeredTracker::default();
        assert!(tracker.update(&procs, &HashSet::new()).is_empty());
        let groups = tracker.update(&procs, &HashSet::new());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].root_pid, 3);
        assert_eq!(groups[0].processes, 4);
    }

    #[test]
    fn work_inside_a_lease_isnt_unbrokered() {
        let procs = tool_tree();
        let leased: HashSet<u32> = [3, 4, 5, 6].into();
        let mut tracker = UnbrokeredTracker::default();
        tracker.update(&procs, &leased);
        assert!(tracker.update(&procs, &leased).is_empty());
    }

    #[test]
    fn a_browser_is_the_person_not_a_build() {
        let procs = vec![proc(1, None, "chrome", 6.0)];
        let mut tracker = UnbrokeredTracker::default();
        tracker.update(&procs, &HashSet::new());
        assert!(tracker.update(&procs, &HashSet::new()).is_empty());
    }

    #[test]
    fn quiet_tools_stay_quiet() {
        let procs = vec![proc(1, None, "node", 0.2)];
        let mut tracker = UnbrokeredTracker::default();
        tracker.update(&procs, &HashSet::new());
        assert!(tracker.update(&procs, &HashSet::new()).is_empty());
    }

    #[test]
    fn a_streak_resets_when_the_tree_goes_quiet() {
        let busy = vec![proc(1, None, "cargo", 2.0)];
        let quiet = vec![proc(1, None, "cargo", 0.0)];
        let mut tracker = UnbrokeredTracker::default();
        tracker.update(&busy, &HashSet::new());
        tracker.update(&quiet, &HashSet::new());
        assert!(tracker.update(&busy, &HashSet::new()).is_empty());
    }
}
