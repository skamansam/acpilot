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

/// A terminal as returned by `GET /v1/terminals`.
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

/// Request body for `POST /v1/terminals`.
///
/// A terminal belongs to a session because that is who owns the work it is doing;
/// a shell with no session would be a second, sessionless way to run commands on
/// the daemon's host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreateTerminalRequest {
    /// Session that owns the terminal.
    pub session_id: SessionId,
    /// Program to run. Defaults to the daemon host's own shell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments for `command`, verbatim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Directory to start in, relative to the workspace root. Defaults to the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Initial dimensions. Both must be non-zero: a terminal with no columns
    /// cannot display anything, and `vt100` takes `NonZeroU16` for the same reason.
    pub size: TerminalSize,
}

/// A control frame a client sends on an attached terminal.
///
/// Binary frames are the terminal itself — bytes in, bytes out, exactly as a
/// terminal would receive them. Text frames are this type: the few instructions
/// that are not keystrokes. Mixing the two in one direction would make the
/// terminal's own text (`{"type":...}` is a plausible thing for a shell to
/// print) indistinguishable from a control message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalControl {
    /// The pane changed size. The program is told, so it can redraw.
    Resize {
        /// Columns.
        cols: u16,
        /// Rows.
        rows: u16,
    },
    /// Leave the stream open but stop listening; the process keeps running.
    Detach,
}

/// A control frame the daemon sends on an attached terminal.
///
/// `Attached` is always first and is followed by exactly `replay_bytes` of
/// binary data: the terminal's current screen, drawn with the escape sequences
/// that reproduce it. Live output follows as binary frames, and `Exit` is last.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalNotice {
    /// The attach succeeded; the screen replay follows.
    Attached {
        /// The terminal as it is now.
        terminal: TerminalSummary,
        /// Length in bytes of the binary frame that follows.
        replay_bytes: usize,
    },
    /// The process ended. The stream closes after this frame.
    Exit {
        /// Exit code, when the host reported one.
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
}
