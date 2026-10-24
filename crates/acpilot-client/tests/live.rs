//! The client against a real socket.
//!
//! The stub speaks the same frames the daemon does, so the tests cover what the
//! daemon's own tests cannot: that this client parses an event stream, follows
//! `Last-Event-ID`, and turns an error envelope back into a code a caller can
//! branch on.

use std::time::Duration;

use acpilot_client::{Client, ClientError, ProfileStream, SessionStream};
use axum::Router;
use axum::response::sse::{Event, Sse};
use futures::{StreamExt, stream};
use tokio::time::timeout;

/// How long a stream test waits for a frame before calling it hung.
const PATIENCE: Duration = Duration::from_secs(5);

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

fn stub() -> Router {
    Router::new()
        .route(
            "/v1/health",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "ok": true,
                    "service": "acpilotd",
                    "version": "0.1.0",
                    "api": "/v1"
                }))
            }),
        )
        .route(
            "/v1/sessions",
            axum::routing::get(|| async { axum::Json(serde_json::json!([])) }),
        )
        .route(
            "/v1/agents",
            axum::routing::get(|| async { axum::Json(serde_json::json!([])) }),
        )
        .route(
            "/v1/sessions/s404",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    axum::Json(serde_json::json!({
                        "code": "not_found",
                        "message": "not found: session s404"
                    })),
                )
            }),
        )
        .route(
            "/v1/sessions/s1/prompt",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::CONFLICT,
                    axum::Json(serde_json::json!({
                        "code": "conflict",
                        "message": "conflict: session s1 already has a turn in flight"
                    })),
                )
            }),
        )
        .route(
            "/v1/sessions/s1/events",
            axum::routing::get(|| async {
                let frames = vec![
                    Ok::<_, std::convert::Infallible>(
                        Event::default().event("session").id("2").data(
                            serde_json::json!({
                                "seq": 2,
                                "session_id": "s1",
                                "kind": "message_chunk",
                                "at": "2026-10-24T00:00:00Z",
                                "payload": { "role": "assistant", "content": "hi" }
                            })
                            .to_string(),
                        ),
                    ),
                    Ok(Event::default()
                        .event("heartbeat")
                        .data(serde_json::json!({ "seq": 2 }).to_string())),
                ];
                Sse::new(stream::iter(frames))
            }),
        )
        .route(
            "/v1/events",
            axum::routing::get(|| async {
                let frames = vec![Ok::<_, std::convert::Infallible>(
                    Event::default().event("permission").data(
                        serde_json::json!({
                            "type": "requested",
                            "request": {
                                "id": "p1",
                                "session_id": "s1",
                                "kind": "file_write",
                                "title": "Write src/main.rs",
                                "detail": "+fn main() {}",
                                "options": ["allow_once", "deny"],
                                "created_at": "2026-10-24T00:00:00Z"
                            }
                        })
                        .to_string(),
                    ),
                )];
                Sse::new(stream::iter(frames))
            }),
        )
}

#[tokio::test]
async fn health_and_the_session_list_round_trip() {
    let client = Client::new(serve(stub()).await);
    let health = client.health().await.expect("health");
    assert!(health.ok);
    assert_eq!(health.service, "acpilotd");
    assert_eq!(client.sessions().await.expect("sessions"), Vec::new());
    assert!(client.agents().await.is_ok());
}

#[tokio::test]
async fn an_error_envelope_becomes_a_code() {
    let client = Client::new(serve(stub()).await);
    let error = client.session("s404").await.expect_err("a 404");
    assert!(error.is_not_found());
    assert_eq!(error.code(), Some("not_found"));
    assert!(error.to_string().contains("session s404"));
}

#[tokio::test]
async fn a_conflict_keeps_its_status_and_message() {
    let client = Client::new(serve(stub()).await);
    let error = client.prompt("s1", "again").await.expect_err("a conflict");
    match error {
        ClientError::Api { status, code, .. } => {
            assert_eq!(status, 409);
            assert_eq!(code, "conflict");
        }
        other => panic!("unexpected {other}"),
    }
}

#[tokio::test]
async fn a_session_stream_delivers_events_then_heartbeats() {
    let client = Client::new(serve(stub()).await);
    let mut frames = client.session_events("s1", 1);
    let first = timeout(PATIENCE, frames.next())
        .await
        .expect("a first frame")
        .expect("a message")
        .expect("a frame");
    match first {
        SessionStream::Event(event) => {
            assert_eq!(event.seq, 2);
            assert_eq!(event.payload["content"], "hi");
        }
        other => panic!("unexpected {other:?}"),
    }
    let second = timeout(PATIENCE, frames.next())
        .await
        .expect("a second frame")
        .expect("a message")
        .expect("a frame");
    assert_eq!(second, SessionStream::Heartbeat { seq: 2 });
}

#[tokio::test]
async fn a_permission_frame_carries_its_request() {
    let client = Client::new(serve(stub()).await);
    let mut frames = client.profile_events();
    let frame = timeout(PATIENCE, frames.next())
        .await
        .expect("a frame")
        .expect("a message")
        .expect("a frame");
    match frame {
        ProfileStream::Permission(request) => {
            assert_eq!(request.id, "p1");
            assert_eq!(request.session_id, "s1");
            assert_eq!(request.title, "Write src/main.rs");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn an_unreachable_daemon_says_where_it_tried() {
    // Port 1 on loopback is never a daemon; the point is the message, not the port.
    let client = Client::new("http://127.0.0.1:1");
    let error = client.health().await.expect_err("no daemon");
    assert!(matches!(error, ClientError::Transport { .. }));
    assert!(error.to_string().contains("127.0.0.1:1"));
}
