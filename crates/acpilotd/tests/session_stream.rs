//! `WS /v1/sessions/{id}/stream`: the session feed over a socket.
//!
//! Driven the way a client drives it — a real listener, a real upgrade, real
//! frames — because the route's job is the handshake as much as the events: an
//! unknown session has to be an HTTP `404` with the usual envelope, not a socket
//! that opens and then closes for a reason a client cannot read.

use std::sync::Arc;

use acpilot_proto::event::{SessionEvent, SessionEventKind};
use acpilot_proto::session::{SessionStatus, SessionSummary};
use acpilot_store::Store;
use acpilotd::{AppState, router_with};
use futures::StreamExt;
use tokio_tungstenite::tungstenite::{self, Message};

fn summary() -> SessionSummary {
    SessionSummary {
        id: "s1".into(),
        profile: "default".into(),
        workspace_id: "w1".into(),
        agent_id: "mock".into(),
        title: "a conversation".into(),
        status: SessionStatus::Closed,
        created_at: "2026-11-03T10:00:00Z".into(),
        updated_at: "2026-11-03T10:05:00Z".into(),
    }
}

fn event(seq: u64) -> SessionEvent {
    SessionEvent {
        seq,
        session_id: "s1".into(),
        kind: SessionEventKind::MessageChunk,
        at: "2026-11-03T10:00:00Z".into(),
        payload: serde_json::json!({ "text": format!("line {seq}") }),
    }
}

/// Open a socket the way a client does: connect, then upgrade.
///
async fn connect(
    address: std::net::SocketAddr,
    path: &str,
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    tungstenite::Error,
> {
    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    tokio_tungstenite::client_async(format!("ws://{address}{path}"), stream).await
}

/// Serve `state` on a loopback port and return its address.
async fn serve(state: AppState) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = router_with(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    address
}

#[tokio::test]
async fn a_stored_session_replays_from_the_sequence_it_was_asked_for() {
    let store = Arc::new(Store::in_memory().expect("store"));
    store.save_session(&summary()).expect("session");
    for seq in 1..=3 {
        store.append_event(&event(seq)).expect("event");
    }
    let address = serve(AppState::new().with_store(Arc::clone(&store))).await;
    let (mut socket, _) = connect(address, "/v1/sessions/s1/stream?after=1")
        .await
        .expect("upgrade");

    let mut seen = Vec::new();
    while let Some(frame) = socket.next().await {
        let frame = frame.expect("frame");
        if let Message::Text(text) = frame {
            let event: SessionEvent = serde_json::from_str(&text).expect("an event");
            seen.push(event.seq);
            if seen.len() == 2 {
                break;
            }
        }
    }
    assert_eq!(seen, vec![2, 3], "the replay starts where it was told to");
}

#[tokio::test]
async fn an_unknown_session_is_a_404_before_the_upgrade() {
    let store = Arc::new(Store::in_memory().expect("store"));
    let address = serve(AppState::new().with_store(store)).await;
    let error = connect(address, "/v1/sessions/nobody/stream")
        .await
        .expect_err("an unknown session is refused");
    match error {
        tungstenite::Error::Http(response) => assert_eq!(response.status(), 404),
        other => panic!("expected a 404, got {other:?}"),
    }
}

#[tokio::test]
async fn a_session_this_daemon_never_started_cannot_be_streamed_without_a_record() {
    let address = serve(AppState::new()).await;
    let error = connect(address, "/v1/sessions/s1/stream")
        .await
        .expect_err("no store and no session");
    match error {
        tungstenite::Error::Http(response) => assert_eq!(response.status(), 404),
        other => panic!("expected a 404, got {other:?}"),
    }
}
