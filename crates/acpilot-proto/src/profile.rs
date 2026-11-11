//! Profiles: a named identity (agents, credentials, policy, preferences).
//!
//! The wire surface exposes summaries and preferences. Secrets never cross this
//! boundary: `GET /v1/profiles` reports which agents have a credential, never the
//! credential itself.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::file::DiagnosticSeverity;

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
    /// The profile this one inherits settings beneath, when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<ProfileId>,
}

/// `POST /v1/profiles` — create a profile directory and its registry row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileCreate {
    /// The directory name and keyring namespace for the new profile.
    pub id: String,
    /// Human-readable label; the id when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The profile this one inherits settings beneath.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
}

/// `PATCH /v1/profiles/{id}` — change the fields a profile's row owns.
///
/// Detaching is a separate flag rather than `extends: null`: a nested option is
/// flattened by serde, so absent and null would arrive as the same value and one
/// of the two intentions would be lost.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProfilePatch {
    /// Human-readable label; an empty label falls back to the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A new parent. Cannot be combined with `clear_extends`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// Remove the current parent, leaving the profile with no inherited layer.
    #[serde(skip_serializing_if = "is_false")]
    pub clear_extends: bool,
}

/// Omit a `false` flag from a request that did not ask for it.
fn is_false(value: &bool) -> bool {
    !*value
}

impl ProfilePatch {
    /// Whether this patch names no field at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// `POST /v1/profiles/{id}/export` — the profile's configuration files.
///
/// The document deliberately has no secrets: a keyring reference is exported as
/// the reference it is, and resolving it is the CLI's opt-in flag, never an HTTP
/// answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProfileExport {
    /// Schema version of the export document.
    pub schema_version: u32,
    /// The profile the files belong to.
    pub profile: ProfileId,
    /// File name to contents, allow-listed to the configuration files a profile owns.
    pub files: BTreeMap<String, String>,
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
    /// Columns to draw a vertical ruler at, in cells.
    ///
    /// Rulers are a reading aid, not a hard limit: a line past one is drawn, not
    /// refused. Empty means the editor draws none, which is the default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rulers: Vec<u16>,
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
    /// Solve the terminal's palette from the terminal's own colours.
    ///
    /// On, `theme_tui` and `theme` name no theme for the terminal: the background
    /// and foreground the terminal reports (`OSC 11`/`OSC 10`) are the pair a
    /// complete token set is solved around, and they are what the terminal UI draws
    /// with. A terminal that cannot report them falls back to the theme the
    /// settings name, so the key is safe to turn on anywhere.
    ///
    /// The desktop app and the browser are unaffected: neither has a terminal to
    /// ask, and a window that took its colours from the shell that launched it would
    /// change appearance depending on where it was started from.
    #[serde(default)]
    pub theme_follow_terminal: bool,
    /// Leave paths a `.gitignore` excludes out of the file tree.
    ///
    /// On, the tree is the project as git sees it: `target/`, `.venv/` and every
    /// other ignored directory are not rows, which is the difference between a
    /// tree a person can read and a listing of a directory. The daemon answers the
    /// listing, so the matching happens where the ignore files are — a client that
    /// filtered a listing it did not read would be guessing at patterns it has not
    /// seen.
    ///
    /// Off, every entry is a row. A workspace that is not a repository has no
    /// `.gitignore` to honour, so the key changes nothing there.
    #[serde(default = "default_true")]
    pub tree_respect_gitignore: bool,
    /// Lowest diagnostic severity shown: `error`, `warning`, `information`, `hint`.
    pub diagnostics_min_severity: DiagnosticSeverity,
}

/// `true`, for a preference whose default is on.
///
/// Spelled out rather than `#[serde(default)]`, which would default a `bool` to
/// `false`: a field added to the defaults file still needs the answer for a client
/// that sends a `Preferences` without it, and "the tree hides ignored paths" is a
/// default that must survive an older sender.
fn default_true() -> bool {
    true
}

/// A partial preferences update.
///
/// Every field is optional because a patch names only what changed. The daemon
/// merges the named fields into the profile's `[preferences]` table and leaves
/// the rest — including keys a newer binary wrote — alone. A full
/// [`Preferences`] is deliberately not the body: a client that did not know
/// about a new field must not erase it by sending the object it read back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PreferencesPatch {
    /// Columns per tab stop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_width: Option<u8>,
    /// Insert spaces instead of tab characters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insert_spaces: Option<bool>,
    /// Run the formatter on save when one is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format_on_save: Option<bool>,
    /// Strip trailing whitespace on save.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trim_trailing_whitespace: Option<bool>,
    /// Soft-wrap long lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrap: Option<bool>,
    /// Columns to draw a vertical ruler at, in cells.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rulers: Option<Vec<u16>>,
    /// Theme identifier for every surface that does not name its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Theme for the terminal UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_tui: Option<String>,
    /// Theme for the desktop app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_gui: Option<String>,
    /// Theme for the browser UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_web: Option<String>,
    /// Solve the terminal's palette from the terminal's own colours.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_follow_terminal: Option<bool>,
    /// Leave paths a `.gitignore` excludes out of the file tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree_respect_gitignore: Option<bool>,
    /// Lowest diagnostic severity shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics_min_severity: Option<DiagnosticSeverity>,
}

impl PreferencesPatch {
    /// Whether this patch names no preference at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
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
