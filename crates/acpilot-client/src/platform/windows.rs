//! Windows transport: loopback HTTP, because the named pipe is not built yet.

/// The URL requests are addressed to, and the client that sends them.
///
/// ADR 0004's Windows endpoint is a named pipe; until that transport exists the
/// daemon listens on loopback (see `acpilot-host`'s platform file), so there is
/// nothing for this client to do differently. A socket path passed here stays a
/// path, and the request fails at connect naming the endpoint — which is the
/// honest failure for a transport this platform does not have.
pub(crate) fn http(endpoint: &str, token: Option<&str>) -> (String, reqwest::Client) {
    if super::is_url(endpoint) {
        return (endpoint.to_owned(), super::tcp_client(token));
    }
    // A path with no socket transport: left as the endpoint so the request fails
    // naming it, rather than being silently redirected to a port.
    (endpoint.to_owned(), reqwest::Client::new())
}

/// A terminal attach over a socket: not a transport this platform has yet.
///
/// The named pipe is ADR 0004's Windows endpoint and is still unbuilt, so a path
/// fails here by name. Failing is the honest answer: a pane that quietly attached
/// to a port the daemon does not listen on would look like a broken terminal.
pub(crate) async fn socket(endpoint: &str) -> std::io::Result<Box<dyn super::Duplex>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("{endpoint}: this platform has no socket transport; attach over http://"),
    ))
}
