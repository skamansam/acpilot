//! Workspaces: a root directory on a host, plus the session history attached to it.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::host::HostId;

/// Opaque workspace identifier.
pub type WorkspaceId = String;

/// A workspace as returned by `GET /v1/workspaces`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceSummary {
    /// Workspace identifier.
    pub id: WorkspaceId,
    /// Display name, defaulting to the directory's basename.
    pub name: String,
    /// Host the root lives on.
    pub host_id: HostId,
    /// Root path, as the host spells it (not translated to the client's platform).
    pub root: String,
    /// Number of sessions recorded against this workspace.
    pub session_count: u32,
}

/// Request body for `POST /v1/workspaces`.
///
/// Opening is idempotent: the same `host_id` + `root` pair resolves to the same
/// workspace, so a client can call this on every launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceOpenRequest {
    /// Host to open the workspace on.
    pub host_id: HostId,
    /// Absolute path on that host.
    pub root: String,
    /// Optional display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
