//! The profile-wide stream, opened the way a client opens it.
//!
//! The point of this route is that a client does not poll: the test subscribes
//! with an ordinary HTTP client, raises a request the way an agent does, and
//! asserts the frame arrives on its own. Nothing in the test asks the daemon
//! whether something is pending.

use std::sync::Arc;
use std::time::Duration;

use acpilot_agent::{Answer, Broker, PermissionKind, PermissionOption, Policy, Subject};
use acpilot_proto::permission::PermissionRequest;
use acpilotd::AppState;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures::StreamExt;
use http_body_util::BodyExt;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tower::ServiceExt;

const PATIENCE: Duration = Duration::from_secs(5);

fn broker(policy: Policy, workspace: &std::path::Path) -> Arc<Broker> {
    Arc::new(Broker::with_timeout(
        policy,
        std::fs::canonicalize(workspace).expect("the workspace resolves"),
        None,
    ))
}

fn app(broker: &Arc<Broker>) -> Router {
    acpilotd::router_with(AppState::new().with_broker(Arc::clone(broker)))
}

async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
    let response = acpilotd::router()
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

/// Raise a request the way an agent does, and leave it waiting for an answer.
async fn raise(
    broker: &Arc<Broker>,
    session: &str,
    title: &str,
) -> (PermissionRequest, JoinHandle<Answer>) {
    let mut announcement = broker.subscribe();
    let asker = Arc::clone(broker);
    let session = session.to_owned();
    let title = title.to_owned();
    let waiting = tokio::spawn(async move {
        asker
            .ask(&session, Subject::new(PermissionKind::Terminal, title))
            .await
    });
    let request = timeout(PATIENCE, announcement.recv())
        .await
        .expect("a request is announced")
        .expect("the stream is open");
    (request, waiting)
}

#[tokio::test]
async fn a_client_learns_a_request_exists_without_asking() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let address = listener.local_addr().expect("local address");
    let serving = tokio::spawn({
        let app = app(&broker);
        async move {
            axum::serve(listener, app).await.expect("server runs");
        }
    });

    let response = reqwest::get(format!("http://{address}/v1/events"))
        .await
        .expect("the stream opens");
    assert_eq!(response.status(), 200);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("a content type")
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "a client can tell it is a stream: {content_type}"
    );

    let mut frames = response.bytes_stream();

    // The request exists only after the client is connected, which is the case
    // the stream is for: nothing had to be pending when it connected.
    let (request, _waiting) = raise(&broker, "session-1", "cargo fmt").await;

    let mut buffer = String::new();
    let frame = loop {
        let chunk = timeout(PATIENCE, frames.next())
            .await
            .expect("a frame arrives without anyone polling")
            .expect("the stream is still open")
            .expect("a readable chunk");
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        if let Some(end) = buffer.find("\n\n") {
            break buffer[..end].to_owned();
        }
    };

    assert!(
        frame.lines().any(|line| line == "event: permission"),
        "the frame says what it is: {frame}"
    );
    assert!(
        frame.lines().any(|line| line == "id: 1"),
        "the frame is numbered from one: {frame}"
    );
    assert!(
        frame.contains(r#""type":"requested""#),
        "the payload is the documented envelope: {frame}"
    );
    assert!(
        frame.contains(&format!(r#""id":"{}""#, request.id)),
        "the announced request is the one the agent is waiting on, so a client can answer it: {frame}"
    );
    assert!(
        frame.contains(r#""session_id":"session-1""#),
        "the client can tell which session is blocked: {frame}"
    );
    assert!(
        frame.contains(r#""options":["allow_once","allow_always","deny"]"#),
        "the client does not have to guess the options: {frame}"
    );

    serving.abort();
    let _ = serving.await;
}

#[tokio::test]
async fn a_daemon_without_a_broker_keeps_the_stream_open_and_quiet() {
    // No profile means nothing can be announced, which is a quiet stream rather
    // than a closed one: a client that reconnected on every close would loop
    // forever against a daemon that is perfectly healthy.
    let response = acpilotd::router()
        .oneshot(
            Request::builder()
                .uri("/v1/events")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("a content type")
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
}

#[tokio::test]
async fn the_stream_is_documented_where_a_client_generator_looks() {
    let (status, body) = get_json("/v1/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    let description = body["paths"]["/v1/events"]["get"]["responses"]["200"]["description"]
        .as_str()
        .expect("the route is in the document");
    for frame in ["text/event-stream", "prefs", "permission"] {
        assert!(
            description.contains(frame),
            "the description says what a client will receive, and `{frame}` is missing: \
             {description}"
        );
    }
    assert_eq!(
        body["paths"]["/v1/events"]["get"]["summary"],
        "Follow permission requests and settings changes",
        "a generated client has a name for the route"
    );
}

#[tokio::test]
async fn one_request_is_announced_once() {
    // The broker hands a decision out once, so the stream must not repeat a
    // request either: a prompt the user already answered reappearing is how a
    // UI double-answers.
    let dir = tempfile::tempdir().expect("temp dir");
    let broker = broker(Policy::shipped(), dir.path());
    let mut announcements = broker.subscribe();
    let (_request, _waiting) = raise(&broker, "session-1", "cargo fmt").await;
    // `raise` subscribed on its own; this receiver saw the same announcement.
    let seen = timeout(PATIENCE, announcements.recv())
        .await
        .expect("the request was announced")
        .expect("the channel is open");
    assert_eq!(seen.session_id, "session-1");
    assert_eq!(
        broker.pending().len(),
        1,
        "one request is one pending entry, whatever was announced"
    );
    assert!(broker.answer(&seen.id, PermissionOption::Deny));
}
