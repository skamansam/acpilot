//! Extensions: themes, language-server presets, agent presets, grammars.
//!
//! An extension is **installed once and activated per profile** (ADR 0003). That
//! split is why these types exist in two shapes: `ExtensionSummary` describes
//! what is on disk, and activation is a profile-scoped fact reported alongside
//! it. Nothing here executes: the declarative tier is a manifest and assets, and
//! a manifest that names a command still meets the trust pin and the broker when
//! something finally runs it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Opaque extension identifier, namespaced `publisher.id`.
pub type ExtensionId = String;

/// What an extension contributes.
///
/// One extension may contribute several kinds; a "Rust pack" is a bundle rather
/// than a category, which is why this is a list on the summary and a single value
/// on each contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
    /// A colour scheme shared by TUI, GUI, and web.
    Theme,
    /// Language-server presets.
    LanguageServers,
    /// Agent presets (ACP commands and their docs).
    Agents,
    /// A prebuilt tree-sitter grammar plus its queries.
    Grammars,
    /// Per-language snippets.
    Snippets,
    /// A keymap layer.
    Keymap,
    /// A program-as-agent transcript parser. Tier 2; not in v0.1.
    TranscriptParser,
}

/// How the extension got here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSource {
    /// Installed from the signed index.
    Registry,
    /// Linked from a directory on this machine, for development.
    Path,
}

/// Verification and activation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionStatus {
    /// On disk and verified, enabled in no profile.
    Installed,
    /// Enabled in at least one profile.
    Active,
    /// On disk, disabled everywhere.
    Disabled,
    /// Checksum mismatch or a manifest that will not load. Never activated.
    Blocked,
}

/// An extension as returned by `GET /v1/extensions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExtensionSummary {
    /// Namespaced identifier, `publisher.id`.
    pub id: ExtensionId,
    /// Display name.
    pub name: String,
    /// Semantic version from the manifest.
    pub version: String,
    /// Who published it, as the index records them.
    pub publisher: String,
    /// One sentence, shown in the list.
    pub description: String,
    /// Everything it contributes.
    pub kinds: Vec<ExtensionKind>,
    /// Verification and activation state.
    pub status: ExtensionStatus,
    /// Where it came from.
    pub source: ExtensionSource,
    /// SHA-256 of the installed directory, verified against the index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// RFC 3339 timestamp of installation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
    /// Project page or repository, for a reader deciding whether to trust it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
}

/// One thing an extension provides, named as the profile will see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExtensionContribution {
    /// Which registry the id lands in.
    pub kind: ExtensionKind,
    /// Identifier inside that registry, for example `rust-analyzer` or
    /// `night-flight`.
    pub id: String,
    /// One line of description, so `ext list` is readable without the manifest.
    pub summary: String,
}

/// One extension, in full, as returned by `GET /v1/extensions/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExtensionDetail {
    /// The same summary the list endpoint returns.
    pub summary: ExtensionSummary,
    /// Everything it contributes, flattened.
    pub contributions: Vec<ExtensionContribution>,
    /// The manifest as written, for reviewing before enabling.
    pub manifest: String,
    /// Profiles that have it activated.
    pub active_profiles: Vec<String>,
}

/// Request body for `POST /v1/extensions`.
///
/// Exactly one of `version` (registry) or `path` (local directory) is used;
/// `path` wins when both are present, because a developer typing a path means it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InstallExtensionRequest {
    /// Identifier from the index, or the manifest's id for a path install.
    pub id: ExtensionId,
    /// Version to install. The index's latest when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Directory to link instead of downloading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Activate it in the profile named by `X-Acpilot-Profile` after verifying.
    pub activate: bool,
}

/// Light or dark, so a UI can pick the right half of a theme pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThemeAppearance {
    /// For night flight.
    Dark,
    /// For day VFR.
    Light,
}

/// A theme as returned by `GET /v1/themes`.
///
/// Themes are the one extension kind every surface consumes, so they get a
/// first-class route rather than being reachable only through the extension list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ThemeSummary {
    /// Identifier, for example `acpilot.night-flight`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Which half of the day it is for.
    pub appearance: ThemeAppearance,
    /// `true` for the themes compiled into the binary.
    pub built_in: bool,
    /// Extension that provided it, when it did not come from the binary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_id: Option<ExtensionId>,
}
