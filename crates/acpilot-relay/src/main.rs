//! Run the self-hosted chat relay.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "acpilot-relay", about = "The Acpilot chat relay")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve groups, membership and presence over HTTP.
    Serve {
        /// Address to bind. The relay serves plain HTTP; TLS is the deployment's job.
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: String,
        /// Data root. One directory per group lives under it.
        #[arg(long, env = "ACPILOT_RELAY_HOME", default_value = "acpilot-relay-data")]
        data: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match Cli::parse().command {
        Command::Serve { bind, data } => {
            let relay = acpilot_relay::Relay::open(&data)?;
            let listener = tokio::net::TcpListener::bind(&bind).await?;
            tracing::info!(address = %listener.local_addr()?, data = %data.display(), "relay listening");
            axum::serve(listener, acpilot_relay::router(relay)).await?;
        }
    }
    Ok(())
}
