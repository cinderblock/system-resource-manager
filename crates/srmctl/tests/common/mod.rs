//! A private daemon per test, so tests run in parallel and never touch one in real use.
#![allow(dead_code)] // Each test binary uses its own subset.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A suite starts hundreds of console programs. Started from a parent with no console,
/// each would open a window on the desktop of whoever is working at this machine.
pub fn hidden(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

pub struct Env {
    pub home: PathBuf,
    pub endpoint: String,
}

impl Env {
    pub fn new(config: &str) -> Self {
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

    pub fn ctl(&self) -> Command {
        let mut command = hidden(env!("CARGO_BIN_EXE_srmctl"));
        command
            .env("SRM_HOME", &self.home)
            .env("SRM_ENDPOINT", &self.endpoint);
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.ctl().args(args).output().unwrap()
    }

    pub fn start(&self, args: &[&str]) -> Child {
        self.ctl()
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    pub fn status(&self) -> Value {
        let output = self.run(&["status", "--json"]);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    pub fn pool(&self) -> Value {
        self.status()["local"]["pools"][0].clone()
    }

    pub fn wait_for(&self, what: &str, test: impl Fn(&Value) -> bool) -> Value {
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

    pub fn wait_for_pool(&self, what: &str, test: impl Fn(&Value) -> bool) -> Value {
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

