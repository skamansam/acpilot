//! Models: local inference servers, the recipes that tune them, and the runs.
//!
//! A model is not an agent. It has no session, no filesystem, and no opinion
//! about permissions; it has a chat endpoint and a context window. These types
//! describe the *settings* around that endpoint — which server, which model,
//! which prompt, which budget — so a tuned recipe can be reviewed, compared, and
//! eventually promoted into an ordinary agent manifest.
//!
//! Nothing here carries a credential. `credential_ref` names a keyring entry,
//! the same indirection profiles use.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::agent::AgentSummary;

/// Opaque provider identifier, derived from the profile's provider table.
pub type ProviderId = String;
/// Opaque recipe identifier, derived from the recipe file name.
pub type RecipeId = String;
/// Opaque identifier for one recorded run.
pub type RunId = String;

/// Which wire dialect a provider speaks.
///
/// `LmStudio` reads LM Studio's native `/api/v0` surface, which reports model
/// metadata (architecture, quantization, `max_context_length`) and per-request
/// stats such as time to first token. `OpenAiCompatible` is the plain
/// `/v1/chat/completions` shape shared by Ollama, llama.cpp, vLLM, OpenRouter and
/// hosted endpoints — fewer facts, so the UI shows fewer gauges rather than
/// inventing them.
///
/// Anthropic and Gemini are not OpenAI-shaped. They are added as new variants
/// together with their adapter, never as a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// LM Studio's native REST API.
    LmStudio,
    /// Any server that implements OpenAI's chat completions shape.
    OpenAiCompatible,
}

/// Reachability of a declared provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    /// Declared but disabled; nothing has been sent to it.
    Disabled,
    /// Answered a probe.
    Reachable,
    /// Declared and enabled, but the last probe failed.
    Unreachable,
    /// The server rejected the credential.
    Unauthorized,
}

/// Where a fact about a model came from.
///
/// The lab is explicit about this because the alternative — a plausible default
/// context window — silently truncates prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetadataSource {
    /// The server reported it.
    Server,
    /// The recipe states it.
    Recipe,
    /// Nobody said; the value is absent rather than assumed.
    Unknown,
}

/// What a model appears able to do, as far as the server admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ModelCapabilities {
    /// Accepts chat messages.
    pub chat: bool,
    /// Streams tokens rather than returning one blob.
    pub streaming: bool,
    /// Accepts tool definitions and returns tool calls.
    pub tools: bool,
    /// Honours a JSON schema for output.
    pub structured_output: bool,
    /// Accepts images alongside text.
    pub vision: bool,
}

/// Broad class of a discovered model, from the server's own typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelType {
    /// Text in, text out.
    Chat,
    /// Text and images in.
    Vision,
    /// Produces embeddings; not usable as an agent.
    Embedding,
    /// Something the server did not classify.
    Other,
}

/// A provider as returned by `GET /v1/providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProviderSummary {
    /// Identifier used in recipes.
    pub id: ProviderId,
    /// Wire dialect.
    pub kind: ProviderKind,
    /// Display label.
    pub label: String,
    /// Base URL, for example `http://127.0.0.1:1234`.
    pub base_url: String,
    /// Current reachability.
    pub status: ProviderStatus,
    /// How many models the last probe saw.
    pub model_count: u32,
    /// Keyring entry holding the bearer token, when the server wants one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// RFC 3339 timestamp of the last probe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_probed_at: Option<String>,
    /// One line of human-readable detail: why it is unreachable, which runtime
    /// answered, and so on. Never a token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Request body for `POST /v1/providers`.
///
/// A provider is inert until it is declared here *and* enabled. There is no
/// scanning for servers on the loopback interface: a network call is something
/// the user asked for, not something that happens during startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AddProviderRequest {
    /// Identifier to store it under.
    pub id: ProviderId,
    /// Wire dialect.
    pub kind: ProviderKind,
    /// Display label.
    pub label: String,
    /// Base URL, for example `http://127.0.0.1:1234`.
    pub base_url: String,
    /// Keyring entry to read a bearer token from, for servers that require one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// Whether the provider may be contacted after it is added.
    pub enabled: bool,
}

/// A model the provider reported, as returned by `GET /v1/models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DiscoveredModel {
    /// Model identifier to send in a request.
    pub id: String,
    /// Provider that reported it.
    pub provider_id: ProviderId,
    /// Broad class, from the server's typing.
    pub model_type: ModelType,
    /// Architecture as reported, for example `llama` or `qwen2_vl`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    /// Quantization as reported, for example `Q4_K_M`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    /// Whether the server currently holds it in memory. Absent when the dialect
    /// does not report load state, which is most of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loaded: Option<bool>,
    /// Context window in tokens, when the server says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Where `context_window` came from.
    pub context_source: MetadataSource,
    /// What the model appears able to do.
    pub capabilities: ModelCapabilities,
}

/// Sampling knobs, as the lab presents them.
///
/// The set is deliberately small and server-agnostic. An optional parameter the
/// server *rejects* is retried without and named in `RunSummary::warnings`: a knob
/// that silently does nothing is worse than no knob. Silent disregard cannot be
/// observed from outside a sampler, so it is not claimed either.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SamplingParams {
    /// Randomness. Agent work wants this low; drafting wants it higher.
    pub temperature: f32,
    /// Nucleus sampling cutoff.
    pub top_p: f32,
    /// Output cap; `null` means the server's own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Top-k cutoff, where the server supports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Penalty for repeating tokens, where the server supports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<f32>,
    /// Fixed seed, so a run can be reproduced on the same server and model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// Sequences that end generation.
    pub stop: Vec<String>,
}

impl Default for SamplingParams {
    /// Agent-shaped defaults: reproducible and cautious, because a tool-calling
    /// loop that improvises is a tool-calling loop that improvises a shell
    /// command.
    fn default() -> Self {
        Self {
            temperature: 0.2,
            top_p: 0.95,
            max_tokens: None,
            top_k: None,
            repeat_penalty: None,
            seed: None,
            stop: Vec::new(),
        }
    }
}

/// What to do when the assembled prompt would exceed the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    /// Drop the oldest turns first. Cheap, and usually right for chat.
    TruncateOldest,
    /// Replace older turns with a summary produced by the same model.
    Summarize,
    /// Fail the run and say how much over budget it was.
    Refuse,
}

/// The budget for one run, in tokens except where stated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ContextBudget {
    /// Total window to plan against. Defaults to the model's reported value.
    pub window: u32,
    /// Tokens held back for the answer.
    pub reserve_output: u32,
    /// What to do on overflow.
    pub strategy: ContextStrategy,
    /// Ceiling on corpus text, in bytes, before tokenisation.
    pub max_corpus_bytes: u32,
}

/// Which tools a model-backed agent may *ask* for.
///
/// This is a ceiling, not a grant. Every call still goes through the permission
/// broker; a tool left out here does not exist as far as the model is concerned,
/// which is a much better place to enforce a boundary than a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ToolExposure {
    /// Offer file reads.
    pub read_files: bool,
    /// Offer file writes.
    pub write_files: bool,
    /// Offer command execution.
    pub run_commands: bool,
    /// Offer workspace search.
    pub search: bool,
    /// Hard cap on model→tool→model steps per turn.
    pub max_steps: u32,
}

/// A tuned configuration for one model, as returned by `GET /v1/recipes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ModelRecipe {
    /// Schema version, so migrations are explicit.
    pub schema_version: u32,
    /// Identifier, also the file name under `recipes/`.
    pub id: RecipeId,
    /// Display label.
    pub label: String,
    /// Provider to call.
    pub provider_id: ProviderId,
    /// Model identifier to send.
    pub model_id: String,
    /// The master prompt. The whole point of the lab is editing this and seeing
    /// what changes.
    pub system_prompt: String,
    /// Sampling knobs.
    pub sampling: SamplingParams,
    /// Context plan.
    pub context: ContextBudget,
    /// Tool ceiling for the promoted agent.
    pub tools: ToolExposure,
    /// Free-text note: what this recipe is for, and what it is bad at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// RFC 3339 timestamp of the last edit.
    pub updated_at: String,
}

/// One way to pull text out of the workspace into the prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CorpusSelector {
    /// Glob relative to the workspace root, for example `docs/**/*.md`.
    pub glob: String,
    /// Maximum number of files to include.
    pub max_files: u32,
    /// Maximum bytes per file, so one generated lockfile cannot eat the budget.
    pub max_bytes: u32,
}

/// Request body for `POST /v1/recipes/{id}/runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RunRequest {
    /// The user message.
    pub prompt: String,
    /// Corpus pulled from the current workspace, in order.
    pub corpus: Vec<CorpusSelector>,
    /// Client-generated key that makes retries safe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// Lifecycle of a recorded run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// In flight.
    Running,
    /// Completed.
    Succeeded,
    /// Ended in an error; `message` says which.
    Failed,
    /// Cancelled by the user.
    Cancelled,
}

/// Token counts for a run, and whether they are trustworthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TokenUsage {
    /// Prompt tokens.
    pub prompt: u32,
    /// Completion tokens.
    pub completion: u32,
    /// `true` when these are the lab's estimate rather than the server's
    /// reported `usage`. The UI must not style them the same way.
    pub estimated: bool,
}

/// A run as returned by `GET /v1/recipes/{id}/runs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RunSummary {
    /// Run identifier.
    pub id: RunId,
    /// Recipe this run used.
    pub recipe_id: RecipeId,
    /// Lifecycle state.
    pub status: RunStatus,
    /// RFC 3339 timestamp of the start.
    pub started_at: String,
    /// Time to first token, when the transport reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_token_ms: Option<u64>,
    /// Wall-clock duration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
    /// Token accounting, with its provenance.
    pub tokens: TokenUsage,
    /// Throughput, when the server reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_second: Option<f32>,
    /// Why the server stopped: `stop`, `length`, or the server's own reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Things that went differently from the recipe: a parameter the server
    /// refused, a truncated corpus, a tool call that had to be repaired.
    pub warnings: Vec<String>,
    /// The parameters actually sent, so a run can be reproduced.
    pub params: SamplingParams,
    /// How many workspace files went into the prompt.
    pub corpus_files: u32,
    /// Error text when the run failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Request body for `POST /v1/recipes/{id}/promote`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PromoteRecipeRequest {
    /// Agent identifier to write into `agents.toml`.
    pub agent_id: String,
    /// Display name; the recipe label is used when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Result of promoting a recipe: an ordinary agent, plus the caveats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PromotedAgent {
    /// The agent as it will now appear in `GET /v1/agents`.
    pub agent: AgentSummary,
    /// Manifest the entry was written to, so it can be reviewed or reverted.
    pub manifest_path: String,
    /// Reasons to reconsider: a low tool-call validity rate, a model that the
    /// server has never loaded, a window larger than the host's memory.
    pub warnings: Vec<String>,
}
