//! Profiles: a named identity (agents, credentials, policy, preferences).
//!
//! The wire surface exposes summaries and preferences. Secrets never cross this
//! boundary: `GET /v1/profiles` reports which agents have a credential, never the
//! credential itself.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Which frontend is asking for a theme.
///
/// The theme is one setting per surface rather than one for all of them, because a
/// terminal is not a window: a person may want a light editor on the desktop and a
/// dark prompt in the terminal beside it. Every surface falls back to
/// [`Preferences::theme`] when its own key is unset, so the usual answer — one
/// theme, everywhere — stays one line of settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThemeSurface {
    /// The terminal UI.
    Tui,
    /// The desktop app.
    Gui,
    /// The browser UI the daemon serves.
    Web,
}

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
    /// Theme identifier for every surface that does not name its own.
    ///
    /// A theme is an extension, so this is the id of one: `acpilot.night-flight`
    /// unless someone says otherwise.
    pub theme: String,
    /// Theme for the terminal UI, when it should differ from [`Preferences::theme`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_tui: Option<String>,
    /// Theme for the desktop app, when it should differ from [`Preferences::theme`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_gui: Option<String>,
    /// Theme for the browser UI, when it should differ from [`Preferences::theme`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_web: Option<String>,
    /// Lowest diagnostic severity shown: `error`, `warning`, `information`, `hint`.
    pub diagnostics_min_severity: String,
}

impl Preferences {
    /// The theme one surface draws with.
    ///
    /// Its own key when that is set, otherwise [`Preferences::theme`]. Resolution
    /// happens here rather than in three frontends, because three fallbacks is
    /// three chances for the terminal and the window to disagree about what "the
    /// theme" means.
    #[must_use]
    pub fn theme_for(&self, surface: ThemeSurface) -> &str {
        let own = match surface {
            ThemeSurface::Tui => self.theme_tui.as_deref(),
            ThemeSurface::Gui => self.theme_gui.as_deref(),
            ThemeSurface::Web => self.theme_web.as_deref(),
        };
        own.unwrap_or(&self.theme)
    }
}
