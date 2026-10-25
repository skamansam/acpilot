//! The terminal routes, driven through the router.
//!
//! Nothing here starts a process: a terminal needs a session to belong to, and a
//! session needs a real agent, so the refusals are what a route can be tested for
//! on its own. The terminal itself — its screen, its replay, its input, its
//! resize — is tested against a real PTY in `terminal_process.rs`.

use acpilotd::{AppState, Workspace};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app(state: AppState) -> axum::Router {
    acpilotd::router_with(state)
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(Body::from(
            body.map_or_else(String::new, |value| value.to_string()),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("router is infallible");
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

fn body(session: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": session,
        "size": { "cols": 80, "rows": 24 }
    })
}

/// A daemon that serves `directory` as its workspace.
fn serving(directory: &std::path::Path) -> AppState {
    AppState::new().with_workspace(Workspace {
        id: directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_owned(),
        root: directory.canonicalize().expect("resolved"),
    })
}

#[tokio::test]
async fn a_daemon_with_no_terminals_lists_none() {
    let (status, body) = send(app(AppState::new()), "GET", "/v1/terminals", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]));
}

#[tokio::test]
async fn a_daemon_without_a_workspace_refuses_to_start_one() {
    // No workspace means no directory to start in, and guessing one would put a
    // shell somewhere the daemon never said it serves.
    let (status, body) = send(
        app(AppState::new()),
        "POST",
        "/v1/terminals",
        Some(body("s1")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "conflict");
}

#[tokio::test]
async fn a_zero_dimension_is_a_400_naming_the_field() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (status, body) = send(
        app(serving(directory.path())),
        "POST",
        "/v1/terminals",
        Some(serde_json::json!({
            "session_id": "s1",
            "size": { "cols": 0, "rows": 24 }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
    assert_eq!(body["details"][0]["field"], "size");
}

#[tokio::test]
async fn arguments_without_a_command_are_refused() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (status, body) = send(
        app(serving(directory.path())),
        "POST",
        "/v1/terminals",
        Some(serde_json::json!({
            "session_id": "s1",
            "args": ["-l"],
            "size": { "cols": 80, "rows": 24 }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["details"][0]["field"], "args");
}

#[tokio::test]
async fn an_unknown_session_is_a_404() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (status, body) = send(
        app(serving(directory.path())),
        "POST",
        "/v1/terminals",
        Some(body("s9")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("s9")),
        "{body}"
    );
}

#[tokio::test]
async fn a_missing_size_is_a_400() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (status, body) = send(
        app(serving(directory.path())),
        "POST",
        "/v1/terminals",
        Some(serde_json::json!({ "session_id": "s1" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
}

#[tokio::test]
async fn the_contract_lists_the_terminal_routes() {
    let (status, body) = send(app(AppState::new()), "GET", "/v1/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    let path = &body["paths"]["/v1/terminals"];
    assert!(path["get"].is_object(), "GET /v1/terminals missing: {path}");
    assert!(
        path["post"].is_object(),
        "POST /v1/terminals missing: {path}"
    );
    assert!(
        body["components"]["schemas"]["CreateTerminalRequest"].is_object(),
        "CreateTerminalRequest missing from the components"
    );
}
