//! The session routes, driven through the router.
//!
//! A session that can actually prompt needs a real agent process, which is what
//! the engine's own tests cover against the mock agent. What is tested here is
//! everything a client hits *before* an agent is talking: the empty list, the
//! refusals that name why a session cannot start, and 404s that do not turn into
//! an empty success. The store's event ring and the title rules are unit tests in
//! `sessions.rs`, next to the code they describe.

use std::sync::Arc;

use acpilot_agent::{MapResolver, Registry};
use acpilotd::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app(state: AppState) -> axum::Router {
    acpilotd::router_with(state)
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    send(app, "GET", uri, None).await
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

fn create_body(agent: &str) -> serde_json::Value {
    serde_json::json!({ "workspace_id": "", "agent_id": agent })
}

#[tokio::test]
async fn a_daemon_with_no_profile_lists_no_sessions() {
    let (status, body) = get(app(AppState::new()), "/v1/sessions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]));
}

#[tokio::test]
async fn creating_a_session_without_a_profile_says_so() {
    let (status, body) = send(
        app(AppState::new()),
        "POST",
        "/v1/sessions",
        Some(create_body("mock")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "conflict");
    assert!(
        body["message"]
            .as_str()
            .expect("a message")
            .contains("no profile"),
        "{body}"
    );
}

#[tokio::test]
async fn an_unknown_agent_is_a_404_that_names_it() {
    let directory = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        directory.path().join("agents.toml"),
        r#"
schema_version = 1

[[agents]]
id = "mock"
name = "Mock"
description = "The fixture agent."
transport = "acp-stdio"
command = "acpilot-mock-agent"
"#,
    )
    .expect("agents.toml");
    let registry = Registry::load(
        directory.path().join("agents.toml"),
        Arc::new(MapResolver::new()),
    )
    .expect("the registry loads");

    let (status, body) = send(
        app(AppState::new().with_registry(Arc::new(registry))),
        "POST",
        "/v1/sessions",
        Some(create_body("nope")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["message"]
            .as_str()
            .expect("a message")
            .contains("nope"),
        "{body}"
    );
}

#[tokio::test]
async fn a_session_that_does_not_exist_is_a_404_on_every_route() {
    for (method, uri) in [
        ("GET", "/v1/sessions/s99"),
        ("DELETE", "/v1/sessions/s99"),
        ("POST", "/v1/sessions/s99/prompt"),
        ("POST", "/v1/sessions/s99/cancel"),
        ("GET", "/v1/sessions/s99/events"),
    ] {
        let body = (method == "POST").then(|| serde_json::json!({ "text": "hello" }));
        let (status, payload) = send(app(AppState::new()), method, uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {payload}");
        assert_eq!(payload["code"], "not_found", "{method} {uri}");
    }
}

#[tokio::test]
async fn an_empty_prompt_is_refused_before_the_session_is_looked_up() {
    let (status, body) = send(
        app(AppState::new()),
        "POST",
        "/v1/sessions/s1/prompt",
        Some(serde_json::json!({ "text": "   \n " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
}

#[tokio::test]
async fn a_workspace_the_daemon_does_not_serve_is_named_in_the_refusal() {
    let directory = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        directory.path().join("agents.toml"),
        r#"
schema_version = 1

[[agents]]
id = "mock"
name = "Mock"
description = "The fixture agent."
transport = "acp-stdio"
command = "acpilot-mock-agent"
"#,
    )
    .expect("agents.toml");
    let registry = Registry::load(
        directory.path().join("agents.toml"),
        Arc::new(MapResolver::new()),
    )
    .expect("the registry loads");
    let workspace = directory.path().join("work");
    std::fs::create_dir(&workspace).expect("workspace");

    let state = AppState::new()
        .with_registry(Arc::new(registry))
        .with_workspace(acpilotd::Workspace {
            id: "work".to_owned(),
            root: workspace,
        });
    let (status, body) = send(
        app(state),
        "POST",
        "/v1/sessions",
        Some(serde_json::json!({ "workspace_id": "elsewhere", "agent_id": "mock" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["message"]
            .as_str()
            .expect("a message")
            .contains("elsewhere"),
        "{body}"
    );
}

#[tokio::test]
async fn a_session_with_no_policy_refuses_to_start_rather_than_guessing() {
    let directory = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        directory.path().join("agents.toml"),
        r#"
schema_version = 1

[[agents]]
id = "mock"
name = "Mock"
description = "The fixture agent."
transport = "acp-stdio"
command = "acpilot-mock-agent"
"#,
    )
    .expect("agents.toml");
    let registry = Registry::load(
        directory.path().join("agents.toml"),
        Arc::new(MapResolver::new()),
    )
    .expect("the registry loads");
    let workspace = directory.path().join("work");
    std::fs::create_dir(&workspace).expect("workspace");

    let state = AppState::new()
        .with_registry(Arc::new(registry))
        .with_workspace(acpilotd::Workspace {
            id: "work".to_owned(),
            root: workspace,
        });
    let (status, body) = send(
        app(state),
        "POST",
        "/v1/sessions",
        Some(create_body("mock")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["message"]
            .as_str()
            .expect("a message")
            .contains("permission policy"),
        "{body}"
    );
}
