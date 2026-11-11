//! `POST /v1/sessions/{id}/mode`, driven through the router and a real agent.
//!
//! The engine's own test proves the ACP call; what this covers is the route: the
//! daemon copies the modes an agent advertised at `session/new`, a switch reaches the
//! agent and answers with the state it switched to, and an id the agent never offered
//! is refused with the ones it did. The agent is the fixture process, so the modes are
//! protocol traffic rather than a body a test handed itself.

mod common;

use std::sync::Arc;

use acpilot_agent::{Broker, MapResolver, Policy, Registry};
use acpilotd::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

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

/// A daemon with the fixture agent as `mock`, a workspace and a broker: everything
/// `POST /v1/sessions` needs to open one.
///
/// The directory is returned because the workspace it holds is the session's cwd; a
/// temporary that dropped early would leave the agent pointing at nothing.
fn daemon() -> (axum::Router, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
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
    let root = std::fs::canonicalize(&workspace).expect("the workspace resolves");
    let state = AppState::new()
        .with_registry(Arc::new(registry))
        .with_broker(Arc::new(Broker::with_timeout(
            Policy::default(),
            &root,
            None,
        )))
        .with_workspace(acpilotd::workspace_at(&root).expect("workspace"));
    (acpilotd::router_with(state), directory)
}

async fn session(app: axum::Router) -> String {
    let (status, created) = send(
        app,
        "POST",
        "/v1/sessions",
        Some(serde_json::json!({ "workspace_id": "", "agent_id": "mock" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    created["id"].as_str().expect("an id").to_owned()
}

#[tokio::test]
async fn a_mode_the_agent_offers_is_answered_with_the_state_it_switched_to() {
    let (app, _directory) = daemon();
    let id = session(app.clone()).await;
    let (status, switched) = send(
        app,
        "POST",
        &format!("/v1/sessions/{id}/mode"),
        Some(serde_json::json!({ "mode": "code" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{switched}");
    assert_eq!(switched["mode"], "code");
    let offered: Vec<&str> = switched["available"]
        .as_array()
        .expect("the modes the agent advertised")
        .iter()
        .map(|mode| mode["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(offered, ["ask", "code"], "the agent's own order");
}

#[tokio::test]
async fn a_mode_the_agent_never_offered_is_refused_with_the_ones_it_did() {
    let (app, _directory) = daemon();
    let id = session(app.clone()).await;
    let (status, refused) = send(
        app,
        "POST",
        &format!("/v1/sessions/{id}/mode"),
        Some(serde_json::json!({ "mode": "yolo" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["code"], "invalid_request");
    assert_eq!(refused["details"][0]["field"], "mode");
    let detail = refused["details"][0]["message"]
        .as_str()
        .expect("a detail message");
    assert!(detail.contains("ask"), "{detail}");
    assert!(detail.contains("code"), "{detail}");
}

#[tokio::test]
async fn an_empty_mode_is_refused_rather_than_sent_to_the_agent() {
    let (app, _directory) = daemon();
    let id = session(app.clone()).await;
    let (status, refused) = send(
        app,
        "POST",
        &format!("/v1/sessions/{id}/mode"),
        Some(serde_json::json!({ "mode": "  " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["details"][0]["field"], "mode");
}

#[tokio::test]
async fn a_session_that_does_not_exist_has_no_mode_to_switch() {
    let (app, _directory) = daemon();
    let (status, _) = send(
        app,
        "POST",
        "/v1/sessions/never/created/mode",
        Some(serde_json::json!({ "mode": "code" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
