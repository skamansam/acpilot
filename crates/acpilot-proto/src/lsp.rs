//! Language servers, as surfaced to UI clients.
//!
//! Clients never speak LSP: the daemon owns the servers so that a server started
//! on an SSH host or inside WSL feeds every UI identically.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Opaque language-server identifier.
pub type LspServerId = String;

/// Position encoding negotiated with a server.
///
/// LSP defaults to UTF-16, while the editor buffers are UTF-8 bytes. The daemon
/// negotiates explicitly and records what was agreed rather than assuming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PositionEncoding {
    /// Characters are UTF-8 code units.
    Utf8,
    /// Characters are UTF-16 code units (the LSP default).
    Utf16,
}

/// Lifecycle state of a language server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LspServerStatus {
    /// Process spawned, handshake in progress.
    Starting,
    /// Handshake complete.
    Ready,
    /// Server reported progress work (indexing, for example).
    Indexing,
    /// Last request timed out or the server exited.
    Crashed,
    /// Shut down because no open document needs it.
    Stopped,
    /// Approved once, but the program's bytes changed since.
    ///
    /// The one state here that is not about the process. The one a client acts on
    /// differently: a row can be stopped and approved, or running a binary nobody
    /// approved after an upgrade replaced it. Reported in place of the lifecycle state
    /// because the question. "Is this the program you approved?". Outranks "is it
    /// running", exactly as `AgentStatus::Untrusted` does for agents.
    Untrusted,
}

/// A language server as returned by `GET /v1/lsp/servers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LspServerSummary {
    /// Server identifier.
    pub id: LspServerId,
    /// Language identifiers this server is attached to.
    pub languages: Vec<String>,
    /// Command that was spawned.
    pub command: String,
    /// Current state.
    pub status: LspServerStatus,
    /// Encoding agreed during `initialize`, absent before the handshake.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<PositionEncoding>,
    /// The extension that introduced this row, absent for this build's own.
    ///
    /// A client that asks the person to trust a command has to name where the
    /// command came from. "Rust-analyzer from acpilot.rust-pack" is a question
    /// about a pin, while a bare path is a question about nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
    /// The last `textDocument/references` round trip, in milliseconds.
    ///
    /// The answer and not the index: a cold server's first answer pays for an index
    /// that is paid once. A figure that mixed the two would make every report
    /// read as slow when only the first question was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round_trip_ms: Option<u64>,
}
