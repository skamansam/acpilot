//! Hosts: the machine whose filesystem and processes a workspace lives on.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Opaque host identifier, assigned by the daemon.
pub type HostId = String;

/// How the daemon reaches a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    /// The machine the daemon runs on.
    Local,
    /// A remote machine over SSH.
    Ssh,
    /// A WSL distribution on a Windows host.
    Wsl,
}

/// What a host can actually do.
///
/// Capabilities are discovered rather than assumed: a host that cannot spawn
/// processes still edits files, and the UIs need to know that before offering an
/// agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HostCapabilities {
    /// Can the daemon read and write files here?
    pub filesystem: bool,
    /// Can the daemon allocate a PTY here?
    pub pty: bool,
    /// Can the daemon spawn agent processes here?
    pub spawn: bool,
    /// Can the daemon run language servers here?
    pub lsp: bool,
}

/// A host as returned by `GET /v1/hosts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HostSummary {
    /// Host identifier.
    pub id: HostId,
    /// Human-readable label.
    pub label: String,
    /// Transport used to reach the host.
    pub kind: HostKind,
    /// Whether the last connection attempt succeeded.
    pub connected: bool,
    /// Negotiated capabilities, absent until the host has been reached once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<HostCapabilities>,
}
