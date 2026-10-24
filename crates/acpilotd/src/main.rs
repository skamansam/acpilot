//! `acpilotd` — the daemon process.
//!
//! The foundation phase (P1): bind, serve health and OpenAPI, and exit cleanly on Ctrl-C. Sockets,
//! single-instance locking and the session routes arrive in the service phase (P3).
//!
//! Two flags are how a profile reaches the daemon today. `--profile` names the
//! directory holding a profile's `config.toml` and `agents.toml`: the `[policy]`
//! table (when it has one — a file without it means the shipped policy rather
//! than none) is the rule set every permission request is answered from, and the
//! agents file, when it exists, is what `GET /v1/agents` lists. `--workspace` is
//! the root the broker compares paths with, because "inside the workspace" is the
//! question the defaults are written in — the daemon serves one workspace until
//! workspaces are first-class in the service phase (P3).
//!
//! Profile *names* and the platform config directory are the profiles phase's P4-2: a name has to
//! resolve the same way for every Acpilot binary, and guessing here would be the
//! second resolver.

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "acpilotd", version, about = "Acpilot daemon")]
struct Cli {
    /// Address to bind. Loopback by default; exposing this needs a token (P3).
    #[arg(long, env = "ACPILOT_ADDR", default_value = "127.0.0.1:7777")]
    addr: String,

    /// Profile directory to load (the one holding `config.toml`).
    ///
    /// Its `[policy]` table becomes the rule set a running broker answers from,
    /// and its `agents.toml`, if present, is the agent list `/v1/agents` serves.
    /// Without this flag the daemon runs the shipped policy and has no profile.
    #[arg(long, value_name = "DIR")]
    profile: Option<PathBuf>,

    /// Workspace root the policy treats as "inside".
    ///
    /// Resolved once at startup, because the broker's inside/outside answer is a
    /// prefix comparison and a root that is itself a symlink would answer it
    /// wrongly for everything under it.
    #[arg(long, value_name = "DIR", default_value = ".")]
    workspace: PathBuf,

    /// Print the OpenAPI document and exit, without binding a socket.
    #[arg(long)]
    print_openapi: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if cli.print_openapi {
        println!("{}", acpilot_proto::openapi_json());
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "acpilotd=info,tower_http=info".into()),
        )
        .init();

    let state = acpilotd::state_for(cli.profile.as_deref(), &cli.workspace)?;

    let listener = tokio::net::TcpListener::bind(&cli.addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(%bound, service = acpilotd::SERVICE, "listening");

    axum::serve(listener, acpilotd::router_with(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

/// Resolve on Ctrl-C so the port is released promptly.
async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for shutdown signal");
    }
    tracing::info!("shutting down");
}
