//! Reaching the daemon, and starting it when it isn't there.

use crate::config;
use crate::wire::{read_message, write_message, Request, Response};
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{split, BufReader, ReadHalf, WriteHalf};

#[cfg(windows)]
pub type Stream = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(unix)]
pub type Stream = tokio::net::UnixStream;

#[cfg(windows)]
async fn open(endpoint: &str) -> std::io::Result<Stream> {
    use tokio::net::windows::named_pipe::ClientOptions;
    /// ERROR_PIPE_BUSY: every instance is mid-accept. The server makes a new one
    /// immediately, so this resolves in moments.
    const PIPE_BUSY: i32 = 231;
    for _ in 0..100 {
        match ClientOptions::new().open(endpoint) {
            Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            other => return other,
        }
    }
    ClientOptions::new().open(endpoint)
}

#[cfg(unix)]
async fn open(endpoint: &str) -> std::io::Result<Stream> {
    tokio::net::UnixStream::connect(endpoint).await
}

pub struct Client {
    reader: BufReader<ReadHalf<Stream>>,
    writer: WriteHalf<Stream>,
}

impl Client {
    /// Connect to a running daemon, without starting one.
    pub async fn connect() -> std::io::Result<Self> {
        let (reader, writer) = split(open(&config::endpoint()).await?);
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
        })
    }

    /// Connect, starting the daemon first if it isn't running.
    ///
    /// Any number of clients can race through this: the daemon claims its endpoint
    /// atomically, and the losers exit straight away.
    pub async fn connect_or_spawn() -> Result<Self> {
        if let Ok(client) = Self::connect().await {
            return Ok(client);
        }
        spawn_daemon()?;
        let mut last = None;
        for attempt in 0..60 {
            match Self::connect().await {
                Ok(client) => return Ok(client),
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(25 + attempt * 5)).await;
        }
        Err(anyhow!(
            "srmd did not come up at {}: {}",
            config::endpoint(),
            last.map(|e| e.to_string()).unwrap_or_default()
        ))
    }

    pub async fn send(&mut self, request: &Request) -> std::io::Result<()> {
        write_message(&mut self.writer, request).await
    }

    /// `None` means the daemon closed the connection.
    pub async fn recv(&mut self) -> std::io::Result<Option<Response>> {
        read_message(&mut self.reader).await
    }

    pub async fn call(&mut self, request: &Request) -> Result<Response> {
        self.send(request).await?;
        self.recv()
            .await?
            .context("srmd closed the connection without answering")
    }
}

/// Next to this executable first, so a build tree finds its own daemon; then the
/// install directory; then `PATH`.
fn daemon_path() -> PathBuf {
    let name = format!("srmd{}", std::env::consts::EXE_SUFFIX);
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)));
    if let Some(path) = beside.filter(|p| p.exists()) {
        return path;
    }
    let installed = config::home().join("bin").join(&name);
    if installed.exists() {
        return installed;
    }
    PathBuf::from(name)
}

#[cfg(windows)]
fn spawn_daemon() -> Result<()> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let path = daemon_path();
    let start = |flags: u32| {
        let mut command = std::process::Command::new(&path);
        command
            .creation_flags(flags)
            // A working directory is a lock on Windows. The daemon outlives whoever
            // started it, and must not stop a worktree from being deleted.
            .current_dir(startup_dir())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        std_handles::without_inheritance(|| command.spawn())
    };

    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    // Agents run inside job objects that kill everything when they end, and the
    // daemon must survive the session that happened to start it. Breakaway is
    // refused if the job forbids it, and then being inside the job is still better
    // than not running.
    start(base | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| start(base))
        .with_context(|| format!("starting {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;

    let path = daemon_path();
    std::process::Command::new(&path)
        .current_dir(startup_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {}", path.display()))?;
    Ok(())
}

fn startup_dir() -> PathBuf {
    let home = config::home();
    let _ = std::fs::create_dir_all(&home);
    home
}

#[cfg(windows)]
mod std_handles {
    //! `CreateProcess` inherits *every* inheritable handle, not only the three it is
    //! given. So a daemon started from a piped shell keeps that pipe's write end open
    //! for as long as it lives, and whatever is reading the pipe (an agent's shell
    //! tool, say) waits forever for an end-of-file that never comes. Our standard
    //! handles are the inheritable ones in practice, so hide them for the moment of
    //! the spawn.

    type Handle = *mut core::ffi::c_void;
    const HANDLE_FLAG_INHERIT: u32 = 1;
    const STD_HANDLES: [u32; 3] = [-10i32 as u32, -11i32 as u32, -12i32 as u32];

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetHandleInformation(handle: Handle, flags: *mut u32) -> i32;
        fn SetHandleInformation(handle: Handle, mask: u32, flags: u32) -> i32;
    }

    pub fn without_inheritance<T>(spawn: impl FnOnce() -> T) -> T {
        let mut restore = Vec::new();
        for which in STD_HANDLES {
            // SAFETY: querying and flagging our own standard handles, which stay
            // open for the life of the process. A null or invalid handle just makes
            // these calls fail, and that case is skipped.
            unsafe {
                let handle = GetStdHandle(which);
                let mut flags = 0;
                if GetHandleInformation(handle, &mut flags) != 0
                    && flags & HANDLE_FLAG_INHERIT != 0
                    && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0
                {
                    restore.push(handle);
                }
            }
        }
        let result = spawn();
        for handle in restore {
            // SAFETY: as above; these handles were flagged a moment ago.
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
        }
        result
    }
}
