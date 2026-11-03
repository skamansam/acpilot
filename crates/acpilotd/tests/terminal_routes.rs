#![cfg(unix)]

//! Resizing and ending a terminal over HTTP, on a real PTY.
//!
//! The WS attach frame is the usual way a pane says it changed shape, but a pane
//! being restored has no socket yet, and a test has no pane at all — so the two
//! instructions that are not bytes are also routes. This is the contract those
//! routes keep: the size the program sees changes, and a deleted terminal is gone
//! from the list with its process ended.
//!
//! The terminal is built through `Terminal::spawn` and inserted, rather than
//! created by `POST /v1/terminals`: that route refuses a session id it does not
//! know, and standing up a real agent session to test a resize would test the
//! agent instead. The route's own refusals are covered by the suite beside this one.

use std::sync::Arc;

use acpilot_host::ExecCommand;
use acpilot_proto::terminal::TerminalSize;
use acpilotd::{AppState, Terminal, router_with};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// One daemon state with one real `/bin/sh` in it, so every request in a test
/// lands on the same registry.
fn app(root: &std::path::Path) -> (axum::Router, Arc<Terminal>) {
    let workspace = acpilotd::workspace_at(root).expect("workspace");
    let state = AppState::new().with_workspace(workspace);
    let terminal = Terminal::spawn(
        "t1".to_owned(),
        "s1".to_owned(),
        ExecCommand::new("/bin/sh"),
        TerminalSize { cols: 80, rows: 24 },
    )
    .expect("a pty");
    state.terminals().insert(Arc::clone(&terminal));
    (router_with(state), terminal)
}

async fn send(
    app: &axum::Router,
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
    let response = app
        .clone()
        .oneshot(request)
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

#[tokio::test]
async fn a_terminal_resizes_and_then_ends() {
    let root = tempfile::tempdir().expect("workspace");
    let (app, terminal) = app(root.path());
    let (status, resized) = send(
        &app,
        "POST",
        "/v1/terminals/t1/resize",
        Some(serde_json::json!({ "cols": 100, "rows": 30 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resized}");
    assert_eq!(resized["size"]["cols"], 100);
    assert_eq!(resized["size"]["rows"], 30);

    let (status, _) = send(&app, "DELETE", "/v1/terminals/t1", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        !terminal.summary().alive,
        "ending a pane ends the program, not just the entry"
    );

    let (status, listed) = send(&app, "GET", "/v1/terminals", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed, serde_json::json!([]), "the pane is forgotten");
}

#[tokio::test]
async fn a_zero_size_is_refused() {
    let root = tempfile::tempdir().expect("workspace");
    let (app, _terminal) = app(root.path());
    let (status, _) = send(
        &app,
        "POST",
        "/v1/terminals/t1/resize",
        Some(serde_json::json!({ "cols": 0, "rows": 30 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_unknown_terminal_is_a_404_on_both_routes() {
    let root = tempfile::tempdir().expect("workspace");
    let (app, _terminal) = app(root.path());
    let (status, _) = send(
        &app,
        "POST",
        "/v1/terminals/t99/resize",
        Some(serde_json::json!({ "cols": 80, "rows": 24 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = send(&app, "DELETE", "/v1/terminals/t99", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
