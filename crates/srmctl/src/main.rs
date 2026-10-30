//! The command line for the system resource manager.

mod job;
mod run;
mod show;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use srm_proto::client::Client;
use srm_proto::config;
use srm_proto::wire::{Request, Response};
use srm_proto::{LeaseRequest, DEFAULT_POOL};

#[derive(Parser)]
#[command(
    version,
    about = "Keep heavy work inside the machine's budget, and see what's using it"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Wait for a lease, then run a command inside it
    Run {
        /// How much of the pool to claim. Match what the command really spends.
        #[arg(long, default_value_t = 1)]
        slots: u32,
        /// Which pool to claim from
        #[arg(long, default_value = DEFAULT_POOL)]
        pool: String,
        #[arg(long)]
        label: Option<String>,
        /// Run below normal priority, so it yields to anything interactive
        #[arg(long)]
        low: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Show what's running and queued
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Keep showing what's running and queued
    Watch,
    /// Compare what work claimed with what it spent
    History {
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Control the daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start it, if it isn't already running
    Start,
    /// Stop it. Running work carries on and rejoins when a daemon returns.
    Stop,
    /// Re-read the config
    Reload,
    /// Say whether it's running
    Status,
}

fn lease_request(
    slots: u32,
    pool: String,
    label: Option<String>,
    low: bool,
    command: &[String],
) -> LeaseRequest {
    LeaseRequest {
        pool,
        amount: slots,
        label: label.unwrap_or_else(|| command.join(" ").chars().take(60).collect()),
        pid: std::process::id(),
        cwd: std::env::current_dir()
            .ok()
            .map(|dir| dir.to_string_lossy().into_owned()),
        session: std::env::var("CLAUDE_CODE_SESSION_ID").ok(),
        low,
    }
}

async fn call(request: &Request) -> Result<Response> {
    let mut client = Client::connect_or_spawn().await?;
    match client.call(request).await? {
        Response::Error { message } => Err(anyhow!(message)),
        response => Ok(response),
    }
}

async fn status(json: bool) -> Result<()> {
    match call(&Request::Status).await? {
        Response::Snapshot { fleet } if json => println!("{}", serde_json::to_string(&fleet)?),
        Response::Snapshot { fleet } => print!("{}", show::fleet(&fleet)),
        other => return Err(anyhow!("unexpected reply: {other:?}")),
    }
    Ok(())
}

async fn watch() -> Result<()> {
    let mut client = Client::connect_or_spawn().await?;
    client.send(&Request::Subscribe).await?;
    while let Some(response) = client.recv().await? {
        if let Response::Snapshot { fleet } = response {
            print!("\x1b[2J\x1b[H{}", show::fleet(&fleet));
        }
    }
    Ok(())
}

async fn history(limit: u32, json: bool) -> Result<()> {
    let request = Request::History { limit: Some(limit) };
    match call(&request).await? {
        Response::History { rows } if json => println!("{}", serde_json::to_string(&rows)?),
        Response::History { rows } => print!("{}", show::history(&rows)),
        other => return Err(anyhow!("unexpected reply: {other:?}")),
    }
    Ok(())
}

async fn daemon(action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start => {
            Client::connect_or_spawn().await?;
            println!("srmd is running at {}", config::endpoint());
        }
        DaemonAction::Stop => match Client::connect().await {
            Ok(mut client) => {
                client.call(&Request::Shutdown).await?;
                println!("srmd stopped");
            }
            Err(_) => println!("srmd wasn't running"),
        },
        DaemonAction::Reload => {
            call(&Request::Reload).await?;
            println!("config reloaded");
        }
        DaemonAction::Status => match Client::connect().await {
            Ok(_) => println!("running at {}", config::endpoint()),
            Err(_) => println!("not running"),
        },
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let code = match Cli::parse().command {
        Command::Run {
            slots,
            pool,
            label,
            low,
            command,
        } => {
            let request = lease_request(slots, pool, label, low, &command);
            run::run(run::Options { request, command }).await?
        }
        Command::Status { json } => status(json).await.map(|()| 0)?,
        Command::Watch => watch().await.map(|()| 0)?,
        Command::History { limit, json } => history(limit, json).await.map(|()| 0)?,
        Command::Daemon { action } => daemon(action).await.map(|()| 0)?,
    };
    std::process::exit(code);
}
