//! Sessions: one conversation between a workspace, an agent, and a user.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::agent::AgentId;
use crate::workspace::WorkspaceId;

/// Opaque session identifier.
pub type SessionId = String;

/// Where a session is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Connected and waiting for a prompt.
    Idle,
    /// A turn is in flight.
    Busy,
    /// Blocked on a permission decision.
    AwaitingPermission,
    /// The agent process ended.
    Closed,
    /// The last turn failed.
    Failed,
}

/// A session as returned by `GET /v1/sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SessionSummary {
    /// Session identifier.
    pub id: SessionId,
    /// Profile that owns this session.
    ///
    /// The daemon routes by profile, so this is normally the profile whose
    /// header selected the router; it is carried so a stored row and a live
    /// summary say the same thing.
    pub profile: String,
    /// Workspace the session edits.
    pub workspace_id: WorkspaceId,
    /// Agent driving the session.
    pub agent_id: AgentId,
    /// Display title, derived from the first prompt when not set.
    pub title: String,
    /// Lifecycle state.
    pub status: SessionStatus,
    /// RFC 3339 timestamp of creation.
    pub created_at: String,
    /// RFC 3339 timestamp of the last event.
    pub updated_at: String,
}

/// Request body for `POST /v1/sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    /// Workspace to attach the session to.
    pub workspace_id: WorkspaceId,
    /// Agent to run.
    pub agent_id: AgentId,
    /// Optional title; the first prompt supplies one otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Request body for `POST /v1/sessions/{id}/prompt`.
///
/// `idempotency_key` exists so a retried request cannot start a second turn —
/// a dropped connection must not cost the user an agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PromptRequest {
    /// The prompt text.
    pub text: String,
    /// Client-generated key that makes retries safe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// Response to a cancel request.
///
/// `cancelled` is false when the session had no turn in flight: the caller's
/// intent ("stop") was already true, which is not an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CancelAccepted {
    /// The turn that was running, when one was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Whether a turn was in flight when the cancel arrived.
    pub cancelled: bool,
}

/// `POST /v1/sessions/{id}/mode` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SetModeRequest {
    /// The id of the mode to switch to, as the agent advertised it.
    pub mode: String,
}

/// One mode an agent offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SessionMode {
    /// Identifier the agent uses for this mode.
    pub id: String,
    /// Human-readable name shown in a picker.
    pub name: String,
    /// What the mode changes, when the agent says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The modes an agent offers, and the one it is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ModeState {
    /// The mode in effect once this answer was written.
    pub mode: String,
    /// Every mode the agent advertised, in its own order.
    pub available: Vec<SessionMode>,
}

/// Response to an accepted prompt.
///
/// The turn itself streams over SSE; this only acknowledges that it started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PromptAccepted {
    /// Session the turn belongs to.
    pub session_id: SessionId,
    /// Identifier for this turn, usable to correlate events and cancels.
    pub turn_id: String,
    /// Sequence number the client should use as `Last-Event-ID` when reconnecting.
    pub first_seq: u64,
}
