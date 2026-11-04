//! What the daemon reports about the machines a workspace can live on.
//!
//! Every request goes through one `AppState`, because the registry is what the
//! routes read: a fresh one per request would forget a host between the `POST` that
//! added it and the `GET` that should list it, which is the bug these tests exist to
//! catch rather than to have.

use std::sync::Arc;

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
async fn the_local_host_is_listed_with_every_capability() {
    let (status, hosts) = send(&app(), "GET", "/v1/hosts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hosts.as_array().expect("a list").len(), 1);
    let local = &hosts[0];
    assert_eq!(local["id"], "local");
    assert_eq!(local["kind"], "local");
    assert_eq!(local["connected"], true);
    assert_eq!(
        local["capabilities"],
        serde_json::json!({
            "filesystem": true,
            "pty": true,
            "spawn": true,
            "lsp": true,
        })
    );
}

#[tokio::test]
async fn an_ssh_host_is_added_as_not_connected() {
    let app = app();
    let (status, added) = send(
        &app,
        "POST",
        "/v1/hosts",
        Some(serde_json::json!({ "kind": "ssh", "address": "me@box" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["id"], "ssh:me@box");
    assert_eq!(added["connected"], false);
    assert_eq!(
        added.get("capabilities"),
        None,
        "a host nobody reached has no report"
    );

    let (status, hosts) = send(&app, "GET", "/v1/hosts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hosts.as_array().expect("a list").len(), 2);

    let (status, _) = send(
        &app,
        "POST",
        "/v1/hosts",
        Some(serde_json::json!({ "kind": "ssh", "address": "me@box" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_remote_host_needs_an_address() {
    let (status, _) = send(
        &app(),
        "POST",
        "/v1/hosts",
        Some(serde_json::json!({ "kind": "wsl" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_daemons_own_host_cannot_be_added_again() {
    let (status, _) = send(
        &app(),
        "POST",
        "/v1/hosts",
        Some(serde_json::json!({ "kind": "local" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn testing_the_local_host_reports_what_it_can_do() {
    let (status, probed) = send(&app(), "POST", "/v1/hosts/local/test", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(probed["connected"], true);
    assert_eq!(probed["capabilities"]["pty"], true);
}

#[tokio::test]
async fn testing_an_unreachable_host_reports_the_transport_that_failed() {
    // `.invalid` is reserved by RFC 2606, so no resolver anywhere answers it: the
    // test is about the refusal, not about whether this machine has an ssh agent.
    let app = app();
    let (status, _) = send(
        &app,
        "POST",
        "/v1/hosts",
        Some(serde_json::json!({ "kind": "ssh", "address": "me@box.invalid" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, refused) = send(&app, "POST", "/v1/hosts/ssh:me@box.invalid/test", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(refused["code"], "host_unavailable");
    assert_eq!(refused["details"][0]["field"], "transport");
}

#[tokio::test]
async fn testing_an_unknown_host_is_a_404() {
    let (status, _) = send(&app(), "POST", "/v1/hosts/ssh:nobody/test", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_recorded_host_is_listed_by_the_next_daemon() {
    let store = Arc::new(acpilot_store::Store::in_memory().expect("store"));
    let (status, _) = send(
        &router_with(AppState::new().with_store(Arc::clone(&store))),
        "POST",
        "/v1/hosts",
        Some(serde_json::json!({ "kind": "ssh", "address": "me@box" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, hosts) = send(
        &router_with(AppState::new().with_store(store)),
        "GET",
        "/v1/hosts",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hosts.as_array().expect("a list").len(), 2);
    assert_eq!(hosts[1]["kind"], "ssh");
    assert_eq!(
        hosts[1]["connected"], false,
        "nothing has opened a connection in this process"
    );
}
