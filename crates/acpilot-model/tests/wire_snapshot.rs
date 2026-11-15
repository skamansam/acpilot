//! The bytes that leave the process, frozen.
//!
//! `mock_server.rs` asserts the fields that matter and would stay green while a
//! parameter started arriving as `null`, a tool schema lost its wrapper, or a
//! message gained a key nobody meant to send. A snapshot makes each of those a
//! diff in review. Update it deliberately — `INSTA_UPDATE=always cargo test -p
//! acpilot-model`, then read the diff: a changed body is a changed contract with
//! every local model server.

use std::sync::{Arc, Mutex};

use acpilot_model::{Backend, ChatMessage, ChatRequest, Provider, Role, ToolSpec};
use acpilot_proto::model::{ProviderKind, SamplingParams};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

type Seen = Arc<Mutex<Vec<Value>>>;

/// Bind a router on an ephemeral loopback port and return its base URL.
async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock server");
    });
    format!("http://{address}")
}

/// A server that records every chat body and answers with one short turn.
fn recording(path: &str, seen: Seen) -> Router {
    async fn record(State(seen): State<Seen>, Json(body): Json<Value>) -> Json<Value> {
        seen.lock().expect("bodies").push(body);
        Json(answer("ok"))
    }
    Router::new().route(path, post(record)).with_state(seen)
}

fn answer(content: &str) -> Value {
    json!({
        "choices": [{ "message": { "content": content }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 7, "completion_tokens": 1 }
    })
}

fn backend(base_url: &str, kind: ProviderKind) -> Backend {
    Backend::new(Provider::new("mock", kind, base_url)).expect("the adapter builds")
}

/// Every knob set, so the snapshot covers the whole optional surface at once.
fn everything() -> ChatRequest {
    let mut request = ChatRequest::new("qwen2-7b-instruct", "read the file");
    request.system_prompt = Some("You are terse.".to_owned());
    request.messages.insert(
        0,
        ChatMessage {
            role: Role::Assistant,
            content: "which file?".to_owned(),
        },
    );
    request.params = SamplingParams {
        temperature: 0.2,
        top_p: 0.95,
        max_tokens: Some(256),
        top_k: Some(40),
        repeat_penalty: Some(1.1),
        seed: Some(7),
        stop: vec!["\n\n".to_owned()],
    };
    request.tools = vec![ToolSpec {
        name: "read_file".to_owned(),
        description: "Read a file".to_owned(),
        schema: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
    }];
    request.json_schema = Some(json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } }
    }));
    request
}

/// Sort every object key so the snapshot records the body, not the map
/// implementation `serde_json` happened to be built with: `preserve_order` is a
/// workspace-wide feature (an ACP dependency enables it), and ordering is not a
/// wire property.
fn canonical(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

#[tokio::test]
async fn the_chat_body_is_unchanged() {
    let seen = Seen::default();
    let openai_base = serve(recording("/v1/chat/completions", seen.clone())).await;
    let lm_base = serve(recording("/api/v0/chat/completions", seen.clone())).await;

    backend(&openai_base, ProviderKind::OpenAiCompatible)
        .chat(&everything())
        .await
        .expect("the openai-compatible call");
    backend(&lm_base, ProviderKind::LmStudio)
        .chat(&everything())
        .await
        .expect("the lm studio call");

    let bodies = seen.lock().expect("bodies").clone();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        canonical(bodies[0].clone()),
        canonical(bodies[1].clone()),
        "the dialects share one body builder; a difference here is a fork nobody asked for"
    );
    insta::assert_json_snapshot!("chat_body", canonical(bodies[0].clone()));
}

#[tokio::test]
async fn the_refusal_retry_removes_exactly_one_key() {
    async fn refuse_top_k(
        State(seen): State<Seen>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let mut bodies = seen.lock().expect("bodies");
        bodies.push(body.clone());
        let refused = bodies.len() == 1;
        drop(bodies);
        if refused {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": { "message": "unknown field `top_k`" } })),
            );
        }
        (StatusCode::OK, Json(answer("ok")))
    }

    let seen = Seen::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(refuse_top_k))
        .with_state(seen.clone());
    let base = serve(app).await;

    let response = backend(&base, ProviderKind::OpenAiCompatible)
        .chat(&everything())
        .await
        .expect("the retry");
    assert_eq!(response.unsupported_params, vec!["top_k".to_owned()]);

    let bodies = seen.lock().expect("bodies").clone();
    assert_eq!(bodies.len(), 2, "one refusal, one retry");
    insta::assert_json_snapshot!(
        "refusal_retry",
        Value::Array(bodies.into_iter().map(canonical).collect())
    );
}

#[tokio::test]
async fn a_silently_ignored_parameter_is_not_reported_as_a_refusal() {
    let seen = Seen::default();
    let base = serve(recording("/v1/chat/completions", seen.clone())).await;

    let response = backend(&base, ProviderKind::OpenAiCompatible)
        .chat(&everything())
        .await
        .expect("the call");
    assert!(
        response.unsupported_params.is_empty(),
        "a server that answers 200 and ignores the knob told us nothing, so the run says nothing: {:?}",
        response.unsupported_params
    );
    assert_eq!(response.content, "ok");
}

#[tokio::test]
async fn a_truncated_body_is_an_error_not_half_an_answer() {
    async fn truncated() -> (StatusCode, &'static str) {
        // 200, then stop mid-object: what a server that dies during a large
        // completion sends, and the one shape a naive parser turns into "".
        (
            StatusCode::OK,
            "{\"choices\": [{\"message\": {\"content\": \"par",
        )
    }

    let base = serve(Router::new().route("/v1/chat/completions", post(truncated))).await;
    let error = backend(&base, ProviderKind::OpenAiCompatible)
        .chat(&everything())
        .await
        .expect_err("half an answer is not an answer");
    assert_eq!(error.code(), "model_unavailable", "{error}");
}

#[tokio::test]
async fn the_probe_requests_are_unchanged() {
    let seen = Seen::default();
    let base = serve(recording("/v1/chat/completions", seen.clone())).await;

    let probe = backend(&base, ProviderKind::OpenAiCompatible)
        .probe("qwen2-7b-instruct")
        .await
        .expect("the probe runs");
    assert!(!probe.tools, "a prose answer is not a tool call");
    assert!(!probe.structured_output, "the model ignored the schema");

    let bodies = seen.lock().expect("bodies").clone();
    assert_eq!(bodies.len(), 2, "one tools probe, one schema probe");
    insta::assert_json_snapshot!(
        "probe_requests",
        Value::Array(bodies.into_iter().map(canonical).collect())
    );
}
