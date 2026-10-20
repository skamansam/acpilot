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
/// LSP defaults to UTF-16, while the editor buffers are UTF-8 bytes; the daemon
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
}

/// A language server as returned by `GET /v1/lsp`.
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
}
