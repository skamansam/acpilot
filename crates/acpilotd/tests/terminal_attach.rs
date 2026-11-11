#![cfg(unix)]

//! `WS /v1/terminals/{id}/attach` opened the way a browser opens it.
//!
//! A browser's socket constructor takes a URL and a protocol list, and sets no
//! headers — so a page cannot send `Authorization`, and the token has to travel as
//! a subprotocol. That is a header, not a query string, which is why the page never
//! spells the token into a URL. This is the exact handshake the web terminal makes,
//! against a real listener with the real auth layer: offer `["acpilot.v1", token]`,
//! expect `acpilot.v1` back and the pane's screen behind it.
//!
//! `oneshot` on the router would never run the upgrade, so the client half here is
//! `tokio-tungstenite`, the same as the session stream's test.

use acpilot_host::ExecCommand;
use acpilot_proto::terminal::{TerminalNotice, TerminalSize};
use acpilotd::{AppState, Terminal, router_with};
use futures::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header;

const TOKEN: &str = "8f5b9d0c2a7e41f3b6c8d9e0f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4";

/// A token-guarded daemon with one `/bin/sh` in it, and its address on this
/// machine.
///
/// The two directories are returned rather than dropped: the auth layer reads the
/// token file per request and the pane's `cwd` is the workspace, so either one
/// going away would leave the test measuring a different daemon.
async fn serve() -> (tempfile::TempDir, tempfile::TempDir, std::net::SocketAddr) {
    let root = tempfile::tempdir().expect("workspace");
    let secret = tempfile::tempdir().expect("token directory");
    let path = secret.path().join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write");

    let workspace = acpilotd::workspace_at(root.path()).expect("workspace");
    let state = AppState::new().with_workspace(workspace);
    let terminal = Terminal::spawn(
        "t1".to_owned(),
        Some("s1".to_owned()),
        ExecCommand::new("/bin/sh"),
        TerminalSize { cols: 80, rows: 24 },
    )
    .expect("a pty");
    state.terminals().insert(terminal);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = acpilotd::protect(router_with(state), &path);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (root, secret, address)
}

/// Open the attach socket with `protocols` as the browser would.
async fn attach(
    address: std::net::SocketAddr,
    protocols: &str,
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    tokio_tungstenite::tungstenite::Error,
> {
    let mut request = format!("ws://{address}/v1/terminals/t1/attach")
        .into_client_request()
        .expect("request");
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        protocols.parse().expect("a header"),
    );
    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    tokio_tungstenite::client_async(request, stream).await
}

#[tokio::test]
async fn a_browser_that_offers_the_token_as_a_protocol_gets_the_pane() {
    let (_root, _secret, address) = serve().await;

    let (mut socket, response) = attach(address, &format!("acpilot.v1, {TOKEN}"))
        .await
        .expect("the handshake completes");

    // The daemon names the protocol it chose; a browser refuses the connection when
    // it offered protocols and the server names none. It must not be the token.
    assert_eq!(
        response
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok()),
        Some("acpilot.v1"),
        "the answer names the protocol, never the secret"
    );

    let frame = socket.next().await.expect("a frame").expect("a good frame");
    let Message::Text(text) = frame else {
        panic!("the first frame is the attach notice, got {frame:?}");
    };
    let notice: TerminalNotice = serde_json::from_str(&text).expect("a notice");
    match notice {
        TerminalNotice::Attached {
            terminal,
            replay_bytes,
        } => {
            assert_eq!(terminal.id, "t1");
            assert_eq!(terminal.session_id.as_deref(), Some("s1"));
            assert!(
                replay_bytes < 1_000_000,
                "a screen replay is a screen, not a scrollback dump"
            );
        }
        TerminalNotice::Exit { .. } => panic!("a live shell reported an exit on attach"),
    }

    // And the screen follows as bytes, which is what `term.write` receives.
    let replay = socket
        .next()
        .await
        .expect("the replay frame")
        .expect("a good frame");
    assert!(
        matches!(replay, Message::Binary(_)),
        "the replay is binary terminal output, got {replay:?}"
    );
}

#[tokio::test]
async fn a_socket_without_the_token_is_refused_before_the_upgrade() {
    let (_root, _secret, address) = serve().await;

    let error = attach(address, "acpilot.v1")
        .await
        .expect_err("a page that forgot the token is not let in");
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), 401);
        }
        other => panic!("expected a 401, got {other:?}"),
    }
}

#[tokio::test]
async fn a_bearer_header_still_works_for_a_client_that_can_set_one() {
    // The page is the reason the protocol list exists, not a replacement for the
    // header every other client sends. A script opening the same socket with
    // `Authorization` and no protocol names still attaches; it just gets no
    // `Sec-WebSocket-Protocol` back, which a non-browser client does not require.
    let (_root, _secret, address) = serve().await;
    let mut request = format!("ws://{address}/v1/terminals/t1/attach")
        .into_client_request()
        .expect("request");
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse().expect("a header"),
    );
    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    let (mut socket, response) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("the header is still the contract");

    assert!(
        response
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .is_none(),
        "there was no protocol to agree on"
    );
    let frame = socket.next().await.expect("a frame").expect("a good frame");
    assert!(
        matches!(frame, Message::Text(_)),
        "the attach notice arrives: {frame:?}"
    );
}
