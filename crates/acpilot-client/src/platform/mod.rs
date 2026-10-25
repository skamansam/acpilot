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
pub(crate) use unix::http;
#[cfg(windows)]
pub(crate) use windows::http;

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
        .build()
        .expect("the HTTP client is always buildable")
}
