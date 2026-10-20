//! Terminals: agent-owned or user-owned PTYs attached to a session.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::session::SessionId;

/// Opaque terminal identifier.
pub type TerminalId = String;

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TerminalSize {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

/// A terminal as returned by `GET /v1/sessions/{id}/terminals`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TerminalSummary {
    /// Terminal identifier.
    pub id: TerminalId,
    /// Session that owns it.
    pub session_id: SessionId,
    /// Current dimensions.
    pub size: TerminalSize,
    /// Whether the child process is still running.
    pub alive: bool,
    /// Command that was started, for display.
    pub command: String,
}
