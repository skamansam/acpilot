#![cfg(unix)]

//! Opening a workspace on a host this daemon reaches through its helper.
//!
//! The transport is a shell that answers the handshake and the listing the way the real
//! helper does, named to the daemon's host registry rather than through the process's
//! environment, so nothing here depends on which tests run beside it.

use std::os::unix::fs::PermissionsExt;

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

#[tokio::test]
async fn a_remote_root_is_checked_by_asking_the_helper() {
    let directory = tempfile::tempdir().expect("temp dir");
    let helper = directory.path().join("helper.sh");
    // The daemon's first request on a host is the test route's `hello` (id 1), and
    // the workspace open is the next one (id 2): a stand-in that answered with a fixed
    // id would be refused, because the daemon checks which request an answer belongs to.
    std::fs::write(
        &helper,
        "#!/bin/sh\n\
         n=0\n\
         while read -r line; do\n\
           n=$((n+1))\n\
           if [ \"$n\" = 1 ]; then\n\
             printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocol\":1,\"methods\":[\"hello\",\"fs.list\"]}}'\n\
           else\n\
             printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"entries\":[{\"name\":\"src\",\"is_dir\":true}]}}'\n\
           fi\n\
         done\n",
    )
    .expect("write the stand-in");
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Told to the daemon rather than written into the environment: the suite runs
    // beside others in one process, and the transport is this daemon's.
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
    assert_eq!(host["id"], "ssh:box");

    // The capabilities are what the helper said, so a host that has not answered has
    // no capabilities and the open is refused until this succeeds.
    let (status, tested) = send(&application, "POST", "/v1/hosts/ssh:box/test", None).await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    assert_eq!(tested["connected"], true, "{tested}");
    assert_eq!(tested["capabilities"]["filesystem"], true, "{tested}");

    let (status, opened) = send(
        &application,
        "POST",
        "/v1/workspaces",
        Some(serde_json::json!({ "host_id": "ssh:box", "root": "/home/box/project" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    assert_eq!(opened["host_id"], "ssh:box");
    assert_eq!(opened["root"], "/home/box/project");
    assert_eq!(
        opened["name"], "project",
        "the far side's path is where the default name comes from"
    );

    let (_, listed) = send(&application, "GET", "/v1/workspaces", None).await;
    let rows = listed.as_array().expect("a list");
    assert!(
        rows.iter()
            .any(|row| row["id"] == opened["id"] && row["host_id"] == "ssh:box"),
        "{listed}"
    );
}
