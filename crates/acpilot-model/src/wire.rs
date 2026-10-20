//! The parts of the chat-completions shape that both dialects share.
//!
//! LM Studio's native surface and the OpenAI-compatible one agree on messages,
//! choices, tool calls, and usage; they disagree on metadata and timings. Keeping
//! the shared part here means a fix to tool-call parsing is one fix, not two that
//! will drift apart.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{ChatRequest, ChatResponse, Stats, ToolCall, Usage};

/// A response body from either dialect.
#[derive(Debug, Deserialize)]
pub(crate) struct Completion {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<UsageBody>,
    #[serde(default)]
    pub stats: Option<StatsBody>,
}

/// One choice. We only ever ask for one.
#[derive(Debug, Deserialize)]
pub(crate) struct Choice {
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The assistant turn.
#[derive(Debug, Deserialize)]
pub(crate) struct Message {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallBody>,
}

/// A tool call envelope, which both dialects wrap the same way.
#[derive(Debug, Deserialize)]
pub(crate) struct ToolCallBody {
    pub function: FunctionCall,
}

/// The call itself.
#[derive(Debug, Deserialize)]
pub(crate) struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

/// Token counts.
#[derive(Debug, Deserialize)]
pub(crate) struct UsageBody {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

/// LM Studio's extra measurements. Seconds on the wire, milliseconds in our types,
/// because every other duration in the product is milliseconds.
#[derive(Debug, Deserialize)]
pub(crate) struct StatsBody {
    #[serde(default)]
    pub time_to_first_token: Option<f64>,
    #[serde(default)]
    pub tokens_per_second: Option<f64>,
    #[serde(default)]
    pub stop_reason: Option<String>,
}

/// Build a chat body: identical for both dialects, optional keys omitted.
///
/// Optional keys are omitted rather than sent as `null` so the retry path in
/// [`crate::post_chat`] can tell "not asked for" from "asked for and refused".
pub(crate) fn chat_body(request: &ChatRequest) -> Map<String, Value> {
    let mut messages: Vec<Value> = Vec::with_capacity(request.messages.len() + 1);
    if let Some(system) = &request.system_prompt {
        messages.push(serde_json::json!({ "role": "system", "content": system }));
    }
    for message in &request.messages {
        messages.push(serde_json::json!({
            "role": message.role.as_str(),
            "content": message.content
        }));
    }

    let params = &request.params;
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(request.model.clone()));
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("temperature".to_owned(), number(params.temperature));
    body.insert("top_p".to_owned(), number(params.top_p));
    body.insert("stream".to_owned(), Value::Bool(false));

    if let Some(max_tokens) = params.max_tokens {
        body.insert("max_tokens".to_owned(), Value::from(max_tokens));
    }
    if let Some(top_k) = params.top_k {
        body.insert("top_k".to_owned(), Value::from(top_k));
    }
    if let Some(penalty) = params.repeat_penalty {
        body.insert("repeat_penalty".to_owned(), number(penalty));
    }
    if let Some(seed) = params.seed {
        body.insert("seed".to_owned(), Value::from(seed));
    }
    if !params.stop.is_empty() {
        body.insert("stop".to_owned(), serde_json::json!(params.stop));
    }

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.schema
                    }
                })
            })
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }

    if let Some(schema) = &request.json_schema {
        body.insert(
            "response_format".to_owned(),
            serde_json::json!({
                "type": "json_schema",
                "json_schema": { "name": "answer", "schema": schema, "strict": true }
            }),
        );
    }

    body
}

/// `f32` into JSON without the `1.0` becoming a string or a null.
fn number(value: f32) -> Value {
    Value::from(f64::from(value))
}

/// Turn a response body into our types, or explain what was wrong with it.
///
/// The error is a string because the caller wraps it in `ModelUnavailable` with
/// the provider's name; there is nothing to decide here.
pub(crate) fn parse(value: Value, unsupported_params: Vec<String>) -> Result<ChatResponse, String> {
    let completion: Completion =
        serde_json::from_value(value).map_err(|error| format!("unreadable body: {error}"))?;

    let choice = completion
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| "no choices in response".to_owned())?;
    let message = choice.message.unwrap_or(Message {
        content: None,
        tool_calls: Vec::new(),
    });

    let tool_calls = message
        .tool_calls
        .into_iter()
        .map(|call| ToolCall {
            name: call.function.name,
            arguments: call.function.arguments,
        })
        .collect();

    let usage = match &completion.usage {
        Some(usage) => Usage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            reported: true,
        },
        None => Usage {
            reported: false,
            ..Usage::default()
        },
    };

    let stop_reason = completion
        .stats
        .as_ref()
        .and_then(|stats| stats.stop_reason.clone())
        .or(choice.finish_reason);

    let stats = completion
        .stats
        .map(|stats| Stats {
            first_token_ms: stats.time_to_first_token.map(|seconds| {
                // Seconds to milliseconds; the cast is safe for any real latency.
                (seconds * 1000.0).round().max(0.0) as u64
            }),
            tokens_per_second: stats.tokens_per_second.map(|rate| rate.max(0.0) as f32),
        })
        .unwrap_or_default();

    Ok(ChatResponse {
        content: message.content.unwrap_or_default(),
        tool_calls,
        stop_reason,
        usage,
        stats,
        unsupported_params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_usage_is_marked_unreported() {
        let body = serde_json::json!({
            "choices": [{ "message": { "content": "hi" }, "finish_reason": "stop" }]
        });
        let response = parse(body, Vec::new()).expect("parses");
        assert_eq!(response.content, "hi");
        assert!(!response.usage.reported, "no usage means not reported");
        assert_eq!(response.usage.prompt_tokens, 0);
        assert_eq!(response.stop_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn tool_calls_keep_their_raw_arguments() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [
                        { "function": { "name": "ping", "arguments": "{\"value\":\"pong\"}" } }
                    ]
                }
            }]
        });
        let response = parse(body, Vec::new()).expect("parses");
        assert_eq!(response.content, "");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "ping");
        assert_eq!(response.tool_calls[0].arguments, "{\"value\":\"pong\"}");
    }

    #[test]
    fn a_body_without_choices_is_an_error_not_an_empty_answer() {
        let body = serde_json::json!({ "choices": [] });
        assert!(parse(body, Vec::new()).is_err());
    }

    #[test]
    fn stop_reason_prefers_the_server_specific_field() {
        let body = serde_json::json!({
            "choices": [{ "message": { "content": "x" }, "finish_reason": "length" }],
            "stats": { "stop_reason": "maxPredictedTokensReached" }
        });
        let response = parse(body, Vec::new()).expect("parses");
        assert_eq!(
            response.stop_reason.as_deref(),
            Some("maxPredictedTokensReached")
        );
    }

    #[test]
    fn omitted_optional_parameters_are_absent_from_the_body() {
        let request = ChatRequest::new("m", "hi");
        let body = chat_body(&request);
        assert!(!body.contains_key("max_tokens"));
        assert!(!body.contains_key("top_k"));
        assert!(!body.contains_key("stop"));
        assert!(!body.contains_key("tools"));
        let temperature = body["temperature"].as_f64().expect("a number");
        assert!((temperature - 0.2).abs() < 1e-6, "got {temperature}");
    }
}
