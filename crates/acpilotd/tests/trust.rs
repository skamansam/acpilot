//! `POST /v1/agents/{id}/trust`, against the fixture agent on this host.
//!
//! The state the route clears has to be reachable, or the test would only prove that
//! a `200` is a `200`: a pin that names another binary is written first, and
//! `GET /v1/agents` has to report `untrusted` until the route records the fingerprint
//! of the file that is actually there.

mod common;

use std::sync::Arc;

use acpilot_agent::{MapResolver, Registry};
use acpilot_store::Store;
use acpilotd::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

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

/// A daemon serving the fixture agent as `mock`, with `store` when one is given.
fn daemon(store: Option<Arc<Store>>) -> (axum::Router, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("temp dir");
    let agents = directory.path().join("agents.toml");
    let command = common::mock_agent()
        .display()
        .to_string()
        .replace('\\', "\\\\");
    std::fs::write(
        &agents,
        format!(
            "schema_version = 1\n\n[[agents]]\nid = \"mock\"\nname = \"Mock\"\n\
             description = \"The fixture agent.\"\ntransport = \"acp-stdio\"\n\
             command = \"{command}\"\n"
        ),
    )
    .expect("agents.toml");
    let registry = Registry::load(&agents, Arc::new(MapResolver::new())).expect("registry");
    let mut state = AppState::new().with_registry(Arc::new(registry));
    if let Some(store) = store {
        state = state.with_store(store);
    }
    (acpilotd::router_with(state), directory)
}

#[tokio::test]
async fn trusting_an_agent_replaces_a_pin_that_no_longer_matches() {
    let store = Arc::new(Store::in_memory().expect("store"));
    // What a replaced binary looks like: the pin is there and the file is not the
    // one it names.
    store
        .save_agent_pin("mock", "the-binary-before-it-was-replaced")
        .expect("a stale pin");
    let (app, _directory) = daemon(Some(Arc::clone(&store)));

    let (status, listed) = send(&app, "GET", "/v1/agents", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(
        listed[0]["status"], "untrusted",
        "a pin that does not name the binary is the state the route exists for"
    );

    let (status, trusted) = send(&app, "POST", "/v1/agents/mock/trust", None).await;
    assert_eq!(status, StatusCode::OK, "{trusted}");
    assert_eq!(trusted["id"], "mock");
    assert_eq!(trusted["status"], "available");

    let pins = store.agent_pins().expect("the pins");
    assert_eq!(pins.len(), 1, "a pin is replaced, not accumulated");
    assert_ne!(pins[0].fingerprint, "the-binary-before-it-was-replaced");

    let (_, listed) = send(&app, "GET", "/v1/agents", None).await;
    assert_eq!(
        listed[0]["status"], "available",
        "the listing agrees with the answer the route gave"
    );
}

#[tokio::test]
async fn the_trust_route_is_in_the_served_document() {
    let (app, _directory) = daemon(None);
    let (status, document) = send(&app, "GET", "/v1/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        document["paths"]["/v1/agents/{id}/trust"]["post"].is_object(),
        "a client reads the route from the document the daemon serves"
    );
}

#[tokio::test]
async fn an_agent_no_profile_declares_cannot_be_trusted() {
    let store = Arc::new(Store::in_memory().expect("store"));
    let (app, _directory) = daemon(Some(store));
    let (status, _) = send(&app, "POST", "/v1/agents/nobody/trust", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_daemon_with_no_store_refuses_a_pin_it_could_not_keep() {
    let (app, _directory) = daemon(None);
    let (status, refused) = send(&app, "POST", "/v1/agents/mock/trust", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .expect("a message")
            .contains("store"),
        "{refused}"
    );
}
