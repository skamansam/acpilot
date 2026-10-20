//! `acpilotd` — the daemon process.
//!
//! P1: bind, serve health and OpenAPI, and exit cleanly on Ctrl-C. Sockets,
//! single-instance locking, profiles and the real routes arrive in P3.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "acpilotd", version, about = "Acpilot daemon")]
struct Cli {
    /// Address to bind. Loopback by default; exposing this needs a token (P3).
    #[arg(long, env = "ACPILOT_ADDR", default_value = "127.0.0.1:7777")]
    addr: String,

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

    let listener = tokio::net::TcpListener::bind(&cli.addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(%bound, service = acpilotd::SERVICE, "listening");

    axum::serve(listener, acpilotd::router())
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
