//! Opening a workspace: the daemon looks at the root before it records one.
//!
//! The local half of `POST /v1/workspaces`. The remote half is in
//! `workspaces_remote.rs`, because it has to move the transport with an environment
//! variable and one test per process is what keeps that honest.

use acpilotd::{AppState, router_with};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn send(
    app: &Router,
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

fn app() -> Router {
    router_with(AppState::new())
}

#[tokio::test]
async fn a_local_root_is_opened_and_then_listed() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path().display().to_string();
    let application = app();

    let (status, opened) = send(
        &application,
        "POST",
        "/v1/workspaces",
        Some(serde_json::json!({ "host_id": "local", "root": root })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    assert_eq!(opened["host_id"], "local");
    assert_eq!(opened["root"], root);
    assert_eq!(opened["session_count"], 0);
    let name = directory
        .path()
        .file_name()
        .expect("a name")
        .to_string_lossy()
        .to_string();
    assert_eq!(opened["name"], name, "the basename is the default name");

    let (status, listed) = send(&application, "GET", "/v1/workspaces", None).await;
    assert_eq!(status, StatusCode::OK);
    let rows = listed.as_array().expect("a list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], opened["id"]);
}

#[tokio::test]
async fn opening_the_same_pair_twice_is_one_workspace() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path().display().to_string();
    let application = app();
    let body = serde_json::json!({ "host_id": "local", "root": root });

    let (_, first) = send(&application, "POST", "/v1/workspaces", Some(body.clone())).await;
    let (status, second) = send(&application, "POST", "/v1/workspaces", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(first["id"], second["id"]);

    let (_, listed) = send(&application, "GET", "/v1/workspaces", None).await;
    assert_eq!(
        listed.as_array().expect("a list").len(),
        1,
        "a client opens its workspace on every launch"
    );
}

#[tokio::test]
async fn a_root_that_is_not_there_is_not_opened() {
    let application = app();
    let (status, body) = send(
        &application,
        "POST",
        "/v1/workspaces",
        Some(serde_json::json!({ "host_id": "local", "root": "/nonexistent/workspace" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "not_found", "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("/nonexistent/workspace")),
        "{body}"
    );
}

#[tokio::test]
async fn a_file_is_not_a_workspace() {
    let directory = tempfile::tempdir().expect("temp dir");
    let file = directory.path().join("notes.md");
    std::fs::write(&file, "hello").expect("write");
    let (status, body) = send(
        &app(),
        "POST",
        "/v1/workspaces",
        Some(serde_json::json!({
            "host_id": "local",
            "root": file.display().to_string(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn a_relative_root_is_refused_before_anything_is_looked_at() {
    let (status, body) = send(
        &app(),
        "POST",
        "/v1/workspaces",
        Some(serde_json::json!({ "host_id": "local", "root": "src/project" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_host_this_daemon_does_not_know_is_a_404() {
    let (status, body) = send(
        &app(),
        "POST",
        "/v1/workspaces",
        Some(serde_json::json!({ "host_id": "ssh:somewhere", "root": "/srv" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}
