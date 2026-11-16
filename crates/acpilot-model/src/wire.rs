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
    /// The model's thinking, which reasoning servers send beside `content`.
    ///
    /// `reasoning` is the other spelling servers use for the same field, so it
    /// is accepted rather than lost.
    #[serde(default, alias = "reasoning")]
    pub reasoning_content: Option<String>,
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

/// The body for a streamed chat: [`chat_body`] plus the two keys a stream needs.
///
/// `include_usage` is asked for because a stream otherwise carries no counts at
/// all, and a run record that could hold the server's number should not fall back
/// to an estimate. Servers that reject the key are retried without it by
/// [`crate::post_chat_stream`], so asking costs nothing but one round trip.
pub(crate) fn stream_body(request: &ChatRequest) -> Map<String, Value> {
    let mut body = chat_body(request);
    body.insert("stream".to_owned(), Value::Bool(true));
    body.insert(
        "stream_options".to_owned(),
        serde_json::json!({ "include_usage": true }),
    );
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
        reasoning_content: None,
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
        reasoning: message.reasoning_content.unwrap_or_default(),
        tool_calls,
        stop_reason,
        usage,
        stats,
        unsupported_params,
    })
}

/// One frame of a streamed completion.
///
/// Both dialects stream the same shape: a `delta` with text, tool-call fragments
/// that are only complete once the stream ends, and a `finish_reason` on the last
/// frame. `error` is seen in practice from servers that fail after the headers
/// went out, and it is a value because servers disagree on whether it is a
/// string or an object.
#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<UsageBody>,
    #[serde(default)]
    error: Option<Value>,
}

/// One streamed choice.
#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// The part of the next token that arrived in this frame.
#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// The thinking behind this frame's answer, with the same two spellings a
    /// whole body allows.
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<DeltaToolCall>,
}

/// What one frame carried: the answer, the thinking that led to it, or both.
///
/// They are reported together rather than by two methods because one frame can
/// hold both, and a caller has to hand each to its own consumer in the frame's
/// order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Piece {
    /// Assistant text.
    pub answer: String,
    /// The model's thinking.
    pub reasoning: String,
}

/// A tool call, assembled from the frames that mention it.
///
/// `index` is what makes that possible: one call's name may arrive in one frame
/// and its arguments over the next ten, interleaved with a second call.
#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

/// One piece of a tool call's function.
#[derive(Debug, Default, Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Decode `text/event-stream` frames without assuming chunk boundaries.
///
/// The transport decides where a read ends, so a frame may arrive in three reads
/// and two frames in one. Only text up to a blank line is decoded; everything
/// after it stays in the buffer.
#[derive(Debug, Default)]
pub(crate) struct Sse {
    buffer: String,
    done: bool,
}

impl Sse {
    /// Feed bytes and take every complete event's payload.
    pub(crate) fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.push_str(chunk);
        // A `\r\n` may be split across two reads, so normalise the whole buffer
        // rather than the arriving slice.
        self.buffer = self.buffer.replace("\r\n", "\n");

        let mut payloads = Vec::new();
        while let Some(end) = self.buffer.find("\n\n") {
            let frame: String = self.buffer.drain(..end + 2).collect();
            let data: Vec<&str> = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|value| value.trim_start())
                .collect();
            if data.is_empty() {
                // A comment (`: keep-alive`) or a field with no data.
                continue;
            }
            let payload = data.join("\n");
            if payload == "[DONE]" {
                self.done = true;
                break;
            }
            payloads.push(payload);
        }
        payloads
    }

    /// Whether the server sent the `[DONE]` sentinel.
    pub(crate) fn done(&self) -> bool {
        self.done
    }
}

/// A completion assembled from streamed deltas.
#[derive(Debug, Default)]
pub(crate) struct Streamed {
    content: String,
    reasoning: String,
    calls: Vec<(String, String)>,
    finish: Option<String>,
    finished: bool,
    usage: Option<UsageBody>,
}

impl Streamed {
    /// Fold one frame in and return the text it carried, if any.
    pub(crate) fn push(&mut self, frame: Value) -> Result<Piece, String> {
        let chunk: StreamChunk = serde_json::from_value(frame)
            .map_err(|error| format!("unreadable stream chunk: {error}"))?;

        if let Some(error) = chunk.error {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| error.as_str().map(str::to_owned))
                .unwrap_or_else(|| "the server reported an error mid-stream".to_owned());
            return Err(message);
        }

        let mut piece = Piece::default();
        for choice in chunk.choices {
            if let Some(content) = choice.delta.content {
                piece.answer.push_str(&content);
            }
            if let Some(reasoning) = choice.delta.reasoning_content {
                piece.reasoning.push_str(&reasoning);
            }
            for call in choice.delta.tool_calls {
                while self.calls.len() <= call.index {
                    self.calls.push((String::new(), String::new()));
                }
                let slot = &mut self.calls[call.index];
                if let Some(function) = call.function {
                    if let Some(name) = function.name.filter(|name| !name.is_empty()) {
                        slot.0 = name;
                    }
                    if let Some(arguments) = function.arguments {
                        slot.1.push_str(&arguments);
                    }
                }
            }
            if choice.finish_reason.is_some() {
                self.finish = choice.finish_reason;
                self.finished = true;
            }
        }
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage);
        }

        self.content.push_str(&piece.answer);
        self.reasoning.push_str(&piece.reasoning);
        Ok(piece)
    }

    /// The assembled answer, or why the stream cannot be one.
    ///
    /// A stream that stops without a `finish_reason` and without `[DONE]` is an
    /// error, never a short answer: a silent truncation is the one failure a
    /// model lab must not present as a result.
    pub(crate) fn finish(
        self,
        done: bool,
        unsupported: Vec<String>,
    ) -> Result<ChatResponse, String> {
        if !self.finished && !done {
            return Err("the stream ended before the answer finished".to_owned());
        }

        Ok(ChatResponse {
            content: self.content,
            reasoning: self.reasoning,
            tool_calls: self
                .calls
                .into_iter()
                .filter(|(name, _)| !name.is_empty())
                .map(|(name, arguments)| ToolCall { name, arguments })
                .collect(),
            stop_reason: self.finish,
            usage: match self.usage {
                Some(usage) => Usage {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    reported: true,
                },
                None => Usage {
                    reported: false,
                    ..Usage::default()
                },
            },
            // A stream reports text as it goes and timings not at all. A number
            // here would be ours, not the server's, and the lab draws those
            // differently on purpose.
            stats: Stats::default(),
            unsupported_params: unsupported,
        })
    }
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
    fn a_frame_split_across_reads_is_decoded_once() {
        let mut sse = Sse::default();
        assert!(sse.push("data: one").is_empty(), "no blank line yet");
        assert_eq!(
            sse.push(" and a half\n\ndata: two\n\n"),
            vec!["one and a half", "two"]
        );
    }

    #[test]
    fn comments_and_other_fields_are_not_payload() {
        let mut sse = Sse::default();
        assert!(
            sse.push(": keep-alive\n\nevent: message\nid: 7\n\n")
                .is_empty()
        );
        assert!(sse.push("data: [DONE]\n\n").is_empty());
        assert!(sse.done(), "[DONE] is the sentinel, not a chunk");
    }

    #[test]
    fn tool_call_fragments_are_joined_by_index() {
        let mut streamed = Streamed::default();
        streamed
            .push(serde_json::json!({
                "choices": [{ "delta": { "tool_calls": [
                    { "index": 0, "function": { "name": "read_file" } }
                ] } }]
            }))
            .expect("a frame");
        streamed
            .push(serde_json::json!({
                "choices": [{ "delta": { "tool_calls": [
                    { "index": 0, "function": { "arguments": "{\"path\":" } },
                    { "index": 1, "function": { "name": "run_command" } }
                ] } }]
            }))
            .expect("a frame");
        streamed
            .push(serde_json::json!({
                "choices": [{ "delta": { "tool_calls": [
                    { "index": 0, "function": { "arguments": "\"/a.rs\"}" } }
                ] }, "finish_reason": "tool_calls" }]
            }))
            .expect("a frame");

        let answer = streamed.finish(false, Vec::new()).expect("an answer");
        assert_eq!(answer.tool_calls.len(), 2);
        assert_eq!(answer.tool_calls[0].name, "read_file");
        assert_eq!(answer.tool_calls[0].arguments, "{\"path\":\"/a.rs\"}");
        assert_eq!(answer.tool_calls[1].name, "run_command");
        assert_eq!(answer.tool_calls[1].arguments, "");
        assert_eq!(answer.stop_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn a_body_carries_thinking_apart_from_the_answer() {
        let body = serde_json::json!({
            "choices": [{
                "message": { "content": "", "reasoning_content": "I should say hi." },
                "finish_reason": "length"
            }]
        });
        let response = parse(body, Vec::new()).expect("parses");
        assert_eq!(response.reasoning, "I should say hi.");
        assert_eq!(
            response.content, "",
            "thinking is what the model wrote before the answer, not the answer"
        );
    }

    #[test]
    fn a_streams_thinking_is_reported_beside_its_answer() {
        let mut streamed = Streamed::default();
        let thinking = streamed
            .push(serde_json::json!({
                "choices": [{ "delta": { "reasoning": "weighing it" } }]
            }))
            .expect("a frame");
        assert_eq!(
            thinking,
            Piece {
                answer: String::new(),
                reasoning: "weighing it".to_owned(),
            },
            "the other spelling of reasoning is thinking too"
        );

        let answer = streamed
            .push(serde_json::json!({
                "choices": [{ "delta": { "content": "hi" }, "finish_reason": "stop" }]
            }))
            .expect("a frame");
        assert_eq!(answer.answer, "hi");
        assert!(answer.reasoning.is_empty());

        let response = streamed.finish(false, Vec::new()).expect("an answer");
        assert_eq!(response.content, "hi");
        assert_eq!(response.reasoning, "weighing it");
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
