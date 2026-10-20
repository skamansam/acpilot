//! Provider tests against a real HTTP server on a loopback port.
//!
//! A stub that returns `Value`s would test the mapping and nothing else. These
//! tests go through `reqwest`, which is where the interesting failures live:
//! connection refused, a `400` for an unknown parameter, a body that is not JSON,
//! and a stream of parameters that must survive the round trip unchanged.

use std::sync::{Arc, Mutex};

use acpilot_core::AcpilotError;
use acpilot_model::{Backend, ChatRequest, Provider};
use acpilot_proto::model::{
    DiscoveredModel, MetadataSource, ModelCapabilities, ModelType, ProviderKind, SamplingParams,
};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

type Log = Arc<Mutex<Vec<Value>>>;

/// Bind a server on an ephemeral port and return its base URL.
async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("a bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

fn backend(base_url: &str, kind: ProviderKind) -> Backend {
    Backend::new(Provider::new("test-provider", kind, base_url)).expect("an http client")
}

// --- LM Studio fixture -------------------------------------------------

async fn lmstudio_models() -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [
            {
                "id": "qwen2-7b-instruct",
                "object": "model",
                "type": "llm",
                "publisher": "lmstudio-community",
                "arch": "qwen2",
                "compatibility_type": "gguf",
                "quantization": "Q4_K_M",
                "state": "not-loaded",
                "max_context_length": 131072
            },
            {
                "id": "text-embedding-nomic",
                "object": "model",
                "type": "embeddings",
                "compatibility_type": "gguf",
                "state": "loaded"
            }
        ]
    }))
}

async fn lmstudio_chat(State(log): State<Log>, Json(body): Json<Value>) -> Json<Value> {
    log.lock().expect("log").push(body.clone());

    let message = if body.get("tools").is_some() {
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "ping", "arguments": "{\"value\":\"pong\"}" }
            }]
        })
    } else if body.get("response_format").is_some() {
        json!({ "role": "assistant", "content": "{\"ok\":true}" })
    } else {
        json!({ "role": "assistant", "content": "pong" })
    };

    Json(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": body["model"],
        "choices": [{ "index": 0, "message": message, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 24, "completion_tokens": 53, "total_tokens": 77 },
        "stats": {
            "tokens_per_second": 41.2,
            "time_to_first_token": 0.111,
            "stop_reason": "eosFound"
        },
        "model_info": { "arch": "qwen2", "quant": "Q4_K_M", "format": "gguf", "context_length": 4096 }
    }))
}

fn lmstudio_app(log: Log) -> Router {
    Router::new()
        .route("/api/v0/models", get(lmstudio_models))
        .route("/api/v0/chat/completions", post(lmstudio_chat))
        .with_state(log)
}

// --- OpenAI-compatible fixture -----------------------------------------

async fn openai_models() -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [
            { "id": "llama3.2", "object": "model", "owned_by": "library" }
        ]
    }))
}

async fn openai_chat(Json(body): Json<Value>) -> Json<Value> {
    Json(json!({
        "id": "chatcmpl-2",
        "object": "chat.completion",
        "model": body["model"],
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hi" },
                      "finish_reason": "stop" }]
    }))
}

fn openai_app() -> Router {
    Router::new()
        .route("/v1/models", get(openai_models))
        .route("/v1/chat/completions", post(openai_chat))
}

// --- Tests -------------------------------------------------------------

#[tokio::test]
async fn lmstudio_reports_metadata_and_keeps_gaps_unknown() {
    let base = serve(lmstudio_app(Log::default())).await;
    let models = backend(&base, ProviderKind::LmStudio)
        .models()
        .await
        .expect("the model list parses");

    assert_eq!(models.len(), 2);

    let chat = &models[0];
    assert_eq!(chat.model_type, ModelType::Chat);
    assert_eq!(chat.architecture.as_deref(), Some("qwen2"));
    assert_eq!(chat.quantization.as_deref(), Some("Q4_K_M"));
    assert_eq!(chat.loaded, Some(false));
    assert_eq!(chat.context_window, Some(131_072));
    assert_eq!(chat.context_source, MetadataSource::Server);
    assert!(chat.capabilities.chat);
    assert!(!chat.capabilities.vision);
    // Metadata is not evidence: these stay false until the probe demonstrates them.
    assert!(!chat.capabilities.tools);

    let embedding = &models[1];
    assert_eq!(embedding.model_type, ModelType::Embedding);
    assert!(
        !embedding.capabilities.chat,
        "an embedding model cannot chat"
    );
    assert_eq!(embedding.loaded, Some(true));
    assert_eq!(embedding.context_window, None);
    assert_eq!(
        embedding.context_source,
        MetadataSource::Unknown,
        "an absent window is unknown, never a default"
    );
}

#[tokio::test]
async fn openai_compatible_says_nothing_it_does_not_know() {
    let base = serve(openai_app()).await;
    let models = backend(&base, ProviderKind::OpenAiCompatible)
        .models()
        .await
        .expect("the model list parses");

    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "llama3.2");
    assert_eq!(models[0].context_window, None);
    assert_eq!(models[0].context_source, MetadataSource::Unknown);
    assert_eq!(models[0].loaded, None, "no load state is not 'not loaded'");
    assert!(models[0].capabilities.chat);
}

#[tokio::test]
async fn an_unreachable_server_is_reported_as_model_unavailable() {
    // Bind and release a port so the number is free and nothing is listening.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = probe.local_addr().expect("addr");
    drop(probe);

    let error = backend(&format!("http://{address}"), ProviderKind::LmStudio)
        .models()
        .await
        .expect_err("nothing is listening");

    assert_eq!(error.code(), "model_unavailable");
    match error {
        AcpilotError::ModelUnavailable { provider, message } => {
            assert_eq!(provider, "test-provider");
            assert!(
                message.contains("is the server running"),
                "a user should be told what to check, got {message:?}"
            );
        }
        other => panic!("wrong error: {other}"),
    }
}

#[tokio::test]
async fn a_server_error_keeps_its_status_and_detail() {
    let app = Router::new().route(
        "/api/v0/models",
        get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let base = serve(app).await;

    let error = backend(&base, ProviderKind::LmStudio)
        .models()
        .await
        .expect_err("500 is not success");

    match error {
        AcpilotError::ModelUnavailable { message, .. } => {
            assert!(message.contains("500"), "got {message:?}");
            assert!(message.contains("boom"), "the server's own words survive");
        }
        other => panic!("wrong error: {other}"),
    }
}

#[tokio::test]
async fn a_body_that_is_not_json_is_not_an_empty_list() {
    let app = Router::new().route(
        "/api/v0/models",
        get(|| async { (StatusCode::OK, "definitely not json") }),
    );
    let base = serve(app).await;

    let error = backend(&base, ProviderKind::LmStudio)
        .models()
        .await
        .expect_err("garbage must not become an empty model list");
    assert!(matches!(error, AcpilotError::ModelUnavailable { .. }));
}

#[tokio::test]
async fn chat_sends_the_parameters_it_was_given_and_reads_the_stats() {
    let log = Log::default();
    let base = serve(lmstudio_app(log.clone())).await;

    let mut request = ChatRequest::new("qwen2-7b-instruct", "ping");
    request.system_prompt = Some("You are a test.".to_owned());
    request.params = SamplingParams {
        temperature: 0.1,
        top_p: 0.9,
        max_tokens: Some(64),
        top_k: None,
        repeat_penalty: None,
        seed: Some(7),
        stop: vec!["\n\n".to_owned()],
    };

    let response = backend(&base, ProviderKind::LmStudio)
        .chat(&request)
        .await
        .expect("the chat succeeds");

    assert_eq!(response.content, "pong");
    assert!(response.usage.reported);
    assert_eq!(response.usage.prompt_tokens, 24);
    assert_eq!(response.usage.completion_tokens, 53);
    assert_eq!(response.stats.first_token_ms, Some(111));
    assert_eq!(response.stop_reason.as_deref(), Some("eosFound"));
    assert!(response.unsupported_params.is_empty());

    let sent = log.lock().expect("log")[0].clone();
    assert_eq!(sent["model"], json!("qwen2-7b-instruct"));
    assert_eq!(sent["messages"][0]["role"], json!("system"));
    assert_eq!(sent["messages"][0]["content"], json!("You are a test."));
    assert_eq!(sent["messages"][1]["role"], json!("user"));
    assert_eq!(sent["seed"], json!(7));
    assert_eq!(sent["max_tokens"], json!(64));
    assert_eq!(sent["stop"], json!(["\n\n"]));
    assert!(
        sent.get("top_k").is_none(),
        "unset optional keys are omitted"
    );
    assert_eq!(sent["stream"], json!(false));
}

#[tokio::test]
async fn a_refused_parameter_is_retried_without_it_and_reported() {
    let log = Log::default();

    async fn chat(State(log): State<Log>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        log.lock().expect("log").push(body.clone());
        if body.get("top_k").is_some() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "unsupported parameter: top_k" })),
            );
        }
        (
            StatusCode::OK,
            Json(json!({
                "choices": [{ "message": { "content": "fine" }, "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
            })),
        )
    }

    let app = Router::new()
        .route("/api/v0/chat/completions", post(chat))
        .with_state(log.clone());
    let base = serve(app).await;

    let mut request = ChatRequest::new("qwen2-7b-instruct", "ping");
    request.params.top_k = Some(5);

    let response = backend(&base, ProviderKind::LmStudio)
        .chat(&request)
        .await
        .expect("the retry succeeds");

    assert_eq!(response.content, "fine");
    assert_eq!(response.unsupported_params, vec!["top_k".to_owned()]);

    let calls = log.lock().expect("log").clone();
    assert_eq!(calls.len(), 2, "one refusal, one retry");
    assert_eq!(calls[0]["top_k"], json!(5));
    assert!(
        calls[1].get("top_k").is_none(),
        "the retry must not send the parameter the server refused"
    );
}

#[tokio::test]
async fn probe_demonstrates_tools_and_structured_output() {
    let base = serve(lmstudio_app(Log::default())).await;

    let probe = backend(&base, ProviderKind::LmStudio)
        .probe("qwen2-7b-instruct")
        .await
        .expect("the probe runs");

    assert!(probe.tools, "evidence: {}", probe.evidence);
    assert!(probe.structured_output);
    assert!(probe.evidence.contains("tools: called `ping`"));
    assert!(probe.evidence.contains("valid JSON object"));

    // Folding the verdict in must not touch what the server reported.
    let mut capabilities = DiscoveredModel {
        id: "qwen2-vl".to_owned(),
        provider_id: "test-provider".to_owned(),
        model_type: ModelType::Vision,
        architecture: None,
        quantization: None,
        loaded: Some(true),
        context_window: Some(32_768),
        context_source: MetadataSource::Server,
        capabilities: ModelCapabilities {
            chat: true,
            vision: true,
            streaming: false,
            tools: false,
            structured_output: false,
        },
    }
    .capabilities;
    probe.apply_to(&mut capabilities);
    assert!(capabilities.tools, "the probe's verdict applies");
    assert!(
        capabilities.vision,
        "a probe that never saw an image cannot unset vision"
    );
    assert!(capabilities.chat);
}

#[tokio::test]
async fn a_model_that_cannot_call_tools_is_reported_honestly() {
    async fn chat(Json(body): Json<Value>) -> Json<Value> {
        // Ignores the tool schema and answers in prose, which is what a small
        // model often does. The probe must not read that as success.
        let content = if body.get("response_format").is_some() {
            "sure! here is json: {ok: true}"
        } else {
            "I would rather not use tools."
        };
        Json(json!({
            "choices": [{ "message": { "content": content }, "finish_reason": "stop" }]
        }))
    }

    let app = Router::new().route("/api/v0/chat/completions", post(chat));
    let base = serve(app).await;

    let probe = backend(&base, ProviderKind::LmStudio)
        .probe("qwen2-0.5b")
        .await
        .expect("the probe runs");

    assert!(!probe.tools);
    assert!(!probe.structured_output);
    assert!(probe.evidence.contains("no tool call"));
    assert!(probe.evidence.contains("not a JSON object"));
}
