//! The attach client against a stub that speaks the daemon's frames.
//!
//! What the daemon's own tests cannot cover: that this client gets through the
//! upgrade, reads the screen replay before the live frames, sends a keystroke as
//! a binary frame, and takes the daemon's close as the end rather than an error.

use std::time::Duration;

use acpilot_client::ClientError;
use acpilot_client::terminal::{Terminal, TerminalEvent};
use acpilot_proto::terminal::{TerminalNotice, TerminalSize, TerminalSummary};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use tokio::time::timeout;

/// How long an attach test waits for a frame before calling it hung.
const PATIENCE: Duration = Duration::from_secs(5);

fn summary() -> TerminalSummary {
    TerminalSummary {
        id: "t1".to_owned(),
        session_id: "s1".to_owned(),
        size: TerminalSize { cols: 80, rows: 24 },
        alive: true,
        command: "/bin/sh".to_owned(),
    }
}

/// A terminal that has already printed `hello`, and echoes one keystroke before
/// ending with code 0 — the shortest sequence that exercises every frame type.
async fn stub_terminal(mut socket: WebSocket) {
    let attached = TerminalNotice::Attached {
        terminal: summary(),
        replay_bytes: 5,
    };
    let text = serde_json::to_string(&attached).expect("a notice is serialisable");
    if socket.send(Message::Text(text.into())).await.is_err() {
        return;
    }
    if socket
        .send(Message::Binary(b"hello".to_vec().into()))
        .await
        .is_err()
    {
        return;
    }

    while let Some(Ok(frame)) = socket.recv().await {
        if let Message::Binary(bytes) = frame {
            let _ = socket.send(Message::Binary(bytes)).await;
            let exit = TerminalNotice::Exit { code: Some(0) };
            let text = serde_json::to_string(&exit).expect("a notice is serialisable");
            let _ = socket.send(Message::Text(text.into())).await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    }
}

fn stub() -> Router {
    Router::new().route(
        "/v1/terminals/t1/attach",
        get(|upgrade: WebSocketUpgrade| async move { upgrade.on_upgrade(stub_terminal) }),
    )
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("the stub serves");
    });
    format!("http://{address}")
}

#[tokio::test]
async fn an_attach_reads_the_replay_before_the_live_frames() {
    let endpoint = serve(stub()).await;
    let mut terminal = Terminal::open(&endpoint, None, "t1")
        .await
        .expect("the attach upgrades");

    let first = timeout(PATIENCE, terminal.next())
        .await
        .expect("a frame arrives")
        .expect("the socket is open")
        .expect("the daemon sent a frame");
    match first {
        TerminalEvent::Attached {
            terminal: attached,
            replay_bytes,
        } => {
            assert_eq!(attached.id, "t1");
            assert_eq!(replay_bytes, 5, "the replay says how long it is");
        }
        other => panic!("expected `attached` first, got {other:?}"),
    }

    let replay = timeout(PATIENCE, terminal.next())
        .await
        .expect("a frame arrives")
        .expect("the socket is open")
        .expect("the daemon sent a frame");
    assert_eq!(replay, TerminalEvent::Data(b"hello".to_vec()));
}

#[tokio::test]
async fn a_keystroke_reaches_the_program_and_the_exit_ends_the_socket() {
    let endpoint = serve(stub()).await;
    let mut terminal = Terminal::open(&endpoint, None, "t1")
        .await
        .expect("the attach upgrades");
    // Drain `attached` and the replay.
    assert!(
        timeout(PATIENCE, terminal.next())
            .await
            .expect("a frame")
            .is_ok()
    );
    assert!(
        timeout(PATIENCE, terminal.next())
            .await
            .expect("a frame")
            .is_ok()
    );

    terminal
        .resize(TerminalSize {
            cols: 100,
            rows: 30,
        })
        .await
        .expect("a resize is a control frame");
    terminal.write(b"hi\r").await.expect("the keystroke sends");

    // The stub echoes what it was sent, so this is the byte the program saw.
    let echoed = timeout(PATIENCE, terminal.next())
        .await
        .expect("a frame arrives")
        .expect("the socket is open")
        .expect("the daemon sent a frame");
    assert_eq!(echoed, TerminalEvent::Data(b"hi\r".to_vec()));

    let exit = timeout(PATIENCE, terminal.next())
        .await
        .expect("a frame arrives")
        .expect("the socket is open")
        .expect("the daemon sent a frame");
    assert_eq!(exit, TerminalEvent::Exit { code: Some(0) });

    // The daemon closes after `exit`; that is the end of the stream, not a fault.
    let closed = timeout(PATIENCE, terminal.next())
        .await
        .expect("the close arrives")
        .expect("a clean close is not an error");
    assert_eq!(closed, None, "the socket is finished");
}

#[cfg(unix)]
#[tokio::test]
async fn an_attach_over_a_socket_is_one_upgrade_with_no_token() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("acpilot.sock");
    let listener = tokio::net::UnixListener::bind(&path).expect("the socket binds");
    tokio::spawn(async move {
        axum::serve(listener, stub())
            .await
            .expect("the stub serves");
    });

    let mut terminal = Terminal::open(path.to_str().expect("a UTF-8 path"), None, "t1")
        .await
        .expect("the attach upgrades over the socket");
    let first = timeout(PATIENCE, terminal.next())
        .await
        .expect("a frame arrives")
        .expect("the socket is open")
        .expect("the daemon sent a frame");
    assert!(matches!(first, TerminalEvent::Attached { .. }));
}

#[tokio::test]
async fn an_endpoint_with_no_daemon_says_so_instead_of_hanging() {
    // A port nothing listens on: the connect fails before any handshake.
    let error = Terminal::open("http://127.0.0.1:1", None, "t1")
        .await
        .expect_err("nothing is listening");
    assert!(
        matches!(error, ClientError::Terminal(_)),
        "a terminal failure has its own variant: {error:?}"
    );
}
