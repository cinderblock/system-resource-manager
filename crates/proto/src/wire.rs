//! The local protocol: one JSON object per line, in both directions.
//!
//! A deliberately plain format, because the JavaScript client in `cpu-slots.mjs`
//! has to speak it with nothing but `node:net`.
//!
//! **A lease lasts exactly as long as the connection that asked for it.** There is
//! no release message. That is the whole design: a client that exits, crashes or is
//! killed gives its slots back at that instant, with nothing to poll and nothing
//! left behind.

use crate::model::*;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Join the queue. Answered by `Queued` (if it can't start yet) and then
    /// `Granted`.
    Acquire(LeaseRequest),
    /// Re-establish a lease that was already running when the previous daemon went
    /// away. Granted immediately: the work is happening whether or not the budget
    /// has room, and the budget should say so.
    Adopt {
        #[serde(flatten)]
        request: LeaseRequest,
        requested_at: Millis,
        granted_at: Millis,
    },
    /// From a lease holder that knows its exact process set. Sent periodically.
    Report {
        pids: Vec<u32>,
        /// Cumulative, including processes that have already exited.
        cpu_seconds: f64,
    },
    Status,
    /// Answered by a stream of `Snapshot`s until the connection closes.
    Subscribe,
    History {
        #[serde(default)]
        limit: Option<u32>,
    },
    Reload,
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Queued {
        lease: LeaseId,
        amount: u32,
        position: u32,
        free: u32,
        capacity: u32,
        holders: Vec<Lease>,
    },
    Granted {
        lease: LeaseId,
        amount: u32,
        capacity: u32,
        waited_ms: u64,
        granted_at: Millis,
    },
    Snapshot {
        fleet: Box<Fleet>,
    },
    History {
        rows: Vec<HistoryRow>,
    },
    Ok,
    Error {
        message: String,
    },
}

pub async fn write_message<W, T>(writer: &mut W, message: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// `Ok(None)` is a clean end of stream.
pub async fn read_message<R, T>(reader: &mut R) -> std::io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&line)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_acquire_defaults_to_the_cpu_pool() {
        // What the JavaScript client sends. Pinned here because it is the contract
        // with code outside this workspace.
        let raw = r#"{"type":"acquire","amount":2,"label":"x","pid":1}"#;
        match serde_json::from_str::<Request>(raw).unwrap() {
            Request::Acquire(r) => assert_eq!((r.pool.as_str(), r.amount), ("cpu", 2)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn adopt_flattens_the_request() {
        let original = Request::Adopt {
            request: LeaseRequest {
                pool: "cpu".into(),
                amount: 1,
                label: "x".into(),
                pid: 1,
                cwd: None,
                session: None,
                low: false,
            },
            requested_at: 5,
            granted_at: 6,
        };
        let text = serde_json::to_string(&original).unwrap();
        assert!(text.contains(r#""amount":1"#));
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), original);
    }
}
