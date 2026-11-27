//! The session routes, driven through the router.
//!
//! Most tests stop before an agent is talking: the empty list, the refusals that
//! name why a session cannot start, and 404s that do not turn into an empty
//! success. The last one starts the fixture process, which is where a selected
//! skill has to survive the daemon, the wire and the child's own parser to count.
//! The store's event ring and the title rules are unit tests in `sessions.rs`,
//! next to the code they describe.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use acpilot_agent::{Broker, MapResolver, Policy, Registry};
use acpilotd::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures::StreamExt;
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

/// A daemon with the fixture agent as `mock`, a workspace and a broker.
///
/// The directory is returned because the workspace it holds is the session's cwd,
/// and the fixture reads the skill from that same root.
fn live_app() -> (axum::Router, tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("temp dir");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let command = common::mock_agent()
        .display()
        .to_string()
        .replace('\\', "\\\\");
    let agents = directory.path().join("agents.toml");
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
    (app(state), directory, root)
}

/// Read the session's event stream until `needle` appears in the raw SSE text.
///
/// The stream does not close on its own, so this stops at the first matching
/// frame rather than collecting the body. Reading the raw text is deliberate:
/// the assertion is about what crossed the socket, not about a value a test
/// reconstructed from the same types the daemon already used.
async fn stream_until(app: axum::Router, uri: &str, needle: &str) -> String {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut seen = String::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, body.next()).await {
            Ok(Some(Ok(chunk))) => {
                seen.push_str(&String::from_utf8_lossy(&chunk));
                if seen.contains(needle) {
                    return seen;
                }
            }
            Ok(Some(Err(error))) => panic!("the event stream failed: {error}"),
            Ok(None) => panic!("the event stream closed before {needle:?}: {seen}"),
            Err(_) => panic!("the event stream never carried {needle:?}; tail: {seen}"),
        }
    }
}

/// Write one skill under the workspace's `.acpilot/skills` root.
fn write_skill(workspace: &Path, name: &str, body: &str) {
    let directory = workspace.join(".acpilot").join("skills").join(name);
    std::fs::create_dir_all(&directory).expect("skill directory");
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: The {name} skill.\n---\n{body}\n"),
    )
    .expect("SKILL.md");
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
            profile: String::new(),
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
async fn an_external_agent_receives_the_selected_skills_over_stdio() {
    let (app, _directory, workspace) = live_app();
    write_skill(&workspace, "review", "Read the diff before commenting.");
    write_skill(
        &workspace,
        "release",
        "Bump the version and write the notes.",
    );

    let (status, created) = send(
        app.clone(),
        "POST",
        "/v1/sessions",
        Some(serde_json::json!({
            "workspace_id": "",
            "agent_id": "mock",
            "skills": ["review"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["id"].as_str().expect("the session id");

    // The fixture advertises `systemPrompt` and echoes what it received when
    // told `system-prompt`; only the selected skill should be in that text.
    let (status, accepted) = send(
        app.clone(),
        "POST",
        &format!("/v1/sessions/{id}/prompt"),
        Some(serde_json::json!({ "text": "system-prompt" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");

    let events = stream_until(
        app,
        &format!("/v1/sessions/{id}/events?after=0"),
        "## Skill: review",
    )
    .await;
    assert!(
        events.contains("Read the diff before commenting."),
        "the selected skill's instructions crossed stdio: {events}"
    );
    assert!(
        !events.contains("## Skill: release"),
        "an unselected skill must not be sent: {events}"
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
            profile: String::new(),
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
