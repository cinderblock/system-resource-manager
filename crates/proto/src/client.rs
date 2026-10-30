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
        // A healthy start takes milliseconds. Waiting long is only ever waiting for a
        // daemon that isn't coming, and that delays the real work.
        for _ in 0..60 {
            match Self::connect().await {
                Ok(client) => return Ok(client),
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
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

/// Started by hand rather than with `Command`, because `Command` always asks for handle
/// inheritance, and `CreateProcess` then passes on *every* inheritable handle in the
/// process, not only the three it's given. A pipe reaches us that way without being our
/// stdout at all: a shell's pipe goes to node as stdout and on to us as a plain inherited
/// handle. Handing it to the daemon keeps it open for the daemon's whole life, so whatever
/// is reading that pipe (an agent's shell tool, say) waits forever for an end of file.
#[cfg(windows)]
fn spawn_daemon() -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
        PROCESS_INFORMATION, STARTUPINFOW,
    };

    let path = daemon_path();
    let directory: Vec<u16> = startup_dir()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();

    let start = |flags: u32| -> std::io::Result<()> {
        let mut command: Vec<u16> = "\""
            .encode_utf16()
            .chain(path.as_os_str().encode_wide())
            .chain("\"".encode_utf16())
            .chain(Some(0))
            .collect();
        // SAFETY: all-zero is valid for both structs once `cb` is set.
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the strings are NUL-terminated and outlive the call, the command buffer
        // is writable as the API requires, and the out-struct is valid. Both returned
        // handles are closed below.
        let ok = unsafe {
            CreateProcessW(
                null(),
                command.as_mut_ptr(),
                null(),
                null(),
                0, // bInheritHandles: the point of doing this by hand.
                flags,
                null(),
                directory.as_ptr(),
                &startup,
                &mut process,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both are valid after a successful create, and aren't used again.
        unsafe {
            CloseHandle(process.hThread);
            CloseHandle(process.hProcess);
        }
        Ok(())
    };

    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    // Agents run inside job objects that kill everything when they end, and the daemon
    // must survive the session that happened to start it. Breakaway is refused if the job
    // forbids it, and then being inside the job is still better than not running.
    start(base | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| start(base))
        .with_context(|| format!("starting {}", path.display()))
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

/// A working directory is a lock on Windows. The daemon outlives whoever started it, and
/// must not stop a worktree from being deleted.
fn startup_dir() -> PathBuf {
    let home = config::home();
    let _ = std::fs::create_dir_all(&home);
    home
}
