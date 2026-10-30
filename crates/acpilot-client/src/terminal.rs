//! Attaching to a terminal: the pane's side of `WS /v1/terminals/{id}/attach`.
//!
//! The route is the one place in this API that is not request/response, and the
//! two directions are deliberately different frame types: binary frames are the
//! terminal itself — keystrokes one way, screen bytes the other — and text frames
//! are control, because a shell can print `{"type":…}` and a pane must not
//! mistake its own output for an instruction.
//!
//! # Why this is a socket and not a stream of events
//!
//! Keystrokes have to arrive while the program is waiting for them, and a poll of
//! the screen would either miss bytes between two reads or repeat them. The daemon
//! sends the screen it has as one binary frame after `attached`, then every chunk
//! as it is produced, and this client is the reader those frames were shaped for.
//!
//! # What is not here
//!
//! TLS. The daemon has no TLS listener: it is a unix socket, or loopback HTTP
//! behind a token. A `https://` endpoint is refused by name rather than silently
//! spoken to in the clear.

use acpilot_proto::terminal::{TerminalControl, TerminalNotice, TerminalSize, TerminalSummary};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, CONNECTION, HOST, UPGRADE};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{WebSocketStream, client_async};

use crate::ClientError;
use crate::platform;

/// A byte stream the handshake can be spoken over.
///
/// A unix socket and a TCP connection are different types and only one of them
/// exists per platform, so the attach wraps whichever it has. The trait is the
/// seam: everything after the handshake is the same code.
pub(crate) trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// One frame from an attached terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// The attach succeeded. The next [`TerminalEvent::Data`] is exactly
    /// `replay_bytes` long and draws the screen as it stands.
    Attached {
        /// The terminal, as the daemon sees it now.
        terminal: TerminalSummary,
        /// Length of the replay frame that follows.
        replay_bytes: usize,
    },
    /// Screen bytes: the replay first, then every chunk as it is produced.
    Data(Vec<u8>),
    /// The program ended. The daemon closes the socket after this frame.
    Exit {
        /// Exit code, when the host reported one.
        code: Option<i32>,
    },
}

/// A live attachment to one terminal.
///
/// Dropping it closes the socket, which leaves the process running: a pane is a
/// view, not an owner. Nothing here kills anything — that is the agent's
/// `terminal/kill` or the daemon's idle sweep.
pub struct Terminal {
    socket: WebSocketStream<Box<dyn Duplex>>,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Terminal").finish_non_exhaustive()
    }
}

impl Terminal {
    /// Attach to terminal `id` at `endpoint` — a daemon URL, or a socket path.
    ///
    /// `token` is for a TCP endpoint only, exactly as it is for the REST client:
    /// a socket's access control is the filesystem, and a secret sent down it
    /// would buy nothing.
    ///
    /// # Errors
    ///
    /// A refused connection, a handshake the daemon does not accept (an unknown
    /// terminal is a `404` before the upgrade), or an `https://` endpoint, which
    /// this daemon does not serve.
    pub async fn open(endpoint: &str, token: Option<&str>, id: &str) -> Result<Self, ClientError> {
        let base = endpoint.trim_end_matches('/');
        let request = attach_request(base, token, id)?;
        let stream = connect(base, &request).await?;
        let (socket, _response) = client_async(request, stream)
            .await
            .map_err(|error| ClientError::Terminal(format!("{base}: {error}")))?;
        Ok(Self { socket })
    }

    /// The next frame, or [`None`] when the daemon closes the socket.
    ///
    /// # Errors
    ///
    /// The connection broke. A clean close — which is how an attach ends when
    /// the program does — is [`None`], not an error.
    pub async fn next(&mut self) -> Result<Option<TerminalEvent>, ClientError> {
        loop {
            let message = match self.socket.next().await {
                Some(Ok(message)) => message,
                Some(Err(error)) => {
                    return Err(ClientError::Terminal(format!("terminal socket: {error}")));
                }
                None => return Ok(None),
            };
            match message {
                Message::Binary(bytes) => return Ok(Some(TerminalEvent::Data(bytes.into()))),
                Message::Text(text) => {
                    // A frame this client does not know is a newer daemon's
                    // notice; the pane has no use for it, and guessing would be
                    // worse than skipping it.
                    match serde_json::from_str::<TerminalNotice>(&text) {
                        Ok(TerminalNotice::Attached {
                            terminal,
                            replay_bytes,
                        }) => {
                            return Ok(Some(TerminalEvent::Attached {
                                terminal,
                                replay_bytes,
                            }));
                        }
                        Ok(TerminalNotice::Exit { code }) => {
                            return Ok(Some(TerminalEvent::Exit { code }));
                        }
                        Err(_) => continue,
                    }
                }
                Message::Close(_) => return Ok(None),
                // Ping, pong and raw frames are the transport's: tungstenite
                // answers pings as it reads them.
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            }
        }
    }

    /// Send bytes as if typed. No newline is added: `text` and `text\r` differ.
    ///
    /// # Errors
    ///
    /// The socket closed, which in practice means the program is gone.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), ClientError> {
        self.socket
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .map_err(|error| ClientError::Terminal(format!("terminal socket: {error}")))
    }

    /// Tell the program its pane changed shape, so it can redraw.
    ///
    /// # Errors
    ///
    /// The socket closed, or the daemon refused the size (a zero dimension is
    /// not a terminal).
    pub async fn resize(&mut self, size: TerminalSize) -> Result<(), ClientError> {
        let control = TerminalControl::Resize {
            cols: size.cols,
            rows: size.rows,
        };
        let text = serde_json::to_string(&control).expect("a control frame is a plain struct");
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|error| ClientError::Terminal(format!("terminal socket: {error}")))
    }
}

/// The upgrade request for `id` at `base`.
///
/// The body is `()` because that is the shape tungstenite's handshake takes: the
/// headers are the whole request.
fn attach_request(base: &str, token: Option<&str>, id: &str) -> Result<Request<()>, ClientError> {
    let (uri, host) = attach_url(base, id)?;
    let mut request = Request::builder()
        .method("GET")
        .uri(&uri)
        .header(HOST, &host)
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header("Sec-WebSocket-Version", "13")
        // The handshake will not start without a key in the request.
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        );
    // A socket needs no token; sending one would be a secret in a channel whose
    // access control is already the filesystem.
    if platform::is_url(base)
        && let Some(token) = token
    {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    request
        .body(())
        .map_err(|error| ClientError::Terminal(format!("{uri}: {error}")))
}

/// The `ws://` URL to ask for, and the `Host` header to say it with.
fn attach_url(base: &str, id: &str) -> Result<(String, String), ClientError> {
    if !platform::is_url(base) {
        // A socket: the URL is only what the `Host` header says, as it is for
        // every HTTP call over the same transport.
        return Ok((
            format!("ws://localhost/v1/terminals/{id}/attach"),
            "localhost".to_owned(),
        ));
    }
    if let Some(rest) = base.strip_prefix("http://") {
        return Ok((
            format!("ws://{rest}/v1/terminals/{id}/attach"),
            rest.split('/').next().unwrap_or(rest).to_owned(),
        ));
    }
    Err(ClientError::Terminal(format!(
        "{base}: a terminal attach needs http:// or a socket path; this daemon serves no TLS"
    )))
}

/// Open the byte stream the handshake is spoken over.
async fn connect(base: &str, request: &Request<()>) -> Result<Box<dyn Duplex>, ClientError> {
    if !platform::is_url(base) {
        return platform::socket(base)
            .await
            .map_err(|error| ClientError::Terminal(format!("{base}: {error}")));
    }
    let uri = request.uri();
    let host = uri.host().unwrap_or("localhost").to_owned();
    let port = uri.port_u16().unwrap_or(80);
    // Bounded, because "nothing is listening" has two shapes. A closed port
    // refuses at once; a port behind a firewall that drops the SYN leaves the
    // kernel retrying for over two minutes, and a pane that is merely empty for
    // that long reads as a hung editor rather than a wrong endpoint.
    let stream = tokio::time::timeout(
        platform::CONNECT,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| {
        ClientError::Terminal(format!(
            "{base}: nothing answered within {:?}",
            platform::CONNECT
        ))
    })?
    .map_err(|error| ClientError::Terminal(format!("{base}: {error}")))?;
    Ok(Box::new(stream))
}

impl Terminal {
    /// The request this client would send, for a test that checks the handshake
    /// without a daemon.
    #[cfg(test)]
    fn request(endpoint: &str, token: Option<&str>, id: &str) -> Request<()> {
        attach_request(endpoint, token, id).expect("a request can be built")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_attach_addresses_localhost_and_carries_no_token() {
        let request = Terminal::request("/run/acpilot.sock", Some("secret"), "t1");
        assert_eq!(
            request.uri().to_string(),
            "ws://localhost/v1/terminals/t1/attach"
        );
        assert_eq!(request.headers()[HOST], "localhost");
        assert!(
            !request.headers().contains_key(AUTHORIZATION),
            "a socket's access control is the filesystem"
        );
        assert!(request.headers().contains_key("Sec-WebSocket-Key"));
    }

    #[test]
    fn a_tcp_attach_keeps_the_host_and_sends_the_token() {
        let request = Terminal::request("http://127.0.0.1:7777", Some("secret"), "agent-x");
        assert_eq!(
            request.uri().to_string(),
            "ws://127.0.0.1:7777/v1/terminals/agent-x/attach"
        );
        assert_eq!(request.headers()[HOST], "127.0.0.1:7777");
        assert_eq!(request.headers()[AUTHORIZATION], "Bearer secret");
    }

    #[test]
    fn an_https_endpoint_is_refused_rather_than_spoken_to_in_the_clear() {
        let error = attach_request("https://box:7443", None, "t1").expect_err("refused");
        assert!(
            error.to_string().contains("no TLS"),
            "the error says why: {error}"
        );
    }
}
