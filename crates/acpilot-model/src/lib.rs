//! Local model providers: discovery, chat, and capability probes.
//!
//! Two facts shape this crate. First, a model server is an HTTP endpoint with a
//! model list and a chat route, so the interesting work is not the transport; it
//! is deciding what the server *actually said* and refusing to invent the rest.
//! Second, servers differ far more in what they report than in what they accept,
//! which is why [`ProviderKind`] has two variants and no more: one native surface
//! with metadata, one OpenAI-shaped surface without it.
//!
//! Everything here is I/O and translation. There is no agent loop, no prompt
//! assembly, and no policy: [`Backend::chat`] sends what it is given and returns
//! what came back, including the parameters the server refused.

use std::time::Duration;

use acpilot_core::AcpilotError;
use acpilot_proto::model::{
    DiscoveredModel, MetadataSource, ModelCapabilities, ProviderKind, SamplingParams,
};
use serde_json::{Map, Value};

mod lmstudio;
mod openai;
mod wire;

pub use lmstudio::LmStudio;
pub use openai::OpenAi;

/// How long discovery may take before the provider is called unreachable.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a single generation may take. Generous on purpose: a 70B model on a
/// laptop is slow, not broken, and cancelling a turn is the user's job.
pub const CHAT_TIMEOUT: Duration = Duration::from_secs(300);
/// Connection timeout. A local server either answers the TCP handshake at once or
/// is not running; waiting longer only delays the honest error.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Optional parameters that a server may reject. They are removed one at a time
/// on a `400`, and the removal is reported rather than hidden.
const OPTIONAL_PARAMS: [&str; 5] = ["top_k", "repeat_penalty", "seed", "stop", "response_format"];

/// A declared model server.
///
/// The token is passed in at construction and never stored anywhere else: this
/// type is the thing that makes the request, so it is the one place a credential
/// has to exist.
#[derive(Debug, Clone)]
pub struct Provider {
    /// Identifier used by recipes.
    pub id: String,
    /// Wire dialect.
    pub kind: ProviderKind,
    /// Base URL without a trailing slash, for example `http://127.0.0.1:1234`.
    pub base_url: String,
    /// Bearer token, when the server wants one.
    pub token: Option<String>,
}

impl Provider {
    /// Build a provider with no credential.
    #[must_use]
    pub fn new(id: impl Into<String>, kind: ProviderKind, base_url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            token: None,
        }
    }

    /// Attach a bearer token, for servers started with one.
    #[must_use]
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    /// Join a path onto the base URL.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }
}

/// Build an HTTP client with the connection timeout this crate promises.
///
/// Shared with the adapters so "2 seconds to refuse" is one decision, not two.
pub fn client() -> Result<reqwest::Client, AcpilotError> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|error| transport_error("http client", &error))
}

/// Who said a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Instructions that frame the turn.
    System,
    /// The corpus or the question.
    User,
    /// A previous answer, when replaying a conversation.
    Assistant,
    /// A tool result handed back to the model.
    Tool,
}

impl Role {
    /// Wire name, which both adapters share.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// One turn in the conversation sent to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// Speaker.
    pub role: Role,
    /// Text. Tool results are text too: the schema lives in the tool spec.
    pub content: String,
}

/// A tool offered to the model.
///
/// The schema is JSON Schema, passed through unchanged. Nothing here decides
/// whether the tool may *run* — that is the permission broker's business.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// Function name the model must call.
    pub name: String,
    /// What it does, in the model's terms.
    pub description: String,
    /// JSON Schema for the arguments.
    pub schema: Value,
}

/// One request to a model.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    /// Model identifier, exactly as the provider reported it.
    pub model: String,
    /// When set, sent as a leading system message.
    pub system_prompt: Option<String>,
    /// Conversation so far.
    pub messages: Vec<ChatMessage>,
    /// Sampling knobs.
    pub params: SamplingParams,
    /// Tools offered this turn.
    pub tools: Vec<ToolSpec>,
    /// When set, ask the server to constrain output to this schema.
    pub json_schema: Option<Value>,
}

impl ChatRequest {
    /// A one-shot request with default sampling.
    #[must_use]
    pub fn new(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system_prompt: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: prompt.into(),
            }],
            params: SamplingParams::default(),
            tools: Vec::new(),
            json_schema: None,
        }
    }
}

/// A tool call the model produced.
///
/// `arguments` is the raw JSON string from the wire, deliberately: parsing it is
/// the caller's job, and a caller that parses it can report a repair instead of
/// pretending the call was valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Function name.
    pub name: String,
    /// Raw JSON arguments.
    pub arguments: String,
}

/// Token counts, and whether the server told us or we counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Prompt tokens.
    pub prompt_tokens: u32,
    /// Completion tokens.
    pub completion_tokens: u32,
    /// `true` when these came from the server's `usage` field rather than from a
    /// heuristic. The lab draws them differently, and so should anything else.
    pub reported: bool,
}

/// Timings the provider volunteered.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Stats {
    /// Milliseconds until the first token.
    pub first_token_ms: Option<u64>,
    /// Generated tokens per second.
    pub tokens_per_second: Option<f32>,
}

/// One answer from a model.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    /// Assistant text, empty when the answer was only tool calls.
    pub content: String,
    /// Tool calls in the order the model produced them.
    pub tool_calls: Vec<ToolCall>,
    /// Why generation stopped, as the server phrased it.
    pub stop_reason: Option<String>,
    /// Token counts.
    pub usage: Usage,
    /// Timings, when the provider reports them.
    pub stats: Stats,
    /// Parameters this server refused and the request was retried without.
    ///
    /// A knob that silently does nothing is worse than no knob, so a refusal
    /// becomes a visible warning on the run instead of a quiet default.
    pub unsupported_params: Vec<String>,
}

/// Result of a capability probe, with the evidence that produced it.
///
/// Only the dimensions that were actually tested appear here. Vision and
/// streaming are not probed, so they are absent rather than `false`: a `false`
/// that means "not tested" is the same lie as a context window that means "we
/// could not find one". Fold the verdict in with [`Probe::apply_to`], which
/// leaves everything discovery established untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The model emitted a tool call when offered a trivial tool.
    pub tools: bool,
    /// The model produced a JSON object when constrained to a schema.
    pub structured_output: bool,
    /// One line per test: what was asked, what came back.
    pub evidence: String,
}

impl Probe {
    /// Apply the probed dimensions to a model's capabilities.
    ///
    /// Only these two fields move. A `vision: true` from the model list survives a
    /// probe that never looked at an image.
    pub fn apply_to(&self, capabilities: &mut ModelCapabilities) {
        capabilities.tools = self.tools;
        capabilities.structured_output = self.structured_output;
    }
}

/// A provider's wire dialect, dispatched without `dyn`.
///
/// Two variants is not enough to justify a boxed trait object and the dependency
/// that makes async methods object-safe. An enum keeps the call sites identical
/// and the compiler checking every match.
#[derive(Debug, Clone)]
pub enum Backend {
    /// LM Studio's native `/api/v0` surface.
    LmStudio(LmStudio),
    /// Any server speaking OpenAI's chat completions shape.
    OpenAiCompatible(OpenAi),
}

impl Backend {
    /// Choose the adapter for a provider, with a default HTTP client.
    pub fn new(provider: Provider) -> Result<Self, AcpilotError> {
        Ok(Self::with_client(provider, client()?))
    }

    /// Choose the adapter with a caller-supplied client, so connections are pooled
    /// across providers and tests can share one.
    #[must_use]
    pub fn with_client(provider: Provider, client: reqwest::Client) -> Self {
        match provider.kind {
            ProviderKind::LmStudio => Self::LmStudio(LmStudio::new(provider, client)),
            ProviderKind::OpenAiCompatible => Self::OpenAiCompatible(OpenAi::new(provider, client)),
        }
    }

    /// The provider behind this adapter.
    #[must_use]
    pub fn provider(&self) -> &Provider {
        match self {
            Self::LmStudio(backend) => backend.provider(),
            Self::OpenAiCompatible(backend) => backend.provider(),
        }
    }

    /// List the models this provider reports.
    ///
    /// Nothing is cached and nothing is retried in the background; a probe is a
    /// question, and a question asked once is answered once.
    pub async fn models(&self) -> Result<Vec<DiscoveredModel>, AcpilotError> {
        match self {
            Self::LmStudio(backend) => backend.models().await,
            Self::OpenAiCompatible(backend) => backend.models().await,
        }
    }

    /// Send one chat request.
    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, AcpilotError> {
        match self {
            Self::LmStudio(backend) => backend.chat(request).await,
            Self::OpenAiCompatible(backend) => backend.chat(request).await,
        }
    }

    /// Ask a loaded model to demonstrate tools and structured output.
    ///
    /// Two requests, both cheap: one offers a single trivial tool, one demands a
    /// JSON object. A model that claims tool support in its metadata and then
    /// cannot emit a call is reported as `tools: false` with the evidence
    /// attached — that gap is exactly what ruins a promoted agent later.
    pub async fn probe(&self, model: &str) -> Result<Probe, AcpilotError> {
        let mut evidence = Vec::new();

        let mut tool_request = ChatRequest::new(model, "Call the ping tool with value \"pong\".");
        tool_request.tools = vec![ping_tool()];
        tool_request.params.temperature = 0.0;
        let tool_answer = self.chat(&tool_request).await?;
        let tools = !tool_answer.tool_calls.is_empty();
        evidence.push(if tools {
            format!("tools: called `{}`", tool_answer.tool_calls[0].name)
        } else {
            format!(
                "tools: no tool call (answer began {:?})",
                tool_answer.content.chars().take(60).collect::<String>()
            )
        });

        let mut json_request = ChatRequest::new(
            model,
            "Reply with a JSON object with one key, \"ok\", set to true.",
        );
        json_request.json_schema = Some(serde_json::json!({
            "type": "object",
            "properties": { "ok": { "type": "boolean" } },
            "required": ["ok"],
            "additionalProperties": false
        }));
        json_request.params.temperature = 0.0;
        let json_answer = self.chat(&json_request).await?;
        let structured_output = serde_json::from_str::<Value>(json_answer.content.trim())
            .map(|value| value.is_object())
            .unwrap_or(false);
        evidence.push(format!(
            "structured output: {}",
            if structured_output {
                "valid JSON object"
            } else {
                "not a JSON object"
            }
        ));

        Ok(Probe {
            tools,
            structured_output,
            evidence: evidence.join("; "),
        })
    }
}

/// The one tool a probe offers. Trivial by design: the question is whether the
/// model can emit a call at all, not whether it can use a tool well.
fn ping_tool() -> ToolSpec {
    ToolSpec {
        name: "ping".to_owned(),
        description: "Echo a value back. Use it whenever the user asks you to ping.".to_owned(),
        schema: serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"]
        }),
    }
}

/// POST a chat body, dropping optional parameters the server refuses.
///
/// A `400`/`422` is retried with one optional parameter removed at a time, up to
/// the number of optional parameters. Everything else — a refused connection, a
/// `500`, a body that is not JSON — is reported as-is: guessing again would turn
/// a broken server into a slow one.
pub(crate) async fn post_chat(
    client: &reqwest::Client,
    provider: &Provider,
    url: &str,
    body: &mut Map<String, Value>,
) -> Result<(Value, Vec<String>), AcpilotError> {
    let mut unsupported = Vec::new();

    loop {
        let mut request = client
            .post(url)
            .timeout(CHAT_TIMEOUT)
            .json(&Value::Object(body.clone()));
        if let Some(token) = &provider.token {
            request = request.bearer_auth(token);
        }

        let response = request
            .send()
            .await
            .map_err(|error| transport_error(&provider.id, &error))?;
        let status = response.status();

        if status.is_success() {
            let value: Value = response
                .json()
                .await
                .map_err(|error| unexpected(&provider.id, &error))?;
            return Ok((value, unsupported));
        }

        let refused = status.as_u16() == 400 || status.as_u16() == 422;
        let detail = response.text().await.unwrap_or_default();
        let next = if refused {
            OPTIONAL_PARAMS
                .iter()
                .find(|name| body.contains_key(**name))
                .copied()
        } else {
            None
        };

        let Some(name) = next else {
            return Err(AcpilotError::ModelUnavailable {
                provider: provider.id.clone(),
                message: format!(
                    "server answered {status}{}",
                    if detail.trim().is_empty() {
                        String::new()
                    } else {
                        format!(": {}", truncate(&detail, 400))
                    }
                ),
            });
        };

        tracing::debug!(
            provider = %provider.id,
            parameter = name,
            "server refused an optional parameter; retrying without it"
        );
        body.remove(name);
        unsupported.push(name.to_owned());
    }
}

/// Error for a request that never reached the server, or reached it and failed
/// at the transport level.
pub(crate) fn transport_error(provider: &str, error: &reqwest::Error) -> AcpilotError {
    let message = if error.is_connect() {
        "could not connect — is the server running?".to_owned()
    } else if error.is_timeout() {
        "timed out".to_owned()
    } else {
        error.to_string()
    };
    AcpilotError::ModelUnavailable {
        provider: provider.to_owned(),
        message,
    }
}

/// Error for a body that is not the shape the dialect promises.
pub(crate) fn unexpected(provider: &str, error: &impl std::fmt::Display) -> AcpilotError {
    AcpilotError::ModelUnavailable {
        provider: provider.to_owned(),
        message: format!("unexpected response: {}", truncate(&error.to_string(), 400)),
    }
}

/// Keep server text short enough to show in a log line or a toast.
pub(crate) fn truncate(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_owned();
    }
    let kept: String = trimmed.chars().take(limit).collect();
    format!("{kept}…")
}

/// Metadata source for a value the server did or did not provide.
pub(crate) fn source(value: Option<u32>) -> MetadataSource {
    if value.is_some() {
        MetadataSource::Server
    } else {
        MetadataSource::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_join_without_doubling_slashes() {
        let provider = Provider::new("lm", ProviderKind::LmStudio, "http://127.0.0.1:1234/");
        assert_eq!(
            provider.url("/api/v0/models"),
            "http://127.0.0.1:1234/api/v0/models"
        );
        assert_eq!(
            provider.url("api/v0/models"),
            "http://127.0.0.1:1234/api/v0/models"
        );
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("héllo", 3), "hél…");
        assert_eq!(truncate("  short  ", 10), "short");
    }

    #[test]
    fn an_unreported_context_window_is_unknown_not_a_default() {
        assert_eq!(source(None), MetadataSource::Unknown);
        assert_eq!(source(Some(4096)), MetadataSource::Server);
    }
}
