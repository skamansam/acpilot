//! Streaming against a loopback server: the deltas, and the ways a stream can
//! fail to be an answer.
//!
//! These go through `reqwest` and a real SSE body because the interesting part is
//! not the JSON: it is arriving text in order, joining tool-call fragments that
//! span frames, and refusing to present a broken or unfinished stream as a short
//! answer.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use acpilot_core::AcpilotError;
use acpilot_model::{Backend, ChatRequest, Provider};
use acpilot_proto::model::ProviderKind;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

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

fn backend(base_url: &str, kind: ProviderKind) -> Backend {
    Backend::new(Provider::new("mock", kind, base_url)).expect("the adapter builds")
}

/// Wrap frames as an SSE body, the way both dialects send them.
fn frames(frames: &[&str]) -> String {
    frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect()
}

fn sse(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

#[tokio::test]
async fn streamed_text_arrives_in_order_and_adds_up_to_the_answer() {
    async fn stream() -> Response {
        sse(frames(&[
            r#"{"choices":[{"delta":{"content":"chunk "}}]}"#,
            r#"{"choices":[{"delta":{"content":"1"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]))
    }

    let base = serve(Router::new().route("/v1/chat/completions", post(stream))).await;
    let mut deltas: Vec<String> = Vec::new();
    let answer = {
        let mut sink = |text: &str| deltas.push(text.to_owned());
        backend(&base, ProviderKind::OpenAiCompatible)
            .chat_stream(&ChatRequest::new("m", "hi"), &mut sink)
            .await
            .expect("the stream")
    };

    assert_eq!(deltas, vec!["chunk ", "1"], "each delta is delivered once");
    assert_eq!(answer.content, "chunk 1");
    assert_eq!(answer.stop_reason.as_deref(), Some("stop"));
    assert!(
        !answer.usage.reported,
        "this server sent no usage frame, and an absent count is not invented"
    );
}

#[tokio::test]
async fn a_stream_asks_for_usage_so_the_counts_are_not_an_estimate() {
    #[derive(Clone, Default)]
    struct Seen {
        bodies: Arc<Mutex<Vec<Value>>>,
    }

    async fn stream(State(seen): State<Seen>, Json(body): Json<Value>) -> Response {
        seen.bodies.lock().expect("bodies").push(body);
        sse(frames(&[
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":1}}"#,
            "[DONE]",
        ]))
    }

    let seen = Seen::default();
    let base = serve(
        Router::new()
            .route("/v1/chat/completions", post(stream))
            .with_state(seen.clone()),
    )
    .await;
    let mut sink = |_text: &str| {};
    let answer = backend(&base, ProviderKind::OpenAiCompatible)
        .chat_stream(&ChatRequest::new("m", "hi"), &mut sink)
        .await
        .expect("the stream");

    let bodies = seen.bodies.lock().expect("bodies").clone();
    assert_eq!(
        bodies[0]["stream_options"]["include_usage"], true,
        "the request asks for the counts that make the record measured: {}",
        bodies[0]
    );
    assert!(
        answer.usage.reported,
        "the usage frame is the server speaking"
    );
    assert_eq!(answer.usage.prompt_tokens, 9);
    assert_eq!(answer.usage.completion_tokens, 1);
}

#[tokio::test]
async fn lm_studio_streams_the_same_frames_on_its_own_route() {
    async fn stream() -> Response {
        sse(frames(&[
            r#"{"choices":[{"delta":{"content":"pong"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
            "[DONE]",
        ]))
    }

    let base = serve(Router::new().route("/api/v0/chat/completions", post(stream))).await;
    let mut deltas: Vec<String> = Vec::new();
    let answer = {
        let mut sink = |text: &str| deltas.push(text.to_owned());
        backend(&base, ProviderKind::LmStudio)
            .chat_stream(&ChatRequest::new("m", "ping"), &mut sink)
            .await
            .expect("the stream")
    };

    assert_eq!(deltas, vec!["pong"]);
    assert_eq!(answer.content, "pong");
    assert!(
        answer.usage.reported,
        "a usage frame is the server speaking, so it is reported"
    );
    assert_eq!(answer.usage.prompt_tokens, 5);
    assert_eq!(answer.usage.completion_tokens, 2);
}

#[tokio::test]
async fn a_server_error_after_the_first_delta_is_an_error_not_a_partial_answer() {
    async fn dies() -> Response {
        sse(frames(&[
            r#"{"choices":[{"delta":{"content":"partial"}}]}"#,
            r#"{"error":{"message":"out of memory"}}"#,
        ]))
    }

    let base = serve(Router::new().route("/v1/chat/completions", post(dies))).await;
    let mut seen: Vec<String> = Vec::new();
    let error = {
        let mut sink = |text: &str| seen.push(text.to_owned());
        backend(&base, ProviderKind::OpenAiCompatible)
            .chat_stream(&ChatRequest::new("m", "hi"), &mut sink)
            .await
            .expect_err("an error frame is not an answer")
    };

    assert_eq!(
        seen,
        vec!["partial"],
        "the text that did arrive was delivered before the failure"
    );
    assert!(
        matches!(error, AcpilotError::ModelUnavailable { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("out of memory"), "{error}");
}

#[tokio::test]
async fn a_transport_that_breaks_mid_stream_is_an_error() {
    async fn broken() -> Response {
        // The error is delayed so the headers and the first frame reach the
        // client: an immediate failure is a failed request, not a broken stream,
        // and this test is about the latter.
        let chunks = futures::stream::unfold(0u8, |state| async move {
            match state {
                0 => Some((
                    Ok::<Bytes, std::io::Error>(Bytes::from_static(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                    )),
                    1,
                )),
                1 => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Some((Err(std::io::Error::other("the server died")), 2))
                }
                _ => None,
            }
        });
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(chunks),
        )
            .into_response()
    }

    let base = serve(Router::new().route("/v1/chat/completions", post(broken))).await;
    let mut seen: Vec<String> = Vec::new();
    let error = {
        let mut sink = |text: &str| seen.push(text.to_owned());
        backend(&base, ProviderKind::OpenAiCompatible)
            .chat_stream(&ChatRequest::new("m", "hi"), &mut sink)
            .await
            .expect_err("a broken stream is not an answer")
    };
    assert_eq!(
        seen,
        vec!["partial"],
        "the text that arrived before the break was delivered"
    );
    assert!(
        matches!(error, AcpilotError::ModelUnavailable { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("stream broke"), "{error}");
}

#[tokio::test]
async fn a_stream_that_stops_without_finishing_is_an_error() {
    async fn stops_early() -> Response {
        sse(frames(&[r#"{"choices":[{"delta":{"content":"half"}}]}"#]))
    }

    let base = serve(Router::new().route("/v1/chat/completions", post(stops_early))).await;
    let mut sink = |_text: &str| {};
    let error = backend(&base, ProviderKind::OpenAiCompatible)
        .chat_stream(&ChatRequest::new("m", "hi"), &mut sink)
        .await
        .expect_err("an unfinished stream is not an answer");

    assert!(
        error
            .to_string()
            .contains("ended before the answer finished"),
        "{error}"
    );
}

#[tokio::test]
async fn tool_call_fragments_spanning_frames_become_one_call() {
    async fn tools() -> Response {
        sse(frames(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"read_file"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"/a.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]))
    }

    let base = serve(Router::new().route("/v1/chat/completions", post(tools))).await;
    let mut deltas: Vec<String> = Vec::new();
    let answer = {
        let mut sink = |text: &str| deltas.push(text.to_owned());
        backend(&base, ProviderKind::OpenAiCompatible)
            .chat_stream(&ChatRequest::new("m", "read it"), &mut sink)
            .await
            .expect("the stream")
    };

    assert!(deltas.is_empty(), "a tool call has no assistant text");
    assert_eq!(answer.tool_calls.len(), 1);
    assert_eq!(answer.tool_calls[0].name, "read_file");
    assert_eq!(answer.tool_calls[0].arguments, r#"{"path":"/a.rs"}"#);
    assert_eq!(answer.stop_reason.as_deref(), Some("tool_calls"));
}

#[tokio::test]
async fn a_refused_parameter_is_retried_before_the_stream_starts() {
    #[derive(Clone, Default)]
    struct Seen {
        calls: Arc<AtomicUsize>,
        bodies: Arc<Mutex<Vec<Value>>>,
    }

    async fn refuse_first(State(seen): State<Seen>, Json(body): Json<Value>) -> Response {
        let call = seen.calls.fetch_add(1, Ordering::SeqCst);
        seen.bodies.lock().expect("bodies").push(body.clone());
        if call == 0 && body.get("top_k").is_some() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": { "message": "unknown field `top_k`" } })),
            )
                .into_response();
        }
        sse(frames(&[
            r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]))
    }

    let seen = Seen::default();
    let base = serve(
        Router::new()
            .route("/v1/chat/completions", post(refuse_first))
            .with_state(seen.clone()),
    )
    .await;

    let mut request = ChatRequest::new("m", "hi");
    request.params.top_k = Some(40);
    let mut deltas: Vec<String> = Vec::new();
    let answer = {
        let mut sink = |text: &str| deltas.push(text.to_owned());
        backend(&base, ProviderKind::OpenAiCompatible)
            .chat_stream(&request, &mut sink)
            .await
            .expect("the retry")
    };

    assert_eq!(deltas, vec!["ok"], "the retry's text is delivered once");
    assert_eq!(answer.unsupported_params, vec!["top_k".to_owned()]);
    assert_eq!(seen.calls.load(Ordering::SeqCst), 2);
    let bodies = seen.bodies.lock().expect("bodies").clone();
    assert_eq!(
        bodies[0]["stream"], true,
        "the first attempt asked to stream"
    );
    assert!(
        bodies[1].get("top_k").is_none(),
        "the retry drops only top_k"
    );
    assert_eq!(bodies[1]["stream"], true, "the retry still streams");
}
