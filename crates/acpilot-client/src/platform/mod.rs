//! How this client reaches the daemon, per platform.
//!
//! An endpoint is either a URL or a filesystem path, and what a filesystem path
//! means is the one thing that differs: on unix it is a socket the HTTP client
//! connects to; on Windows there is no unix socket at this pin, so the default
//! endpoint there is loopback HTTP and a path is a mistake that fails at connect.
//!
//! The branch lives in this directory, not in `lib.rs`, for the same reason the
//! host crate's does: a `cfg(windows)` in the middle of the client compiles on one
//! OS and silently never runs on the other. `acpilot-host` owns the platform rules
//! that both sides share (the default endpoint, the runtime directory); this file
//! owns only the transport this crate builds with them.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::{http, socket};
#[cfg(windows)]
pub(crate) use windows::{http, socket};

// The type a terminal attach hands the WebSocket handshake. Declared in
// `terminal` because that is where the frames live; named here because opening
// the stream is the platform's half.
pub(crate) use crate::terminal::Duplex;

/// How long a TCP connection to the daemon may take before it counts as a failure.
///
/// A daemon on this machine either answers at once or refuses, and a refusal is
/// immediate. The case this bounds is the other one: a host that drops the SYN
/// instead of rejecting it — a firewall, a wrong address on a network that does
/// not answer — where the kernel waits out its own retry schedule and the caller
/// waits with it. Two minutes of a pane with nothing in it is indistinguishable
/// from a hung editor, and the endpoint was wrong all along. A local handshake
/// that has not finished in two seconds is not going to.
pub(crate) const CONNECT: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether an endpoint names a URL rather than a socket path.
pub(crate) fn is_url(endpoint: &str) -> bool {
    endpoint.starts_with("http://") || endpoint.starts_with("https://")
}

/// An HTTP client that carries the token it was given, if any.
///
/// Only used for TCP: a socket's access control is the filesystem, and sending a
/// token down it would be a secret in a channel that never needed one. The token
/// is a parameter rather than something this module looks up — an environment
/// variable read here would make the same `Client::new` mean two things depending
/// on the process it runs in, and a library is the wrong place to decide where a
/// secret comes from. `acpilot` resolves one and passes it.
pub(crate) fn tcp_client(token: Option<&str>) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = token
        && let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
    {
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        // Bounded for the same reason the terminal attach is: an endpoint that
        // drops packets must fail with a message, not wait out the kernel.
        .connect_timeout(CONNECT)
        .build()
        .expect("the HTTP client is always buildable")
}
