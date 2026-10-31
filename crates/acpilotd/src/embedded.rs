//! The daemon inside the client's own process (P6-11).
//!
//! `acpilot tui` on a machine with no daemon used to start one — a detached
//! process with its own log, lock and shutdown story. Embedded mode is the other
//! answer: the same router, the same auth middleware and the same wire types,
//! listening on a loopback port this process chose, so the client that opens a
//! session is the process that owns it. Nothing here re-implements the API; the
//! only thing that differs is who is serving it.
//!
//! Two consequences are deliberate. The port is ephemeral (`127.0.0.1:0`), because
//! an embedded daemon is not a service anyone else is meant to find; and the token
//! is generated here rather than read from the runtime directory, because a secret
//! on disk outlives the process it guarded and would be one more thing a second
//! client could use to reach a daemon it did not start. A client that wants its
//! sessions to survive closing the window runs a real `acpilotd`.

use std::io;

use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::AppState;

/// A daemon listening on a loopback port, for the client that embedded it.
///
/// Dropping it asks the listener to stop, which is what the end of the client
/// means: unlike a spawned daemon there is nothing left for it to outlive.
#[derive(Debug)]
pub struct Embedded {
    endpoint: String,
    token: String,
    stop: Option<oneshot::Sender<()>>,
}

impl Embedded {
    /// Serve `state` on `127.0.0.1:0`, with a token nothing else knows.
    ///
    /// The listener is bound before this returns, so a client can connect the
    /// moment it has the endpoint — there is no readiness race to wait out, and no
    /// need to be inside a runtime to call this. The server runs on a thread of its
    /// own for the same reason the TUI's client worker does: it was launched by
    /// something (a launch helper's temporary runtime) that does not live as long
    /// as the thing it started.
    ///
    /// # Errors
    ///
    /// The port could not be bound, the thread could not be started, or the
    /// system's random source refused.
    pub fn start(state: AppState) -> io::Result<Self> {
        // Bound synchronously so the port is known before the thread exists: a bind
        // inside the spawned task could not report the address it got without a
        // second channel for one fact.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let address = listener.local_addr()?;
        // Tokio's listener requires it, and setting it here keeps the option from
        // being a thing the async half has to remember.
        listener.set_nonblocking(true)?;
        let token = acpilot_host::token::generate()?;
        let (stop, stopped) = oneshot::channel();
        let router = crate::auth::protect_with_token(crate::router_with(state), token.clone());

        std::thread::Builder::new()
            .name("acpilotd-embedded".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(%error, "no runtime for the embedded listener");
                        return;
                    }
                };
                let served = runtime.block_on(async move {
                    let listener = TcpListener::from_std(listener)?;
                    axum::serve(listener, router)
                        .with_graceful_shutdown(async move {
                            let _ = stopped.await;
                        })
                        .await
                });
                if let Err(error) = served {
                    tracing::error!(%error, "the embedded listener stopped");
                }
            })?;

        Ok(Self {
            endpoint: format!("http://{address}"),
            token,
            stop: Some(stop),
        })
    }

    /// The URL a client connects to, as `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The token every request to [`Self::endpoint`] must carry.
    ///
    /// Not written to disk and not the runtime directory's token: this is the one
    /// secret the process that embedded the daemon is given and nobody else is.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        // A signal, not a join: dropping happens on the way out of a client, and
        // waiting for a graceful shutdown there would be the client waiting on
        // streams it is in the middle of closing. The listener stops accepting
        // immediately; the thread ends when the process does.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
