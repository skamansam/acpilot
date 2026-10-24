//! The unix listener: a socket in the runtime directory, made private.

use std::io;
use std::path::{Path, PathBuf};

use axum::Router;
use tokio::net::{TcpListener, UnixListener};

use super::Target;

/// The socket `acpilot-host` names as the default endpoint, so a client's default
/// probe and the daemon's default bind are the same string by construction.
pub(crate) fn default_target() -> Target {
    Target::Socket(PathBuf::from(acpilot_host::default_endpoint()))
}

/// Bind `target` and serve until `shutdown` resolves.
///
/// # Errors
///
/// The directory or socket could not be created, a socket path that another
/// daemon is already serving, or the listener failed while serving.
pub async fn serve<F>(router: Router, target: Target, shutdown: F) -> io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match target {
        Target::Tcp(addr) => {
            let listener = TcpListener::bind(&addr).await?;
            tracing::warn!(
                addr = %listener.local_addr()?,
                "listening on TCP; this listener has no token yet, so anything that can reach \
                 the port can use it (ADR 0004 §3)"
            );
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown)
                .await
        }
        Target::Socket(path) => {
            let stale = prepare(&path)?;
            let listener = UnixListener::bind(&path)?;
            // After the bind: the mode is set on the socket that exists, and a
            // bind that fails leaves nothing to widen.
            acpilot_host::private_file(&path)?;
            tracing::info!(
                socket = %path.display(),
                replaced_stale_socket = stale,
                "listening on a unix socket"
            );
            let served = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown)
                .await;
            // The file outlives the process otherwise, and the next start would
            // have to guess whether the socket behind it is alive.
            let _ = std::fs::remove_file(&path);
            served
        }
    }
}

/// Resolve when the daemon should stop: Ctrl-C in a terminal, or `SIGTERM` from a
/// service manager.
///
/// Both, because the difference is visible here: the socket file is removed on the
/// way out, and a `systemctl stop` that left it behind would make the next start
/// look like a crash and the runtime directory look like a graveyard. This is
/// unix-only because `SIGTERM` is.
pub async fn shutdown() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(error) => {
            tracing::error!(%error, "failed to listen for SIGTERM");
            return wait_for_ctrl_c().await;
        }
    };
    tokio::select! {
        _ = wait_for_ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}

async fn wait_for_ctrl_c() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for shutdown signal");
    }
}

/// The runtime directory, the path, and the socket file in that order.
///
/// Returns whether a socket left by a dead daemon was removed. A socket that
/// still answers is refused rather than replaced: two daemons on one path is how
/// a session disappears into a process nobody is looking at.
fn prepare(path: &Path) -> io::Result<bool> {
    if let Some(directory) = path.parent()
        && !directory.as_os_str().is_empty()
    {
        std::fs::create_dir_all(directory)?;
        acpilot_host::private_dir(directory)?;
    }
    if !path.exists() {
        return Ok(false);
    }
    // `connect` is the test: a live daemon accepts, a stale socket file refuses.
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another daemon is already listening on {}; stop it, or pass --socket with \
                 another path",
                path.display()
            ),
        )),
        Err(_) => {
            std::fs::remove_file(path)?;
            Ok(true)
        }
    }
}
