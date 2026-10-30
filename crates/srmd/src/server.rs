//! One task per client connection.

use crate::broker::Grant;
use crate::state::{Claim, Shared};
use srm_proto::config::Config;
use srm_proto::wire::{write_message, Request, Response};
use srm_proto::{Fleet, LeaseId};
use std::future::pending;
use std::sync::Arc;
use tokio::io::{split, AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{oneshot, watch};

const HISTORY_DEFAULT_LIMIT: u32 = 50;

fn granted(grant: Grant) -> Response {
    Response::Granted {
        lease: grant.lease,
        amount: grant.amount,
        capacity: grant.capacity,
        waited_ms: grant.waited_ms,
        granted_at: grant.granted_at,
    }
}

async fn next_grant(waiting: &mut Option<oneshot::Receiver<Grant>>) -> Option<Grant> {
    match waiting {
        Some(receiver) => {
            let grant = receiver.await.ok();
            *waiting = None;
            grant
        }
        None => pending().await,
    }
}

async fn next_snapshot(subscription: &mut Option<watch::Receiver<Arc<Fleet>>>) -> Option<Arc<Fleet>> {
    match subscription {
        Some(receiver) => receiver
            .changed()
            .await
            .ok()
            .map(|()| receiver.borrow_and_update().clone()),
        None => pending().await,
    }
}

pub async fn serve<S>(stream: S, shared: Arc<Shared>)
where
    S: AsyncRead + AsyncWrite,
{
    let (reader, mut writer) = split(stream);
    let mut lines = BufReader::new(reader).lines();
    let mut lease: Option<LeaseId> = None;
    let mut waiting = None;
    let mut subscription = None;

    loop {
        let response = tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    // End of stream or a broken pipe: either way the client has gone,
                    // and that is what ends a lease.
                    Ok(None) | Err(_) => break,
                };
                let request = match serde_json::from_str::<Request>(&line) {
                    Ok(request) => request,
                    Err(e) => {
                        let message = format!("bad request: {e}");
                        let _ = write_message(&mut writer, &Response::Error { message }).await;
                        continue;
                    }
                };
                match handle(request, &shared, &mut lease, &mut waiting, &mut subscription).await {
                    Some(response) => response,
                    None => continue,
                }
            }
            grant = next_grant(&mut waiting) => match grant {
                Some(grant) => granted(grant),
                None => continue,
            },
            fleet = next_snapshot(&mut subscription) => match fleet {
                Some(fleet) => Response::Snapshot { fleet: Box::new((*fleet).clone()) },
                None => break,
            },
        };
        if write_message(&mut writer, &response).await.is_err() {
            break;
        }
    }

    if let Some(lease) = lease {
        shared.finish(lease);
    }
}

async fn handle(
    request: Request,
    shared: &Arc<Shared>,
    lease: &mut Option<LeaseId>,
    waiting: &mut Option<oneshot::Receiver<Grant>>,
    subscription: &mut Option<watch::Receiver<Arc<Fleet>>>,
) -> Option<Response> {
    const ONE_LEASE: &str = "a connection holds one lease; open another";
    Some(match request {
        Request::Acquire(_) | Request::Adopt { .. } if lease.is_some() => Response::Error {
            message: ONE_LEASE.into(),
        },
        Request::Acquire(request) => match shared.acquire(request) {
            Err(message) => Response::Error { message },
            Ok(Claim::Granted(grant)) => {
                *lease = Some(grant.lease);
                granted(grant)
            }
            Ok(Claim::Queued {
                lease: id,
                amount,
                position,
                free,
                capacity,
                holders,
                grant,
            }) => {
                *lease = Some(id);
                *waiting = Some(grant);
                Response::Queued {
                    lease: id,
                    amount,
                    position,
                    free,
                    capacity,
                    holders,
                }
            }
        },
        Request::Adopt {
            request,
            requested_at,
            granted_at,
        } => match shared.adopt(request, requested_at, granted_at) {
            Err(message) => Response::Error { message },
            Ok(grant) => {
                *lease = Some(grant.lease);
                granted(grant)
            }
        },
        Request::Report { pids, cpu_seconds } => {
            if let Some(lease) = *lease {
                shared.report(lease, pids, cpu_seconds);
            }
            return None;
        }
        Request::Status => Response::Snapshot {
            fleet: Box::new(shared.fleet_now()),
        },
        Request::Subscribe => {
            let mut receiver = shared.fleet.subscribe();
            receiver.mark_changed();
            *subscription = Some(receiver);
            shared.activity.notify_one();
            return None;
        }
        Request::History { limit } => {
            let limit = limit.unwrap_or(HISTORY_DEFAULT_LIMIT);
            let shared = shared.clone();
            let rows = tokio::task::spawn_blocking(move || shared.history.summary(limit)).await;
            match rows {
                Ok(Ok(rows)) => Response::History { rows },
                Ok(Err(e)) => Response::Error {
                    message: format!("{e:#}"),
                },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::Reload => match Config::load() {
            Ok(config) => {
                shared.reconfigure(&config);
                Response::Ok
            }
            Err(e) => Response::Error {
                message: format!("{e:#}"),
            },
        },
        Request::Shutdown => {
            shared.shutdown.notify_one();
            Response::Ok
        }
    })
}
