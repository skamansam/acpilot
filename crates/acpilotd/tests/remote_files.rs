#![cfg(unix)]

//! Reading a workspace that lives on another host.
//!
//! The stand-in shell answers by the method name in the request line and echoes the id it
//! was given, because the daemon refuses an answer to a request it did not make. Nothing
//! in this file is a file on this machine: `/home/box/project` is a path only the stand-in
//! claims to have, which is the point — the routes have to ask, not look.

use std::os::unix::fs::PermissionsExt;

use acpilotd::{AppState, router_with};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

const STAND_IN: &str = r#"#!/bin/sh
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"fs.read"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"path":"/home/box/project/README.md","text":"hello, remote\\n","bytes":14}}\n' "$id"
      ;;
    *'"method":"fs.list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"entries":[{"name":"src","path":"/home/box/project/src","is_dir":true,"size":0},{"name":"README.md","path":"/home/box/project/README.md","is_dir":false,"size":14}]}}\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocol":1,"methods":["hello","fs.list","fs.read"]}}\n' "$id"
      ;;
  esac
done
"#;

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

/// A daemon whose `ssh` is the stand-in, a tested host on it, and one workspace opened on
/// that host: the state the file routes are then asked about.
async fn remote_workspace() -> (Router, String) {
    let directory = tempfile::tempdir().expect("temp dir");
    let helper = directory.path().join("helper.sh");
    std::fs::write(&helper, STAND_IN).expect("write the stand-in");
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let state = AppState::new();
    state.hosts().prefer_ssh(Some(helper.display().to_string()));
    let application = router_with(state);

    let (status, host) = send(
        &application,
        "POST",
        "/v1/hosts",
        Some(serde_json::json!({ "kind": "ssh", "address": "box" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{host}");
    let (status, tested) = send(&application, "POST", "/v1/hosts/ssh:box/test", None).await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    let (status, opened) = send(
        &application,
        "POST",
        "/v1/workspaces",
        Some(serde_json::json!({ "host_id": "ssh:box", "root": "/home/box/project" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    let id = opened["id"].as_str().expect("an id").to_owned();
    (application, id)
}

#[tokio::test]
async fn a_remote_listing_is_the_host_s_listing_in_workspace_paths() {
    let (application, workspace) = remote_workspace().await;
    let (status, listed) = send(
        &application,
        "GET",
        &format!("/v1/files/list?workspace={workspace}&path="),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let rows = listed.as_array().expect("a list");
    assert_eq!(rows.len(), 2, "{listed}");
    assert_eq!(
        rows[0]["path"], "README.md",
        "sorted, and relative to the root"
    );
    assert_eq!(rows[0]["is_dir"], false);
    assert_eq!(rows[0]["size"], 14);
    assert_eq!(rows[1]["path"], "src");
    assert_eq!(rows[1]["is_dir"], true);
}

#[tokio::test]
async fn a_remote_read_is_the_text_the_other_host_sent() {
    let (application, workspace) = remote_workspace().await;
    let (status, read) = send(
        &application,
        "GET",
        &format!("/v1/files?workspace={workspace}&path=README.md"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["text"], "hello, remote\n");
    assert_eq!(read["path"], "README.md");
    assert!(
        read["etag"].as_str().is_some_and(|etag| !etag.is_empty()),
        "a client needs a token for what it read: {read}"
    );
    assert!(
        read.get("marks").is_none(),
        "no gutter letters without git on the far host: {read}"
    );
}

#[tokio::test]
async fn a_remote_path_still_may_not_leave_the_root() {
    let (application, workspace) = remote_workspace().await;
    let (status, body) = send(
        &application,
        "GET",
        &format!("/v1/files?workspace={workspace}&path=../secrets"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request", "{body}");
}

#[tokio::test]
async fn a_route_that_walks_this_filesystem_says_why_it_cannot_walk_that_one() {
    let (application, workspace) = remote_workspace().await;
    let (status, body) = send(
        &application,
        "GET",
        &format!("/v1/files/search?workspace={workspace}&q=hello"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict", "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("ssh:box")),
        "the refusal names the host that would have to be asked: {body}"
    );
}
