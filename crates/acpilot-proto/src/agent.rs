//! Agents: the programs that do the work, and how the daemon talks to them.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Opaque agent identifier, derived from the manifest name.
pub type AgentId = String;

/// Transport used to drive an agent.
///
/// `AcpStdio` is the native path. `Pty` is the program-as-agent adapter, which
/// lets an ordinary TUI program (freebuff, claude, anything) participate through
/// a transcript parser. `HttpJsonRpc` is reserved for remote agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentTransport {
    /// ACP over stdio.
    AcpStdio,
    /// A program driven through a PTY and a transcript parser.
    Pty,
    /// ACP over HTTP/JSON-RPC.
    HttpJsonRpc,
}

/// Whether an agent can be used right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Detected and runnable.
    Available,
    /// Configured, but the command was not found on the host.
    Missing,
    /// The last handshake or health check failed.
    Unhealthy,
    /// Run once, but its binary changed since it was trusted.
    Untrusted,
}

/// An agent as returned by `GET /v1/agents`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AgentSummary {
    /// Agent identifier.
    pub id: AgentId,
    /// Human-readable name.
    pub name: String,
    /// How the agent is driven.
    pub transport: AgentTransport,
    /// Current availability.
    pub status: AgentStatus,
    /// Command that will be spawned, for display and debugging.
    pub command: String,
    /// Protocol version reported by the last handshake, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
}
