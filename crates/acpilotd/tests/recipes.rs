//! Recipes, models and runs over the real router.
//!
//! The provider is a loopback HTTP server, because the interesting part of a run
//! is the request that leaves and the record that comes back, not the mapping of a
//! stubbed `Value`. The recipe directory is a temporary profile, so validation runs
//! through the same loader the daemon uses.

use acpilot_profile::providers::ProvidersFile;
use acpilotd::providers::Providers;
use acpilotd::{AppState, router_with};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router, Router as AxumRouter};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, bytes) = raw(app, method, uri, body).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn raw(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(
                    body.map_or_else(String::new, |value| value.to_string()),
                ))
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, bytes)
}

fn recipe(id: &str) -> Value {
    json!({
        "schema_version": 1,
        "id": id,
        "label": "Demo",
        "provider_id": "mock",
        "model_id": "mock-1",
        "system_prompt": "Be terse.",
        "context": { "window": 4096, "reserve_output": 256, "strategy": "truncate_oldest", "max_corpus_bytes": 1024 },
        "tools": { "read_files": true, "write_files": false, "run_commands": false, "search": false, "max_steps": 4 }
    })
}

/// A profile with one declared provider pointing at `base_url`.
fn state(profile: &std::path::Path, base_url: &str) -> AppState {
    std::fs::create_dir_all(profile.join("recipes")).expect("recipes");
    std::fs::write(profile.join("config.toml"), "schema_version = 1\n").expect("config");
    std::fs::write(
        ProvidersFile::path_in(profile),
        format!(
            "schema_version = 1\n\n[[providers]]\nid = \"mock\"\nkind = \"open_ai_compatible\"\nlabel = \"Mock\"\nbase_url = \"{base_url}\"\n"
        ),
    )
    .expect("providers");
    let providers = Providers::load(ProvidersFile::path_in(profile)).expect("providers load");
    AppState::new()
        .with_providers(providers)
        .with_recipes(profile.join("recipes"))
        .with_run_log(profile)
        .with_profile_root(Some(profile.to_path_buf()))
}

fn app(profile: &std::path::Path, base_url: &str) -> Router {
    router_with(state(profile, base_url))
}

/// The same profile, with the agents registry a real daemon always has.
///
/// Promotion writes `agents.toml` and asks the registry to read it back, so a
/// state without a registry is a state where the route has nowhere to report the
/// agent it just wrote.
fn app_with_agents(profile: &std::path::Path, base_url: &str) -> Router {
    let registry = acpilot_agent::registry::Registry::load(
        profile.join("agents.toml"),
        std::sync::Arc::new(acpilot_agent::secret::MapResolver::new()),
    )
    .expect("an absent manifest is an empty registry");
    router_with(state(profile, base_url).with_registry(std::sync::Arc::new(registry)))
}

/// A loopback server answering a model list and a chat completion.
fn provider() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    listener.set_nonblocking(true).expect("nonblocking");
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
            axum::serve(
                listener,
                AxumRouter::new()
                    .route(
                        "/v1/models",
                        get(|| async { Json(json!({ "data": [{ "id": "mock-1" }] })) }),
                    )
                    .route(
                        "/v1/chat/completions",
                        post(|| async {
                            Json(json!({
                                "choices": [{
                                    "message": { "content": "hello from the model" },
                                    "finish_reason": "stop"
                                }],
                                "usage": { "prompt_tokens": 11, "completion_tokens": 5 }
                            }))
                        }),
                    ),
            )
            .await
            .expect("mock provider");
        });
    });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn a_recipe_is_created_read_edited_and_removed() {
    let profile = tempfile::tempdir().expect("profile");
    let app = app(profile.path(), "http://127.0.0.1:1");

    let (status, created) = send(&app, "POST", "/v1/recipes", Some(recipe("demo"))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["id"], "demo");
    assert!(
        profile.path().join("recipes/demo.toml").is_file(),
        "the file is the source of truth"
    );

    let (status, list) = send(&app, "GET", "/v1/recipes", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().map(Vec::len), Some(1));

    let (status, patched) = send(
        &app,
        "PATCH",
        "/v1/recipes/demo",
        Some(json!({ "label": "Renamed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["label"], "Renamed");
    assert_eq!(
        patched["system_prompt"], "Be terse.",
        "an unnamed field did not move"
    );

    let (status, _) = send(&app, "DELETE", "/v1/recipes/demo", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "GET", "/v1/recipes/demo", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_recipe_naming_an_undeclared_provider_is_refused_and_not_left_behind() {
    let profile = tempfile::tempdir().expect("profile");
    let app = app(profile.path(), "http://127.0.0.1:1");
    let mut bad = recipe("bad");
    bad["provider_id"] = json!("nowhere");

    let (status, body) = send(&app, "POST", "/v1/recipes", Some(bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        !profile.path().join("recipes/bad.toml").exists(),
        "a refused write leaves no file"
    );
}

#[tokio::test]
async fn models_reports_each_provider_and_the_ones_that_could_not_answer() {
    let profile = tempfile::tempdir().expect("profile");
    let base_url = provider();
    let _ = app(profile.path(), &base_url);
    // A second provider that refuses every connection.
    let declarations = format!(
        "schema_version = 1\n\n[[providers]]\nid = \"mock\"\nkind = \"open_ai_compatible\"\nbase_url = \"{base_url}\"\n\n[[providers]]\nid = \"down\"\nkind = \"open_ai_compatible\"\nbase_url = \"http://127.0.0.1:1\"\n"
    );
    std::fs::write(ProvidersFile::path_in(profile.path()), declarations).expect("providers");
    let providers = Providers::load(ProvidersFile::path_in(profile.path())).expect("load");
    let app = router_with(
        AppState::new()
            .with_providers(providers)
            .with_recipes(profile.path().join("recipes"))
            .with_run_log(profile.path()),
    );

    let (status, body) = send(&app, "GET", "/v1/models", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["models"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["models"][0]["id"], "mock-1");
    assert_eq!(body["problems"][0]["provider_id"], "down");
}

#[tokio::test]
async fn a_run_streams_three_frames_and_appends_a_record() {
    let profile = tempfile::tempdir().expect("profile");
    let base_url = provider();
    let app = app(profile.path(), &base_url);
    let (status, created) = send(&app, "POST", "/v1/recipes", Some(recipe("demo"))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let root = profile.path().to_string_lossy().to_string();
    let (status, body) = raw(
        &app,
        "POST",
        &format!("/v1/recipes/demo/runs?root={root}"),
        Some(json!({ "prompt": "say hello", "corpus": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).expect("SSE is text");
    for event in [
        "event: model.run.started",
        "event: model.run.delta",
        "event: model.run.finished",
    ] {
        assert!(text.contains(event), "missing {event}:\n{text}");
    }
    assert!(text.contains("hello from the model"), "{text}");

    let (status, page) = send(&app, "GET", "/v1/recipes/demo/runs", None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["runs"].as_array().map(Vec::len), Some(1));
    assert_eq!(page["runs"][0]["status"], "succeeded");
    assert_eq!(page["runs"][0]["tokens"]["prompt"], 11);
    assert_eq!(
        page["runs"][0]["tokens"]["estimated"], false,
        "the server reported the counts"
    );
    assert!(
        profile.path().join("runs.jsonl").is_file(),
        "the record survives the request"
    );
}

#[tokio::test]
async fn a_run_record_is_read_back_with_the_request_it_sent() {
    let profile = tempfile::tempdir().expect("profile");
    let base_url = provider();
    let app = app(profile.path(), &base_url);
    let (status, created) = send(&app, "POST", "/v1/recipes", Some(recipe("demo"))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let root = profile.path().to_string_lossy().to_string();
    let (status, _) = raw(
        &app,
        "POST",
        &format!("/v1/recipes/demo/runs?root={root}"),
        Some(json!({ "prompt": "say hello", "corpus": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, page) = send(&app, "GET", "/v1/recipes/demo/runs", None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let id = page["runs"][0]["id"]
        .as_str()
        .expect("the page carries the run id")
        .to_owned();

    // The lab asks what the run read, which the page does not carry.
    let (status, record) = send(&app, "GET", &format!("/v1/recipes/demo/runs/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{record}");
    assert_eq!(record["summary"]["id"], id.as_str());
    assert_eq!(record["request"]["prompt"], "say hello");
    assert_eq!(record["request"]["budget"]["window"], 4096);
    assert!(
        record["request"]["files"].is_array(),
        "a run with no selectors read no files, and says so: {record}"
    );

    let (status, body) = send(&app, "GET", "/v1/recipes/demo/runs/not-a-run", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn a_run_with_corpus_but_no_root_is_refused_before_anything_is_sent() {
    let profile = tempfile::tempdir().expect("profile");
    let app = app(profile.path(), "http://127.0.0.1:1");
    send(&app, "POST", "/v1/recipes", Some(recipe("demo"))).await;

    let (status, body) = send(
        &app,
        "POST",
        "/v1/recipes/demo/runs",
        Some(json!({
            "prompt": "read the docs",
            "corpus": [{ "glob": "docs/**/*.md", "max_files": 2, "max_bytes": 100 }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["details"][0]["field"] == "root",
        "the refusal names the missing parameter: {body}"
    );
}

#[tokio::test]
async fn a_promoted_recipe_becomes_an_agent_and_promoting_again_replaces_the_entry() {
    let profile = tempfile::tempdir().expect("profile");
    let app = app_with_agents(profile.path(), &provider());
    let (status, created) = send(&app, "POST", "/v1/recipes", Some(recipe("demo"))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, body) = send(
        &app,
        "POST",
        "/v1/recipes/demo/promote",
        Some(json!({ "agent_id": "demo" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["agent"]["id"], "demo");
    assert_eq!(
        body["agent"]["transport"], "acp_stdio",
        "the wire spelling, which is what a client switches on: {body}"
    );
    assert_eq!(
        body["agent"]["command"], "acpilot-agent-lm --recipe demo",
        "the command is the one documented: {body}"
    );
    assert!(
        body["manifest_path"]
            .as_str()
            .is_some_and(|path| path.ends_with("agents.toml")),
        "the response says where it wrote: {body}"
    );
    // The mock answers with text and no tool call, so the probe is a warning and
    // not a refusal: warnings do not block.
    assert!(
        body["warnings"]
            .as_array()
            .is_some_and(|warnings| warnings.iter().any(|warning| warning
                .as_str()
                .is_some_and(|text| text.contains("tools probe")))),
        "{body}"
    );

    let manifest = std::fs::read_to_string(profile.path().join("agents.toml")).expect("manifest");
    assert!(
        manifest.contains("# acpilot:promoted recipe=\"recipes/demo.toml\""),
        "the entry names the recipe it came from: {manifest}"
    );

    let (status, _) = send(
        &app,
        "POST",
        "/v1/recipes/demo/promote",
        Some(json!({ "agent_id": "demo" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let manifest = std::fs::read_to_string(profile.path().join("agents.toml")).expect("manifest");
    assert_eq!(
        manifest.matches("[[agents]]").count(),
        1,
        "a second promotion replaces the entry: {manifest}"
    );

    let (status, agents) = send(&app, "GET", "/v1/agents", None).await;
    assert_eq!(status, StatusCode::OK, "{agents}");
    assert!(
        agents
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == "demo")),
        "the running daemon lists the agent without a restart: {agents}"
    );
}

#[tokio::test]
async fn promotion_refuses_to_replace_a_hand_written_entry() {
    let profile = tempfile::tempdir().expect("profile");
    let app = app_with_agents(profile.path(), &provider());
    send(&app, "POST", "/v1/recipes", Some(recipe("demo"))).await;
    std::fs::write(
        profile.path().join("agents.toml"),
        "schema_version = 1\n\n[[agents]]\nid = \"demo\"\ntransport = \"acp-stdio\"\ncommand = \"mine\"\n",
    )
    .expect("manifest");

    let (status, body) = send(
        &app,
        "POST",
        "/v1/recipes/demo/promote",
        Some(json!({ "agent_id": "demo" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let manifest = std::fs::read_to_string(profile.path().join("agents.toml")).expect("manifest");
    assert!(
        manifest.contains("command = \"mine\""),
        "the hand-written entry survived: {manifest}"
    );
}

#[tokio::test]
async fn promoting_a_recipe_that_is_not_there_is_a_not_found() {
    let profile = tempfile::tempdir().expect("profile");
    let app = app_with_agents(profile.path(), &provider());
    let (status, body) = send(
        &app,
        "POST",
        "/v1/recipes/ghost/promote",
        Some(json!({ "agent_id": "ghost" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn a_provider_in_use_cannot_be_deleted() {
    let profile = tempfile::tempdir().expect("profile");
    let base_url = provider();
    let app = app(profile.path(), &base_url);
    send(&app, "POST", "/v1/recipes", Some(recipe("demo"))).await;

    let (status, body) = send(
        &app,
        "PATCH",
        "/v1/providers/mock",
        Some(json!({ "label": "Mocked" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["label"], "Mocked");

    let (status, body) = send(&app, "DELETE", "/v1/providers/mock", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("demo")),
        "the refusal names the recipe: {body}"
    );

    send(&app, "DELETE", "/v1/recipes/demo", None).await;
    let (status, _) = send(&app, "DELETE", "/v1/providers/mock", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let text = std::fs::read_to_string(ProvidersFile::path_in(profile.path())).expect("file");
    assert!(!text.contains("mock"), "{text}");
}

#[tokio::test]
async fn a_probe_route_keeps_the_answer_it_received() {
    let profile = tempfile::tempdir().expect("profile");
    let base_url = provider();
    let app = app(profile.path(), &base_url);

    let (status, body) = send(&app, "POST", "/v1/providers/mock/probe", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "reachable");
    assert_eq!(body["model_count"], 1);
    assert!(body["last_probed_at"].as_str().is_some());
}

#[tokio::test]
async fn a_daemon_without_a_profile_says_so_instead_of_serving_an_empty_list() {
    let app = router_with(AppState::new());
    let (status, body) = send(&app, "GET", "/v1/recipes", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
