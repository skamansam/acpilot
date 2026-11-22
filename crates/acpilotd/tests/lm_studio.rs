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
//! the model. Without the latter the first model the server lists is used.

use acpilot_profile::providers::ProvidersFile;
use acpilotd::providers::Providers;
use acpilotd::{AppState, router_with};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The record inside the SSE body's `model.run.finished` payload.
///
/// The frame wraps the summary in `run`, exactly as `acpilot-client` reads it.
/// returning the envelope here made every field on the run a null.
fn finished(body: &str) -> Value {
    let mut event = String::new();
    for line in body.lines() {
        if let Some(name) = line.strip_prefix("event: ") {
            event = name.trim().to_owned();
        } else if event == "model.run.finished"
            && let Some(data) = line.strip_prefix("data: ")
        {
            let frame: Value = serde_json::from_str(data).expect("the finished frame is JSON");
            return frame["run"].clone();
        }
    }
    panic!("no finished frame in:\n{body}");
}

/// Which server, and which model on it.
async fn studio() -> (String, String) {
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
    (base_url, model)
}

/// A profile holding one LM Studio provider and one recipe over `model`.
///
/// `sampling` is the recipe's `[sampling]` block, so a test can pin the output
/// budget. Empty means the agent-shaped defaults. It goes last because a TOML
/// table header owns every key after it, and `context` and `tools` are top-level.
fn profile(base_url: &str, model: &str, sampling: &str) -> (tempfile::TempDir, Router, String) {
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
            "schema_version = 1\nid = \"studio\"\nlabel = \"LM Studio\"\nprovider_id = \"studio\"\nmodel_id = \"{model}\"\nsystem_prompt = \"Answer in one short sentence.\"\ncontext = {{ window = 4096, reserve_output = 256, strategy = \"truncate_oldest\", max_corpus_bytes = 1024 }}\ntools = {{ read_files = false, write_files = false, run_commands = false, search = false, max_steps = 1 }}\n{sampling}"
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
    (profile, app, root)
}

/// Run `prompt` through the recipe and return the SSE body.
async fn stream_run(app: Router, root: &str, prompt: &str) -> String {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/recipes/studio/runs?root={root}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "prompt": prompt, "corpus": [] }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("router is infallible");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let text = String::from_utf8(body.to_vec()).expect("SSE is text");
    assert_eq!(status, StatusCode::OK, "{text}");
    text
}

#[tokio::test]
#[ignore = "needs LM Studio on 127.0.0.1:1234"]
async fn a_real_run_streams_and_its_replay_sends_the_same_request() {
    let (base_url, model) = studio().await;
    let (_profile, app, root) = profile(&base_url, &model, "");

    let text = stream_run(app.clone(), &root, "Say hello in one short sentence.").await;
    assert!(
        text.contains("event: model.run.delta"),
        "a real model streams its answer:\n{text}"
    );
    let record = finished(&text);
    assert_eq!(record["status"], "succeeded", "{record}");
    assert!(
        record["tokens"]["prompt"].as_u64().unwrap_or(0) > 0,
        "the server reported usage: {record}"
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
    assert_ne!(replay["id"], record["id"], "a replay is a new record");

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

/// A budget too small to reach an answer proves the thinking is not dropped.
///
/// `ornith` writes `reasoning_content` before `content`, so a tight
/// `max_tokens` is spent thinking. Before reasoning had a channel of its own,
/// that run streamed nothing and recorded an empty answer with no explanation.
#[tokio::test]
#[ignore = "needs LM Studio on 127.0.0.1:1234"]
async fn a_tight_budget_spends_it_thinking_and_the_run_says_so() {
    let (base_url, model) = studio().await;
    // `SamplingParams` has no per-field defaults in a file, so the knobs the
    // recipe would otherwise inherit are written out beside the budget.
    let (_profile, app, root) = profile(
        &base_url,
        &model,
        "[sampling]\ntemperature = 0.2\ntop_p = 0.95\nmax_tokens = 48\nstop = []\n",
    );

    let text = stream_run(
        app.clone(),
        &root,
        "How many r letters are in strawberry? Think it through.",
    )
    .await;
    assert!(
        text.contains("event: model.run.thought"),
        "the thinking arrives as its own frame:\n{text}"
    );
    assert!(
        !text.contains("event: model.run.delta"),
        "at this budget there was no answer, only thinking:\n{text}"
    );
    let summary = finished(&text);
    assert_eq!(summary["status"], "succeeded", "{summary}");
    assert_eq!(
        summary["stop_reason"], "length",
        "forty-eight tokens was the limit, so the run ended on its budget rather \
         than on the model's own stop: {summary}"
    );

    // The finished frame carries the summary, which has no thinking in it. The
    // record is what a pane reads back for a run it did not watch.
    let id = summary["id"].as_str().expect("the summary names the run");
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/recipes/studio/runs/{id}"))
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
    let record: Value = serde_json::from_slice(&body).expect("the record is JSON");
    let thinking = record["reasoning"].as_str().unwrap_or_default();
    assert!(
        !thinking.is_empty(),
        "the record kept what the budget was spent on: {record}"
    );
}
