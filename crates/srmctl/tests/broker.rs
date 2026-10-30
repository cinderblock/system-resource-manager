//! The whole local stack against a real, private daemon.
//!
//! Each test gets its own `SRM_HOME` and endpoint, so these run in parallel and never
//! touch a daemon that's in real use. Needs `srmd` built alongside `srmctl`, which
//! `cargo test --workspace` does. Uses `node` as a portable child process.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct Env {
    home: PathBuf,
    endpoint: String,
}

impl Env {
    fn new(config: &str) -> Self {
        let id = format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let home = std::env::temp_dir().join(format!("srm-test-{id}"));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), config).unwrap();
        let endpoint = if cfg!(windows) {
            format!(r"\\.\pipe\srmd-test-{id}")
        } else {
            home.join("srmd.sock").to_string_lossy().into_owned()
        };
        Self { home, endpoint }
    }

    fn ctl(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_srmctl"));
        command
            .env("SRM_HOME", &self.home)
            .env("SRM_ENDPOINT", &self.endpoint);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.ctl().args(args).output().unwrap()
    }

    fn start(&self, args: &[&str]) -> Child {
        self.ctl()
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn status(&self) -> Value {
        let output = self.run(&["status", "--json"]);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn pool(&self) -> Value {
        self.status()["local"]["pools"][0].clone()
    }

    fn wait_for(&self, what: &str, test: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = self.status();
            if test(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "never saw {what}: {status:#}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_for_pool(&self, what: &str, test: impl Fn(&Value) -> bool) -> Value {
        self.wait_for(what, |s| test(&s["local"]["pools"][0]))["local"]["pools"][0].clone()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
        // The daemon may hold the log open for a moment after answering.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.home).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Long enough that a loaded machine can't finish it before a test has looked.
const SLEEP: &str = "setTimeout(()=>{},15000)";

fn capacity(n: u32) -> String {
    format!("[resources.cpu]\ncapacity = {n}\n")
}

#[test]
fn starting_the_daemon_doesnt_hold_our_pipes_open() {
    // `output()` reads to end of file. If the daemon inherited our stdout, this would
    // never return, which is exactly how an agent's shell tool would hang.
    let env = Env::new("");
    let mut child = env
        .ctl()
        .args(["run", "--", "node", "-e", "console.log('hello')"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut text).unwrap();
        text
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    while !reader.is_finished() {
        assert!(Instant::now() < deadline, "stdout never closed");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(reader.join().unwrap().trim(), "hello");
    child.wait().unwrap();
}

#[test]
fn the_exit_code_comes_through() {
    let env = Env::new("");
    let output = env.run(&["run", "--", "node", "-e", "process.exit(7)"]);
    assert_eq!(output.status.code(), Some(7));
}

#[test]
fn arguments_survive_intact() {
    let env = Env::new("");
    let script = "console.log(JSON.stringify(process.argv.slice(1)))";
    let output = env.run(&["run", "--", "node", "-e", script, "a b", r#"c"d"#]);
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed, serde_json::json!(["a b", r#"c"d"#]));
}

#[cfg(windows)]
#[test]
fn cmd_launchers_run() {
    // npm and bun install `.cmd` shims, which need help to start.
    let env = Env::new("");
    let dir = env.home.join("shims");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("fake-tool.cmd"),
        "@echo off\r\necho shim %1\r\nexit /b 3\r\n",
    )
    .unwrap();
    let path = format!(
        "{};{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = env
        .ctl()
        .env("PATH", path)
        .args(["run", "--", "fake-tool", "arg"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "shim arg");
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn an_unstartable_command_is_a_failure_not_a_hang() {
    let env = Env::new("");
    let output = env.run(&["run", "--", "this-program-does-not-exist-anywhere"]);
    assert!(!output.status.success());
}

#[test]
fn a_full_pool_queues_and_says_who_holds_it() {
    let env = Env::new(&capacity(2));
    let mut holder = env.start(&[
        "run", "--slots", "2", "--label", "holder", "--", "node", "-e", SLEEP,
    ]);
    env.wait_for_pool("the holder", |p| p["used"] == 2);

    let waiter = env
        .ctl()
        .args(["run", "--label", "waiter", "--", "node", "-e", ""])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let pool = env.wait_for_pool("the queue", |p| p["waiters"].as_array().unwrap().len() == 1);
    assert_eq!(pool["waiters"][0]["label"], "waiter");

    let output = waiter.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("waiting for 1 cpu (0 of 2 free)"), "{said}");
    assert!(said.contains("\"holder\""), "{said}");
    assert!(output.status.success());
    holder.wait().unwrap();
}

#[test]
fn killing_a_holder_frees_its_slots_at_once() {
    let env = Env::new(&capacity(1));
    let mut holder = env.start(&["run", "--slots", "1", "--", "node", "-e", SLEEP]);
    env.wait_for_pool("the holder", |p| p["used"] == 1);

    holder.kill().unwrap();
    holder.wait().unwrap();

    let killed_at = Instant::now();
    env.wait_for_pool("the slot back", |p| p["used"] == 0);
    assert!(killed_at.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_stampede_of_clients_shares_one_daemon() {
    let env = Env::new(&capacity(4));
    let script = "setTimeout(()=>{},500)";
    let children: Vec<_> = (0..8)
        .map(|_| env.start(&["run", "--", "node", "-e", script]))
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let status = env.status();
    assert_eq!(status["local"]["pools"][0]["used"], 0);
    // Every winner of the race would have logged a start.
    let log = std::fs::read_to_string(env.home.join("srmd.log")).unwrap();
    assert_eq!(log.matches("starting").count(), 1, "{log}");
}

#[test]
fn work_survives_a_daemon_restart_and_rejoins_the_count() {
    let env = Env::new(&capacity(2));
    let mut holder = env.start(&[
        "run",
        "--slots",
        "2",
        "--label",
        "survivor",
        "--",
        "node",
        "-e",
        "setTimeout(()=>{},9000)",
    ]);
    env.wait_for_pool("the holder", |p| p["used"] == 2);

    env.run(&["daemon", "stop"]);

    // Asking for status starts a fresh daemon, which knows nothing until the client
    // comes back.
    let pool = env.wait_for_pool("the lease adopted", |p| p["used"] == 2);
    assert_eq!(pool["holders"][0]["label"], "survivor");
    assert!(holder.wait().unwrap().success());
}

#[test]
fn low_priority_reaches_grandchildren() {
    let env = Env::new("");
    // Node reports a process's priority class as a nice value: 10 is below normal.
    let grandchild = "require('os').getPriority()";
    let script = format!(
        r#"const out=require('child_process').execFileSync('node',['-p',"{grandchild}"]);console.log(String(out).trim())"#
    );
    let output = env.run(&["run", "--low", "--", "node", "-e", &script]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "10",
        "{output:?}"
    );
}

#[test]
fn normal_runs_keep_the_priority_they_inherited() {
    let env = Env::new("");
    let probe = ["-p", "require('os').getPriority()"];
    // Relative to this test's own priority, which may itself have been lowered.
    let inherited = Command::new("node").args(probe).output().unwrap();
    let output = env.run(&["run", "--", "node", probe[0], probe[1]]);
    assert_eq!(output.stdout, inherited.stdout);
}

#[test]
fn spending_is_measured_and_kept() {
    let env = Env::new(&capacity(2));
    // Burn a fixed amount of CPU time rather than wall time, so the expected total
    // doesn't depend on what else the machine is doing.
    let burn = "for(;;){const u=process.cpuUsage();if(u.user+u.system>2e6)break}";
    let output = env.run(&[
        "run", "--slots", "2", "--label", "burn", "--", "node", "-e", burn,
    ]);
    assert!(output.status.success());

    let deadline = Instant::now() + Duration::from_secs(10);
    let rows = loop {
        let output = env.run(&["history", "--json"]);
        let rows: Value = serde_json::from_slice(&output.stdout).unwrap();
        if !rows.as_array().unwrap().is_empty() {
            break rows;
        }
        assert!(Instant::now() < deadline, "nothing recorded");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(rows[0]["label"], "burn");
    assert_eq!(rows[0]["avg_amount"], 2.0);
    let held = rows[0]["avg_held_ms"].as_f64().unwrap() / 1000.0;
    let spent = rows[0]["avg_cores"].as_f64().unwrap() * held;
    assert!(
        (1.9..3.5).contains(&spent),
        "2 s of CPU measured as {spent}"
    );
}

#[test]
fn a_running_lease_shows_its_live_process_usage() {
    let env = Env::new("");
    let burn = "for(;;){const u=process.cpuUsage();if(u.user+u.system>6e6)break}";
    let mut holder = env.start(&["run", "--label", "burn", "--", "node", "-e", burn]);
    let pool = env.wait_for_pool("usage", |p| {
        p["holders"][0]["usage"]["cores"].as_f64().unwrap_or(0.0) > 0.05
    });
    let usage = &pool["holders"][0]["usage"];
    assert!(usage["top"][0]["name"]
        .as_str()
        .unwrap()
        .starts_with("node"));
    holder.wait().unwrap();
}

#[test]
fn claiming_from_an_unknown_pool_warns_and_runs_anyway() {
    let env = Env::new("");
    let output = env.run(&[
        "run",
        "--pool",
        "nope",
        "--",
        "node",
        "-e",
        "console.log('ran')",
    ]);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ran");
    assert!(String::from_utf8_lossy(&output.stderr).contains("no pool named"));
}

#[test]
fn reload_applies_a_new_capacity_to_waiters() {
    let env = Env::new(&capacity(1));
    let mut holder = env.start(&["run", "--", "node", "-e", SLEEP]);
    env.wait_for_pool("the holder", |p| p["used"] == 1);
    let mut waiter = env.start(&["run", "--", "node", "-e", SLEEP]);
    env.wait_for_pool("the queue", |p| p["waiters"].as_array().unwrap().len() == 1);

    write_config(&env.home, &capacity(2));
    assert!(env.run(&["daemon", "reload"]).status.success());

    let pool = env.wait_for_pool("the waiter let in", |p| p["used"] == 2);
    assert_eq!(pool["capacity"], 2);
    holder.kill().unwrap();
    waiter.kill().unwrap();
    holder.wait().unwrap();
    waiter.wait().unwrap();
}

fn write_config(home: &Path, text: &str) {
    std::fs::write(home.join("config.toml"), text).unwrap();
}

#[test]
fn no_daemon_binary_still_runs_the_command() {
    // SRM_HOME and the daemon's location are unrelated here: the real test of "can't
    // reach a broker" is an endpoint nothing can ever bind.
    let env = Env::new("");
    let output = env
        .ctl()
        .env(
            "SRM_ENDPOINT",
            if cfg!(windows) {
                r"\\.\nope\srmd"
            } else {
                "/nonexistent/srmd.sock"
            },
        )
        .args(["run", "--", "node", "-e", "console.log('ran')"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ran");
    assert!(String::from_utf8_lossy(&output.stderr).contains("running unbudgeted"));
}
