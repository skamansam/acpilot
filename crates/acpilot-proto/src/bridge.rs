//! The ACP bridge: a remote driver attached to this daemon.
//!
//! Acpilot is normally the ACP *client*: it spawns agents and answers their
//! callbacks. The bridge flips one seat. A remote driver — Devin, a CI job, a
//! second editor — connects to `acpilotd`, opens a session here, and prompts the
//! agents that live here, while the files, the terminals, and the permission
//! broker stay on this machine.
//!
//! The distinction that matters: a driver may **type**. It may not **sign**.
//! `DriverScope::AnswerPermissions` exists, is off by default, and requires an
//! explicit grant per driver.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Opaque identifier for a connected or paired driver.
pub type DriverId = String;

/// What a driver is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriverScope {
    /// Read sessions, transcripts, and workspace metadata.
    Observe,
    /// Start sessions and send prompts.
    Prompt,
    /// Cancel a turn it started.
    Cancel,
    /// Answer permission requests. Off by default: a driver that can approve its
    /// own writes is an agent with a credit card.
    AnswerPermissions,
}

/// Transport a driver is attached over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BridgeTransport {
    /// A local process on the other end of stdin/stdout.
    Stdio,
    /// ACP over HTTP with JSON-RPC bodies.
    HttpJsonRpc,
}

/// State of a driver connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriverStatus {
    /// A pairing code was issued. Nothing is connected yet.
    PendingPairing,
    /// Connected and handshaken.
    Connected,
    /// Was connected, is not now. The record stays for the audit trail.
    Disconnected,
    /// Pairing or token was revoked by the user.
    Revoked,
}

/// The bridge as returned by `GET /v1/acp/bridge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AcpBridgeSummary {
    /// Whether the endpoint is listening at all. Off unless configured.
    pub enabled: bool,
    /// Transports currently served.
    pub transports: Vec<BridgeTransport>,
    /// Address the HTTP transport listens on, when it is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// Externally reachable URL, for the driver's configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Whether a bearer token is required. Loopback-only endpoints may disable
    /// it. Anything else may not.
    pub require_token: bool,
    /// Connected drivers.
    pub connected: u32,
    /// Drivers on record, including revoked ones.
    pub known_drivers: u32,
}

/// A driver as returned by `GET /v1/acp/drivers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AcpDriverSummary {
    /// Driver identifier, stable across reconnects.
    pub id: DriverId,
    /// Name the driver gave during the handshake.
    pub name: String,
    /// Client name and version reported by `initialize`, for example
    /// `devin 2.1`. Recorded, not trusted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Peer address, or `stdio` for a local process.
    pub peer: String,
    /// Current state.
    pub status: DriverStatus,
    /// Scopes actually granted, which may be fewer than requested.
    pub scopes: Vec<DriverScope>,
    /// RFC 3339 timestamp of the current connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected_at: Option<String>,
    /// RFC 3339 timestamp of the last request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    /// Sessions this driver has open.
    pub sessions: u32,
    /// Turns this driver has prompted. The audit trail at a glance.
    pub prompt_turns: u32,
}

/// A one-time pairing code, from `POST /v1/acp/pairings`.
///
/// The code is shown in the cockpit and typed into the driver. It is short-lived,
/// single-use, and useless without the endpoint's token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PairingCode {
    /// The code to hand to the driver, for example `ALFA-NINER`.
    pub code: String,
    /// RFC 3339 expiry. Two minutes by default.
    pub expires_at: String,
    /// Where the driver should connect, when the bridge is exposed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handshake_url: Option<String>,
}

/// Ask for a code, from the cockpit.
///
/// The person issuing the code decides whether the driver may answer permission
/// requests. A driver cannot grant itself one by asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreatePairingRequest {
    /// Allow the paired driver to answer permission requests for the turns it
    /// starts. Off by default, and off is the safe answer.
    #[serde(default)]
    pub grant_answer_permissions: bool,
}

/// What a driver gets when it claims a pair: its row, and the token once.
///
/// The token is in this response and nowhere else. The ledger keeps an Argon2id
/// hash of it, so a driver that loses it asks for another code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PairedDriver {
    /// The driver, as the cockpit will list it.
    pub driver: AcpDriverSummary,
    /// The bearer token every later request carries.
    pub token: String,
}

/// Request the driver sends over the bridge to claim a pair.
///
/// Scopes are a request, not a grant: the daemon intersects them with what the
/// user allowed when the code was issued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PairDriverRequest {
    /// The code from the cockpit.
    pub code: String,
    /// Name to record for this driver.
    pub name: String,
    /// Scopes it would like. `answer_permissions` requires an explicit grant.
    pub requested_scopes: Vec<DriverScope>,
}
