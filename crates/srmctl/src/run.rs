//! `srmctl run`: wait for a lease, run the command, give the lease back by exiting.

use crate::job::{self, Job};
use anyhow::Result;
use srm_proto::client::Client;
use srm_proto::wire::{Request, Response};
use srm_proto::{now_ms, Lease, LeaseRequest, Millis};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::oneshot;

const REPORT_INTERVAL: Duration = Duration::from_secs(2);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);
const FINAL_REPORT_TIMEOUT: Duration = Duration::from_secs(1);
/// Longer than `RECONNECT_INTERVAL`, so running work gets back in first.
const REJOIN_DELAY: Duration = Duration::from_secs(3);

pub struct Options {
    pub request: LeaseRequest,
    pub command: Vec<String>,
}

struct Grant {
    requested_at: Millis,
    granted_at: Millis,
}

struct Held {
    client: Client,
    requested_at: Millis,
    granted_at: Millis,
}

fn describe_wait(amount: u32, pool: &str, free: u32, capacity: u32, holders: &[Lease]) -> String {
    let mut text = format!("srmctl: waiting for {amount} {pool} ({free} of {capacity} free)");
    for holder in holders {
        let age = holder
            .granted_at
            .map(|at| (now_ms() - at) / 1000)
            .unwrap_or(0);
        text.push_str(&format!(
            "\n  held by pid {}  \"{}\"  ({}, {age}s)",
            holder.pid, holder.label, holder.amount
        ));
    }
    text
}

/// Wait for a lease, keeping our place in line across a daemon restart.
///
/// Losing the connection while queued is not the same as having no broker. Treating it
/// that way would run every queued command the moment the daemon restarts, which is the
/// exact pile-up this exists to stop.
async fn acquire(request: &LeaseRequest) -> Result<Held> {
    let requested_at = now_ms();
    let mut rejoining = false;
    loop {
        let mut client = Client::connect_or_spawn().await?;
        if rejoining {
            // Work that was already running is on its way back. Let it be counted first,
            // or this would see an empty pool and start on top of it.
            tokio::time::sleep(REJOIN_DELAY).await;
        }
        if let Some(held) = wait_in_line(&mut client, request, requested_at).await? {
            return Ok(Held {
                client,
                requested_at: held.requested_at,
                granted_at: held.granted_at,
            });
        }
        eprintln!("srmctl: lost srmd while queued; rejoining the queue");
        rejoining = true;
    }
}

/// `None` means the connection dropped before a grant.
async fn wait_in_line(
    client: &mut Client,
    request: &LeaseRequest,
    requested_at: Millis,
) -> Result<Option<Grant>> {
    if client
        .send(&Request::Acquire(request.clone()))
        .await
        .is_err()
    {
        return Ok(None);
    }
    loop {
        match client.recv().await {
            Ok(Some(Response::Queued {
                amount,
                free,
                capacity,
                holders,
                ..
            })) => {
                eprintln!(
                    "{}",
                    describe_wait(amount, &request.pool, free, capacity, &holders)
                );
            }
            Ok(Some(Response::Granted {
                amount,
                capacity,
                waited_ms,
                granted_at,
                ..
            })) => {
                if waited_ms > 1000 {
                    eprintln!(
                        "srmctl: acquired {amount}/{capacity} after {}s",
                        waited_ms / 1000
                    );
                }
                return Ok(Some(Grant {
                    requested_at,
                    granted_at,
                }));
            }
            Ok(Some(Response::Error { message })) => anyhow::bail!("{message}"),
            Ok(Some(other)) => anyhow::bail!("unexpected reply: {other:?}"),
            Ok(None) | Err(_) => return Ok(None),
        }
    }
}

async fn report(client: &mut Client, job: &Option<Arc<Job>>) -> std::io::Result<()> {
    if let Some(usage) = job.as_ref().and_then(|job| job.usage()) {
        client
            .send(&Request::Report {
                pids: usage.pids,
                cpu_seconds: usage.cpu_seconds,
            })
            .await?;
    }
    Ok(())
}

/// Keep the lease for the life of the child.
///
/// The connection is the lease, so a daemon that restarts mid-build would otherwise
/// forget this build is running. The child carries on regardless; this just puts it
/// back in the count as soon as there's a daemon to count it.
async fn hold(
    mut held: Held,
    request: LeaseRequest,
    job: Option<Arc<Job>>,
    mut finished: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut finished => {
                // The tail of a run would otherwise go unmeasured, and a run shorter
                // than the interval would never be measured at all.
                let _ = report(&mut held.client, &job).await;
                return;
            }
            _ = tokio::time::sleep(REPORT_INTERVAL) => {
                if report(&mut held.client, &job).await.is_ok() {
                    continue;
                }
            }
            message = held.client.recv() => {
                if matches!(message, Ok(Some(_))) {
                    continue;
                }
            }
        }
        eprintln!("srmctl: lost srmd; the command keeps running");
        held.client = loop {
            tokio::time::sleep(RECONNECT_INTERVAL).await;
            // Only ever connect. Starting one here would undo `srmctl daemon stop`.
            let Ok(mut client) = Client::connect().await else {
                continue;
            };
            let adopt = Request::Adopt {
                request: request.clone(),
                requested_at: held.requested_at,
                granted_at: held.granted_at,
            };
            if matches!(client.call(&adopt).await, Ok(Response::Granted { .. })) {
                eprintln!("srmctl: back in the budget");
                break client;
            }
        };
    }
}

/// Find what to run. `Command` only looks for `.exe` on `PATH`, but npm and bun
/// install `.cmd` launchers, and those are what most of these commands are.
fn program(command: &[String]) -> Command {
    let mut resolved = match which::which(&command[0]) {
        Ok(path) => Command::new(path),
        // A shell built-in, or a typo that the shell will report better than we can.
        Err(_) if cfg!(windows) => {
            let mut shell = Command::new("cmd.exe");
            shell.args(["/d", "/c", &command[0]]);
            shell
        }
        Err(_) => Command::new(&command[0]),
    };
    resolved.args(&command[1..]);
    resolved
}

pub async fn run(options: Options) -> Result<i32> {
    let Options { request, command } = options;

    let held = match acquire(&request).await {
        Ok(held) => Some(held),
        Err(e) => {
            // A courtesy layer must never be the reason a repo can't build.
            eprintln!("srmctl: no broker ({e:#}); running unbudgeted");
            None
        }
    };

    let (mut child, job) = job::spawn(&mut program(&command), request.low)?;
    let job = job.map(Arc::new);
    let (finished, on_finish) = oneshot::channel();
    let holder = held.map(|held| tokio::spawn(hold(held, request, job, on_finish)));

    // The console delivers Ctrl-C to the child too, so the right response is to stay
    // around for its exit code rather than to die first and orphan it.
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = tokio::signal::ctrl_c() => {}
        }
    };
    if let Some(holder) = holder {
        let _ = finished.send(());
        // Bounded, because a daemon stuck on the far side of the pipe must not hold
        // up the exit of a command that has already finished.
        let _ = tokio::time::timeout(FINAL_REPORT_TIMEOUT, holder).await;
    }
    Ok(job::exit_code(status))
}
