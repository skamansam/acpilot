//! Commit-message drafting, against a real repository and the fixture agent.
//!
//! The route is the join of three things that already have their own tests: git's
//! view of the index (`acpilot-git`), the agent's answer arriving as events
//! (`acpilotd::sessions`), and the format check (`acpilot-core`). What is new here
//! is that they meet: the request names a session, the staged diff reaches that
//! session's agent, and the answer comes back validated or not at all.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use acpilot_agent::{Broker, MapResolver, Policy, Registry};
use acpilotd::{AppState, Workspace};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

mod common;

/// Run `git` in `root` with the variables a hook exports removed.
fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    for key in [
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_QUARANTINE_PATH",
        "GIT_NAMESPACE",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
    ] {
        command.env_remove(key);
    }
    let output = command.output().expect("git is on PATH");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository with one commit, one staged edit and one unstaged edit.
fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("a temp dir");
    let root = directory.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test User"]);
    fs::write(root.join("one.txt"), "one\n").expect("one");
    fs::write(root.join("two.txt"), "two\n").expect("two");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "first"]);

    fs::write(root.join("one.txt"), "one changed\n").expect("one");
    fs::write(root.join("two.txt"), "two changed\n").expect("two");
    git(root, &["add", "two.txt"]);
    directory
}

struct Harness {
    _profile: tempfile::TempDir,
    repository: tempfile::TempDir,
    app: axum::Router,
    session: String,
}

/// A daemon with the fixture agent, a profile directory and a session already open.
async fn harness() -> Harness {
    let repository = repository();
    let profile = tempfile::tempdir().expect("a profile directory");
    let command = common::mock_agent()
        .display()
        .to_string()
        .replace('\\', "\\\\");
    let agents = profile.path().join("agents.toml");
    fs::write(
        &agents,
        format!(
            "schema_version = 1\n\n[[agents]]\nid = \"mock\"\nname = \"Mock\"\n\
             description = \"The fixture agent.\"\ntransport = \"acp-stdio\"\n\
             command = \"{command}\"\n"
        ),
    )
    .expect("agents.toml");
    let registry = Registry::load(&agents, Arc::new(MapResolver::new())).expect("the registry");
    let root = repository
        .path()
        .canonicalize()
        .expect("the workspace resolves");
    let state = AppState::new()
        .with_registry(Arc::new(registry))
        .with_broker(Arc::new(Broker::with_timeout(
            Policy::default(),
            root.clone(),
            None,
        )))
        .with_workspace(Workspace {
            id: "work".to_owned(),
            root,
        })
        .with_profile_root(Some(profile.path().to_path_buf()));
    let app = acpilotd::router_with(state);

    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/sessions",
        Some(serde_json::json!({ "workspace_id": "work", "agent_id": "mock" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let session = body["id"].as_str().expect("a session id").to_owned();

    Harness {
        _profile: profile,
        repository,
        app,
        session,
    }
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(json) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let request = builder.body(body).expect("request");
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

#[tokio::test]
async fn a_draft_comes_from_the_staged_diff_and_the_named_session() {
    let harness = harness().await;
    let (status, body) = send(
        harness.app,
        "POST",
        "/v1/git/commit-message",
        Some(serde_json::json!({ "session_id": harness.session })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The fixture only answers with a commit message when it recognises the default
    // prompt, so a 200 proves the rendered template reached the session's agent.
    assert_eq!(body["subject"], "feat(mock): ✨ describe the staged change");
    assert_eq!(body["kind"], "feat");
    assert_eq!(body["scope"], "mock");
    assert_eq!(body["emoji"], "✨");
    assert_eq!(body["body"], "");
    assert_eq!(body["message"], body["subject"]);
    assert_eq!(
        body["generator"],
        format!("session:{}", harness.session),
        "the response says which generator produced it"
    );
}

#[tokio::test]
async fn the_prompt_route_shows_the_default_then_the_profile_s_file() {
    let harness = harness().await;

    let (status, body) = send(harness.app.clone(), "GET", "/v1/git/commit/prompt", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], "default");
    assert!(
        body["template"]
            .as_str()
            .is_some_and(|template| template.contains("Conventional Commits 1.0.0")),
        "the default is the template the core crate ships: {body}"
    );

    fs::write(
        harness._profile.path().join("commit-prompt.txt"),
        "Write one line about {{files}}.\n",
    )
    .expect("the prompt");
    let (status, body) = send(harness.app, "GET", "/v1/git/commit/prompt", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], "profile");
    assert_eq!(body["template"], "Write one line about {{files}}.\n");
}

#[tokio::test]
async fn nothing_staged_is_a_conflict_before_any_agent_is_asked() {
    let harness = harness().await;
    git(harness.repository.path(), &["commit", "-q", "-m", "second"]);

    let (status, body) = send(
        harness.app,
        "POST",
        "/v1/git/commit-message",
        Some(serde_json::json!({ "session_id": harness.session })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("staged")),
        "the refusal says what the message describes: {body}"
    );
}

#[tokio::test]
async fn an_unknown_session_is_not_found() {
    let harness = harness().await;
    let (status, body) = send(
        harness.app,
        "POST",
        "/v1/git/commit-message",
        Some(serde_json::json!({ "session_id": "ghost" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn a_per_request_prompt_is_used_and_a_bad_answer_is_a_validation_error() {
    let harness = harness().await;
    // The fixture answers ordinary prose with `chunk 1`, which is not a commit
    // message. The override proves two things at once: it replaces the profile's
    // template, and an answer that ignores the format is refused here rather than
    // repaired into something nobody wrote.
    let (status, body) = send(
        harness.app,
        "POST",
        "/v1/git/commit-message",
        Some(serde_json::json!({
            "session_id": harness.session,
            "prompt": "Say anything."
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "invalid_request");
    assert_eq!(body["details"][0]["field"], "message");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("<type>")),
        "the refusal repeats the format rule: {body}"
    );
}

#[tokio::test]
async fn an_empty_session_id_is_refused_without_touching_git() {
    let harness = harness().await;
    let (status, body) = send(
        harness.app,
        "POST",
        "/v1/git/commit-message",
        Some(serde_json::json!({ "session_id": "  " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["details"][0]["field"], "session_id");
}

#[tokio::test]
async fn the_openapi_document_documents_the_commit_routes() {
    let (status, document) = send(acpilotd::router(), "GET", "/v1/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        document["paths"]["/v1/git/commit/prompt"]["get"].is_object(),
        "the prompt route is in the document"
    );
    assert!(
        document["paths"]["/v1/git/commit-message"]["post"].is_object(),
        "the draft route is in the document"
    );
    for schema in [
        "CommitPrompt",
        "CommitPromptSource",
        "CommitMessageRequest",
        "CommitDraft",
    ] {
        assert!(
            document["components"]["schemas"][schema].is_object(),
            "{schema} is in the components"
        );
    }
}
