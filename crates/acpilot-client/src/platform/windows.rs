//! Windows transport: loopback HTTP, because the named pipe is not built yet.

/// The URL requests are addressed to, and the client that sends them.
///
/// ADR 0004's Windows endpoint is a named pipe; until that transport exists the
/// daemon listens on loopback (see `acpilot-host`'s platform file), so there is
/// nothing for this client to do differently. A socket path passed here stays a
/// path, and the request fails at connect naming the endpoint — which is the
/// honest failure for a transport this platform does not have.
pub(crate) fn http(endpoint: &str) -> (String, reqwest::Client) {
    if super::is_url(endpoint) {
        return (endpoint.to_owned(), super::tcp_client());
    }
    // A path with no socket transport: left as the endpoint so the request fails
    // naming it, rather than being silently redirected to a port.
    (endpoint.to_owned(), reqwest::Client::new())
}
