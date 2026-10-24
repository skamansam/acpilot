//! The Windows listener: loopback TCP, until the named pipe exists.

use std::io;
use std::path::Path;

use axum::Router;
use tokio::net::TcpListener;

use super::Target;

/// Loopback HTTP, derived from the endpoint `acpilot-host` names.
///
/// ADR 0004 §3 asks for `\\.\pipe\acpilot-<user>` with an ACL to the owning user.
/// That transport is not built yet (P10-5's named pipe), and binding a pipe this
/// client cannot reach would be worse than binding a socket it can. The API is
/// the same either way; only the address changes.
pub(crate) fn default_target() -> Target {
    Target::Tcp(
        acpilot_host::default_endpoint()
            .trim_start_matches("http://")
            .to_owned(),
    )
}

/// Resolve when the daemon should stop: Ctrl-C, which is what a console window
/// closing sends. Windows has no `SIGTERM` for a process without a console
/// handler, so that half of the unix answer has no equivalent here.
pub async fn shutdown() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for shutdown signal");
    }
    tracing::info!("shutting down");
}

/// Bind `target` and serve until `shutdown` resolves.
///
/// # Errors
///
/// A socket target (no pipe transport yet), an address that cannot be bound, or
/// the listener failing while serving.
pub async fn serve<F>(router: Router, target: Target, shutdown: F) -> io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let addr = match target {
        Target::Tcp(addr) => addr,
        Target::Socket(path) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "a unix socket ({}) is not a Windows transport; the named pipe is P10-5, \
                     so pass --addr",
                    Path::new(&path).display()
                ),
            ));
        }
    };
    let listener = TcpListener::bind(&addr).await?;
    tracing::warn!(
        addr = %listener.local_addr()?,
        "listening on TCP; this listener has no token yet, so anything that can reach the port \
         can use it (ADR 0004 §3)"
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}
