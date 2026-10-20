//! Permission requests: the broker's question and the user's answer.
//!
//! Agents ask to do things. The daemon never auto-answers on the user's behalf
//! for a write: a request becomes a pending item that a human or an explicit
//! policy rule resolves, and every resolution is audited.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::session::SessionId;

/// Opaque identifier for a pending permission request.
pub type PermissionRequestId = String;

/// What an agent is asking to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    /// Write a file.
    FileWrite,
    /// Read a file.
    FileRead,
    /// Run a command, or attach to a terminal.
    Terminal,
    /// Invoke a tool that is not a plain file operation.
    ToolCall,
    /// Anything the daemon could not classify; treated as the strictest case.
    Other,
}

/// An answer the caller may give.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOption {
    /// Allow this one request.
    AllowOnce,
    /// Allow matching requests for the rest of the session.
    AllowAlways,
    /// Refuse.
    Deny,
}

/// The decision the broker applied, with its provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PermissionDecision {
    /// What was chosen.
    pub option: PermissionOption,
    /// Who or what decided: `user`, `policy:<rule-id>`, or `timeout`.
    pub decided_by: String,
    /// RFC 3339 timestamp of the decision.
    pub at: String,
}

/// A pending request as returned by `GET /v1/permissions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PermissionRequest {
    /// Request identifier, used when answering.
    pub id: PermissionRequestId,
    /// Session that is blocked on this.
    pub session_id: SessionId,
    /// Classification of the request.
    pub kind: PermissionKind,
    /// One-line title, safe to show in a notification.
    pub title: String,
    /// Full detail: the diff, the command line, the tool payload.
    pub detail: String,
    /// Options the caller may choose from, in display order.
    pub options: Vec<PermissionOption>,
    /// RFC 3339 timestamp of when the request was raised.
    pub created_at: String,
}
