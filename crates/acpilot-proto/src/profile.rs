//! Profiles: a named identity (agents, credentials, policy, preferences).
//!
//! The wire surface exposes summaries and preferences. Secrets never cross this
//! boundary: `GET /v1/profiles` reports which agents have a credential, never the
//! credential itself.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Opaque profile identifier, derived from its directory name.
pub type ProfileId = String;

/// A profile as returned by `GET /v1/profiles`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProfileSummary {
    /// Profile identifier.
    pub id: ProfileId,
    /// Human-readable label.
    pub label: String,
    /// Whether this is the daemon's currently active profile.
    pub active: bool,
    /// Number of configured agents.
    pub agent_count: u32,
    /// Number of workspaces opened under this profile.
    pub workspace_count: u32,
}

/// Editor and UI preferences, per profile.
///
/// Defaults are data (`defaults.toml`), not code: adding a preference should be a
/// one-line change that every UI picks up, and unknown keys are reported rather
/// than ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Preferences {
    /// Schema version of this document, so migrations are explicit.
    pub schema_version: u32,
    /// Columns per tab stop.
    pub tab_width: u8,
    /// Insert spaces instead of tab characters.
    pub insert_spaces: bool,
    /// Run the formatter on save when one is available.
    pub format_on_save: bool,
    /// Strip trailing whitespace on save.
    pub trim_trailing_whitespace: bool,
    /// Soft-wrap long lines.
    pub wrap: bool,
    /// Theme identifier, shared by TUI, GUI and web.
    pub theme: String,
    /// Lowest diagnostic severity shown: `error`, `warning`, `information`, `hint`.
    pub diagnostics_min_severity: String,
}
