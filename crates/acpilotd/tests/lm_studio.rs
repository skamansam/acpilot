//! A run and its replay against a real LM Studio.
//!
//! Ignored on purpose, like the LSP and PTY tests that need a real tool: this
//! machine may have LM Studio listening on 1234 while CI does not, and a test
//! that pretends to prove a provider works by mocking it is the test the mock
//! suites already are. Run it where the server is up:
//!
//! ```text
//! cargo test -p acpilotd --test lm_studio -- --ignored
//! ```
//!
//! `ACPILOT_LM_STUDIO_URL` moves the server and `ACPILOT_LM_STUDIO_MODEL` names
//! the model; without the latter the first model the server lists is used.

use acpilot_profile::providers::ProvidersFile;
use acpilotd::providers::Providers;
use acpilotd::{AppState, router_with};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The SSE body's `model.run.finished` payload.
fn finished(body: &str) -> Value {
    let mut event = String::new();
    for line in body.lines() {
        if let Some(name) = line.strip_prefix("event: ") {
            event = name.trim().to_owned();
        } else if event == "model.run.finished"
            && let Some(data) = line.strip_prefix("data: ")
        {
            return serde_json::from_str(data).expect("the finished frame is JSON");
        }
    }
    panic!("no finished frame in:\n{body}");
}

#[tokio::test]
#[ignore = "needs LM Studio on 127.0.0.1:1234"]
async fn a_real_run_streams_and_its_replay_sends_the_same_request() {
    let base_url = std::env::var("ACPILOT_LM_STUDIO_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:1234".to_owned());
    let models: Value = reqwest::get(format!("{base_url}/v1/models"))
        .await
        .expect("LM Studio should be answering on 1234")
        .json()
        .await
        .expect("a model list");
    let model = std::env::var("ACPILOT_LM_STUDIO_MODEL")
        .ok()
        .or_else(|| {
            models["data"]
                .as_array()
                .and_then(|models| models.first())
                .and_then(|model| model["id"].as_str())
                .map(str::to_owned)
        })
        .expect("LM Studio listed no model");

    let profile = tempfile::tempdir().expect("profile");
    std::fs::create_dir_all(profile.path().join("recipes")).expect("recipes");
    std::fs::write(profile.path().join("config.toml"), "schema_version = 1\n").expect("config");
    std::fs::write(
        ProvidersFile::path_in(profile.path()),
        format!(
            "schema_version = 1\n\n[[providers]]\nid = \"studio\"\nkind = \"open_ai_compatible\"\nlabel = \"LM Studio\"\nbase_url = \"{base_url}\"\n"
        ),
    )
    .expect("providers");
    std::fs::write(
        profile.path().join("recipes/studio.toml"),
        format!(
            "schema_version = 1\nid = \"studio\"\nlabel = \"LM Studio\"\nprovider_id = \"studio\"\nmodel_id = \"{model}\"\nsystem_prompt = \"Answer in one short sentence.\"\ncontext = {{ window = 4096, reserve_output = 256, strategy = \"truncate_oldest\", max_corpus_bytes = 1024 }}\ntools = {{ read_files = false, write_files = false, run_commands = false, search = false, max_steps = 1 }}\n"
        ),
    )
    .expect("recipe");

    let providers =
        Providers::load(ProvidersFile::path_in(profile.path())).expect("providers load");
    let app = router_with(
        AppState::new()
            .with_providers(providers)
            .with_recipes(profile.path().join("recipes"))
            .with_run_log(profile.path())
            .with_profile_root(Some(profile.path().to_path_buf())),
    );
    let root = profile.path().to_string_lossy().to_string();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/recipes/studio/runs?root={root}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "prompt": "Say hello in one short sentence.", "corpus": [] })
                        .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let text = String::from_utf8(body.to_vec()).expect("SSE is text");
    assert!(
        text.contains("event: model.run.delta"),
        "a real model streams its answer:\n{text}"
    );
    let run = finished(&text);
    assert_eq!(run["status"], "succeeded", "{run}");
    assert!(
        run["tokens"]["prompt"].as_u64().unwrap_or(0) > 0,
        "the server reported usage: {run}"
    );

    let page = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/recipes/studio/runs")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    let page: Value =
        serde_json::from_slice(&page.into_body().collect().await.expect("body").to_bytes())
            .expect("page");
    let id = page["runs"][0]["id"]
        .as_str()
        .expect("the run was recorded")
        .to_owned();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/recipes/studio/runs/{id}/replay?root={root}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let text = String::from_utf8(body.to_vec()).expect("SSE is text");
    let replay = finished(&text);
    assert_eq!(
        replay["status"], "succeeded",
        "the replay ran the stored request: {replay}"
    );
    assert_ne!(replay["id"], run["id"], "a replay is a new record");

    let page = app
        .oneshot(
            Request::builder()
                .uri("/v1/recipes/studio/runs")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    let page: Value =
        serde_json::from_slice(&page.into_body().collect().await.expect("body").to_bytes())
            .expect("page");
    assert_eq!(
        page["runs"].as_array().map(Vec::len),
        Some(2),
        "both records are kept: {page}"
    );
}
