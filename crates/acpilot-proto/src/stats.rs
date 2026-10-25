//! Integration stats: the numbers behind the bar.
//!
//! One shape covers both kinds of integration a user watches — a model provider
//! and an agent — because the questions are the same two: how much of the context
//! window is in use, and how much credit is left. The UI draws one bar per
//! integration from this document.
//!
//! The rule that matters is the same one the model lab already follows: a number
//! is only present when something actually reported it. `source` says where each
//! figure came from, and `unknown` is a value, not a zero. A bar that invents a
//! 32k window is how a prompt gets truncated without anyone noticing.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What the stats describe.
///
/// Providers and agents share the bar because they share the questions, not
/// because they share a lifecycle: a provider can serve several agents, and an
/// agent can outlive the provider that answered its last turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationKind {
    /// A declared inference endpoint, for example LM Studio or OpenRouter.
    Provider,
    /// A running agent, for example Claude Code or a promoted recipe.
    Agent,
}

/// Where one figure came from.
///
/// This is recorded per figure rather than per integration: a server may report a
/// context window but no balance, and a configured plan may name a credit ceiling
/// the server never mentions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatSource {
    /// The provider or agent reported it.
    Server,
    /// A profile or recipe stated it, for example a configured monthly budget.
    Configured,
    /// The daemon counted it locally, for example tokens in the transcript.
    /// Estimates are labelled so a UI does not style them as measured facts.
    Estimated,
    /// Nobody said. The bar renders as indeterminate, never as full or empty.
    Unknown,
}

/// Context-window usage for one integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ContextStats {
    /// Tokens currently in the conversation, including the system prompt and
    /// any corpus that was assembled for the next turn.
    pub used_tokens: u32,
    /// The window to measure against. Absent when neither the server nor the
    /// recipe states one; a client must not substitute a default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_tokens: Option<u32>,
    /// Where `used_tokens` and `window_tokens` came from.
    pub source: StatSource,
}

/// Credit balance for one integration.
///
/// Amounts are in the provider's own unit — credits, not necessarily currency —
/// and `currency` carries the label to show. Any field may be absent: some
/// providers report only a balance, some only usage, and local servers report
/// neither. [`limit`](Self::limit) is separate because a remaining balance is not
/// the same as a plan ceiling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CreditStats {
    /// Credits spent in the current billing period, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    /// Credits left, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    /// Plan ceiling for the period, when reported. With `used` this makes the
    /// bar's denominator; without it the bar is indeterminate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// What the amounts are counted in, for example `credits` or `USD`.
    pub currency: String,
    /// Where the figures came from.
    pub source: StatSource,
}

/// One integration's stats, as returned by `GET /v1/stats`.
///
/// A list of these is what the stats bar renders. Integrations with nothing
/// reported still appear, with `source: unknown`, so the bar has a stable row
/// instead of disappearing when a provider is quiet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct IntegrationStats {
    /// Provider or agent identifier.
    pub id: String,
    /// Which kind of integration this is.
    pub kind: IntegrationKind,
    /// Display label, so a bar can be drawn without a second lookup.
    pub label: String,
    /// Context usage, absent when the integration has no conversation attached
    /// (a provider that has not served a turn, for example).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextStats>,
    /// Credit balance, absent for local servers and for plans that do not
    /// expose one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<CreditStats>,
    /// RFC 3339 timestamp of the last observation. A stale bar is better than a
    /// silently frozen one, so the age is on the wire.
    pub updated_at: String,
}
