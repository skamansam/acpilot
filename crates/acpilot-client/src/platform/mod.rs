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
