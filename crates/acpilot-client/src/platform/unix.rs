//! Unix transport: an HTTP client that talks over the daemon's socket.

use std::path::Path;

/// The URL requests are addressed to, and the client that sends them.
///
/// `reqwest` 0.13 connects to a unix socket when it is built with one, and the
/// URL is then only what the `Host` header says — nothing in the daemon routes on
/// it. `http://localhost` is used rather than an invented host name so a log line
/// from a proxy or a trace cannot pretend the request went somewhere else.
pub(crate) fn http(endpoint: &str) -> (String, reqwest::Client) {
    if super::is_url(endpoint) {
        return (endpoint.to_owned(), reqwest::Client::new());
    }
    let socket = Path::new(endpoint);
    let client = reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .expect("the HTTP client over a unix socket is always buildable");
    ("http://localhost".to_owned(), client)
}
