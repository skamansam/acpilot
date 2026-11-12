#![cfg(unix)]

//! Letters and hidden rows on a workspace that lives on another host.
//!
//! Both questions are asked of *that* host — `git status` for the letters and
//! `git check-ignore` for the hidden rows — so the stand-in answers them the way a host
//! would and the routes are asserted to pass them through untouched. Nothing here reads a
//! file on this machine: `/home/box/project` is a path only the far side claims to have.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use acpilotd::{AppState, router_with};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// The stand-in host, with `MARKER` replaced by the path a test uses to say whether this
/// helper offers `fs.ignored`. A helper from before that method refuses by name, which is
/// the case where a tree must show every file rather than lose rows.
const STAND_IN: &str = r##"#!/bin/sh
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"exec"'*)
      case "$line" in
        *rev-parse*)
          cat <<JSON
{"jsonrpc":"2.0","id":$id,"result":{"code":0,"stdout":"/home/box/project","stderr":""}}
JSON
          ;;
        *)
          cat <<JSON
{"jsonrpc":"2.0","id":$id,"result":{"code":0,"stdout":"# branch.head main\u00001 .M N... 100644 100644 100644 a a README.md\u0000? src/new.rs\u0000","stderr":""}}
JSON
          ;;
      esac
      ;;
    *'"method":"fs.ignored"'*)
      if [ -e "MARKER" ]; then
        cat <<JSON
{"jsonrpc":"2.0","id":$id,"result":{"ignored":["target"]}}
JSON
      else
        cat <<JSON
{"jsonrpc":"2.0","id":$id,"error":{"code":-32601,"message":"fs.ignored is not a method this helper has"}}
JSON
      fi
      ;;
    *'"method":"fs.list"'*)
      cat <<JSON
{"jsonrpc":"2.0","id":$id,"result":{"entries":[{"name":".git","path":"/home/box/project/.git","is_dir":true,"size":0},{"name":"README.md","path":"/home/box/project/README.md","is_dir":false,"size":14},{"name":"src","path":"/home/box/project/src","is_dir":true,"size":0},{"name":"target","path":"/home/box/project/target","is_dir":true,"size":0}]}}
JSON
      ;;
    *)
      cat <<JSON
{"jsonrpc":"2.0","id":$id,"result":{"protocol":1,"methods":["hello","fs.list","fs.read","fs.ignored","exec"]}}
JSON
      ;;
  esac
done
"##;

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
/// that host. The returned directory holds the stand-in and the marker, so a test that
/// wants an older helper deletes the marker before listing.
async fn remote_workspace(directory: &Path) -> (Router, String) {
    let marker = directory.join("offers-ignore");
    std::fs::write(&marker, "").expect("the marker");

    let transport = directory.join("ssh.sh");
    std::fs::write(
        &transport,
        STAND_IN.replace("MARKER", &marker.display().to_string()),
    )
    .expect("write the stand-in");
    std::fs::set_permissions(&transport, std::fs::Permissions::from_mode(0o755))
        .expect("chmod the stand-in");

    let state = AppState::new();
    state
        .hosts()
        .prefer_ssh(Some(transport.display().to_string()));
    let app = router_with(state);

    let (status, host) = send(
        &app,
        "POST",
        "/v1/hosts",
        Some(serde_json::json!({ "kind": "ssh", "address": "box" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{host}");
    let (status, tested) = send(&app, "POST", "/v1/hosts/ssh:box/test", None).await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    let (status, opened) = send(
        &app,
        "POST",
        "/v1/workspaces",
        Some(serde_json::json!({ "host_id": "ssh:box", "root": "/home/box/project" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    let id = opened["id"].as_str().expect("an id").to_owned();
    (app, id)
}

#[tokio::test]
async fn a_remote_tree_draws_the_letters_the_far_host_reports() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (app, workspace) = remote_workspace(directory.path()).await;
    let (status, listed) = send(
        &app,
        "GET",
        &format!("/v1/files/list?workspace={workspace}&path="),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let rows = listed.as_array().expect("a list");
    let paths: Vec<&str> = rows.iter().filter_map(|row| row["path"].as_str()).collect();
    assert_eq!(
        paths,
        ["README.md", "src"],
        "`.git` is never a row and `target` is hidden by the host's ignore rule: {listed}"
    );
    assert_eq!(rows[0]["status"], "modified", "{listed}");
    assert_eq!(
        rows[1]["status"], "untracked",
        "a directory takes the change beneath it: {listed}"
    );
}

#[tokio::test]
async fn a_remote_tree_can_show_ignored_rows_and_marks_them() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (app, workspace) = remote_workspace(directory.path()).await;
    let (status, listed) = send(
        &app,
        "GET",
        &format!("/v1/files/list?workspace={workspace}&path=&include_ignored=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let rows = listed.as_array().expect("a list");
    let hidden = rows
        .iter()
        .find(|row| row["path"] == "target")
        .unwrap_or_else(|| panic!("the ignored row is shown when it is asked for: {listed}"));
    assert_eq!(
        hidden["ignored"], true,
        "and it is marked as hidden rather than drawn as an ordinary folder: {listed}"
    );
    assert!(
        rows.iter().all(|row| row["path"] != ".git"),
        "the repository's own directory is still not a row: {listed}"
    );
}

#[tokio::test]
async fn a_host_that_cannot_say_what_is_ignored_shows_every_file() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (app, workspace) = remote_workspace(directory.path()).await;
    // A helper from before `fs.ignored` refuses it by name, which is what an old upload
    // beside a new daemon does. The stand-in checks the marker per request, so removing it
    // is enough: the connection is kept, and the next question is what changes.
    std::fs::remove_file(directory.path().join("offers-ignore")).expect("the marker");

    let (status, listed) = send(
        &app,
        "GET",
        &format!("/v1/files/list?workspace={workspace}&path="),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let rows = listed.as_array().expect("a list");
    assert!(
        rows.iter().any(|row| row["path"] == "target"),
        "a host that cannot answer shows the file rather than hiding what it does not know: {listed}"
    );
    assert!(
        rows.iter().all(|row| row.get("ignored").is_none()),
        "and claims nothing about it: {listed}"
    );
}
