//! The adapters against a loopback server, with the request body under a lens.
//!
//! A unit test of the body builder proves the map is built; these tests prove what
//! actually leaves the process, what happens when a server refuses a knob, and what
//! a provider that reports metadata fills in. Every handler records the body, so a
//! parameter regression is a diff in an assertion rather than a mystery in a lab.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use acpilot_core::AcpilotError;
use acpilot_model::{Backend, ChatMessage, ChatRequest, Provider, Role, ToolSpec};
use acpilot_proto::model::SamplingParams;
use acpilot_proto::model::{MetadataSource, ModelType, ProviderKind};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

/// What a mock server saw.
#[derive(Clone, Default)]
struct Seen {
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl Seen {
    fn record(&self, body: Value) -> usize {
        self.bodies.lock().expect("bodies").push(body);
        self.calls.fetch_add(1, Ordering::SeqCst)
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().expect("bodies").clone()
    }
}

/// Start a router on a loopback port and return its base URL.
async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("mock server");
    });
    format!("http://{address}")
}

/// The OpenAI-compatible adapter over a base URL.
fn openai(base_url: &str) -> Backend {
    Backend::new(Provider::new(
        "mock",
        ProviderKind::OpenAiCompatible,
        base_url,
    ))
    .expect("the adapter builds")
}

/// A one-answer chat body.
fn answer(content: &str) -> Value {
    json!({
        "choices": [{
            "message": { "content": content },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 7, "completion_tokens": 3 }
    })
}

async fn capture(State(seen): State<Seen>, Json(body): Json<Value>) -> Json<Value> {
    seen.record(body);
    Json(answer("ok"))
}

#[tokio::test]
async fn the_body_that_leaves_is_the_request_and_nothing_else() {
    let seen = Seen::default();
    let base_url = serve(
        Router::new()
            .route("/v1/chat/completions", post(capture))
            .with_state(seen.clone()),
    )
    .await;

    let mut request = ChatRequest::new("mock-1", "say hi");
    request.messages.insert(
        0,
        ChatMessage {
            role: Role::System,
            content: "be terse".to_owned(),
        },
    );
    request.params = SamplingParams {
        top_k: Some(40),
        seed: Some(7),
        ..SamplingParams::default()
    };
    request.tools = vec![ToolSpec {
        name: "read_file".to_owned(),
        description: "Read a file".to_owned(),
        schema: json!({ "type": "object" }),
    }];

    let response = openai(&base_url).chat(&request).await.expect("the call");
    assert_eq!(response.content, "ok");

    let bodies = seen.bodies();
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    assert_eq!(body["model"], "mock-1");
    assert_eq!(body["stream"], false, "this adapter does not stream yet");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["content"], "say hi");
    assert_eq!(body["top_k"], 40);
    assert_eq!(body["seed"], 7);
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    assert!(
        body.get("max_tokens").is_none(),
        "an unset parameter is omitted, not sent as null: {body}"
    );
    assert!(body.get("stop").is_none(), "an empty stop list is not sent");
}

#[tokio::test]
async fn a_server_that_refuses_a_parameter_is_asked_again_without_it() {
    async fn refuse_top_k(State(seen): State<Seen>, Json(body): Json<Value>) -> Response {
        let call = seen.record(body.clone());
        if body.get("top_k").is_some() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "unknown field `top_k`" })),
            )
                .into_response();
        }
        Json(answer(&format!("ok after {call} calls"))).into_response()
    }

    let seen = Seen::default();
    let base_url = serve(
        Router::new()
            .route("/v1/chat/completions", post(refuse_top_k))
            .with_state(seen.clone()),
    )
    .await;

    let mut request = ChatRequest::new("mock-1", "say hi");
    request.params.top_k = Some(40);
    let response = openai(&base_url).chat(&request).await.expect("the retry");

    assert_eq!(
        response.unsupported_params,
        vec!["top_k"],
        "the refused knob is named rather than hidden"
    );
    assert_eq!(seen.calls.load(Ordering::SeqCst), 2);
    let bodies = seen.bodies();
    assert_eq!(bodies[0]["top_k"], 40);
    assert!(
        bodies[1].get("top_k").is_none(),
        "the retry omits exactly the refused parameter"
    );
    assert_eq!(bodies[1]["model"], "mock-1", "the rest is unchanged");
}

#[tokio::test]
async fn a_server_that_errors_is_model_unavailable_not_a_bad_answer() {
    async fn broken(Json(_body): Json<Value>) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "out of memory" })),
        )
            .into_response()
    }

    let base_url = serve(Router::new().route("/v1/chat/completions", post(broken))).await;
    let error = openai(&base_url)
        .chat(&ChatRequest::new("mock-1", "hi"))
        .await
        .expect_err("a 500 is not an answer");
    assert!(
        matches!(error, AcpilotError::ModelUnavailable { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("out of memory"), "{error}");
}

#[tokio::test]
async fn a_tool_call_and_usage_survive_the_wire() {
    async fn tool_answer(Json(_body): Json<Value>) -> Json<Value> {
        Json(json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"/work/a.rs\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 20, "completion_tokens": 9 }
        }))
    }

    let base_url = serve(Router::new().route("/v1/chat/completions", post(tool_answer))).await;
    let response = openai(&base_url)
        .chat(&ChatRequest::new("mock-1", "read it"))
        .await
        .expect("the call");
    assert_eq!(response.content, "");
    assert_eq!(response.tool_calls[0].name, "read_file");
    assert_eq!(
        response.tool_calls[0].arguments,
        "{\"path\":\"/work/a.rs\"}"
    );
    assert_eq!(response.stop_reason.as_deref(), Some("tool_calls"));
    assert!(response.usage.reported);
    assert_eq!(response.usage.prompt_tokens, 20);
    assert_eq!(response.usage.completion_tokens, 9);
}

#[tokio::test]
async fn lm_studio_metadata_fills_only_the_gauges_it_reported() {
    async fn models() -> Json<Value> {
        Json(json!({
            "data": [
                {
                    "id": "qwen2-vl-7b-instruct",
                    "type": "vlm",
                    "arch": "qwen2_vl",
                    "quantization": "4bit",
                    "state": "not-loaded",
                    "max_context_length": 32768
                },
                {
                    "id": "nomic-embed",
                    "type": "embeddings"
                }
            ]
        }))
    }

    let base_url = serve(Router::new().route("/api/v0/models", get(models))).await;
    let backend = Backend::new(Provider::new("lm", ProviderKind::LmStudio, base_url))
        .expect("the adapter builds");
    let rows = backend.models().await.expect("the model list");

    let vision = &rows[0];
    assert_eq!(vision.id, "qwen2-vl-7b-instruct");
    assert_eq!(vision.model_type, ModelType::Vision);
    assert_eq!(vision.architecture.as_deref(), Some("qwen2_vl"));
    assert_eq!(vision.quantization.as_deref(), Some("4bit"));
    assert_eq!(vision.loaded, Some(false));
    assert_eq!(vision.context_window, Some(32768));
    assert_eq!(vision.context_source, MetadataSource::Server);

    let embedding = &rows[1];
    assert_eq!(
        embedding.context_window, None,
        "an absent window is absent, never a default"
    );
    assert_eq!(embedding.context_source, MetadataSource::Unknown);
}

#[tokio::test]
async fn a_server_with_no_model_list_is_an_error_not_an_empty_picker() {
    let base_url = serve(Router::new().route(
        "/api/v0/models",
        get(|| async { (StatusCode::NOT_FOUND, "no such route") }),
    ))
    .await;
    let backend = Backend::new(Provider::new("lm", ProviderKind::LmStudio, base_url))
        .expect("the adapter builds");
    let error = backend.models().await.expect_err("a 404 is not a list");
    assert!(matches!(error, AcpilotError::ModelUnavailable { .. }));
}
