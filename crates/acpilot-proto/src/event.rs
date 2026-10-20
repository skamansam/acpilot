//! Session events: the streamed record of what an agent did.
//!
//! Every event carries a per-session monotonic sequence number, so a client that
//! reconnects can ask for everything after the last sequence it saw and lose
//! nothing. The sequence is assigned by the daemon, never by the adapter.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::session::SessionId;

/// Per-session monotonic sequence number, starting at 1.
pub type EventSeq = u64;

/// The kind of a session event.
///
/// This enum is deliberately closed: UIs switch on it exhaustively, and adding a
/// variant is a wire change that shows up in the OpenAPI snapshot. Adapter-
/// specific detail belongs in the payload, not in new kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventKind {
    /// A chunk of assistant or user message text.
    MessageChunk,
    /// A chunk of reasoning/thinking text.
    Thought,
    /// A tool call was proposed, started, or finished.
    ToolCall,
    /// The agent changed its plan.
    Plan,
    /// The session's mode changed (for example `ask` to `code`).
    ModeChanged,
    /// Informational note from the daemon or the agent.
    Notice,
    /// Context was compacted.
    Compaction,
    /// The agent's available commands changed.
    CommandsChanged,
}

/// One event on a session's stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SessionEvent {
    /// Monotonic sequence number within the session.
    pub seq: EventSeq,
    /// Session this event belongs to.
    pub session_id: SessionId,
    /// What happened.
    pub kind: SessionEventKind,
    /// RFC 3339 timestamp of when the daemon recorded the event.
    pub at: String,
    /// Kind-specific payload.
    #[schema(value_type = Object)]
    pub payload: serde_json::Value,
}
