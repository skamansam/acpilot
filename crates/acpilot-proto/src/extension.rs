//! Extensions: themes, language-server presets, agent presets, grammars.
//!
//! An extension is **installed once and activated per profile** (ADR 0003). That
//! split is why these types exist in two shapes: `ExtensionSummary` describes
//! what is on disk, and activation is a profile-scoped fact reported alongside
//! it. Nothing here executes: the declarative tier is a manifest and assets, and
//! a manifest that names a command still meets the trust pin and the broker when
//! something finally runs it.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::profile::ThemeSurface;

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
    /// Agent Skills (`SKILL.md` directories) installed with the extension.
    Skills,
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
    /// Profiles that activate this id. Empty when none does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_profiles: Vec<String>,
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
    /// Every installed version, oldest first. The newest is [`Self::summary`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub versions: Vec<ExtensionSummary>,
    /// Versions the signed index offers for this id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listed: Vec<ExtensionListing>,
}

/// One version a signed index offers, as shown beside what is installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExtensionListing {
    /// Semantic version.
    pub version: String,
    /// The registry the listing is filed under.
    pub kind: ExtensionKind,
    /// Who published it.
    pub publisher: String,
    /// `sha256:<64 hex characters>` over the archive's bytes.
    pub checksum: String,
    /// The archive's length in bytes.
    pub size: u64,
    /// Where the archive is downloaded from.
    pub download: String,
    /// Oldest acpilot that can load it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_acpilot: Option<String>,
}

/// What an activation request changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExtensionAction {
    /// Namespaced identifier.
    pub id: ExtensionId,
    /// Profile the approval was written to, or withdrawn from.
    pub profile: String,
    /// `false` when the list already said what it was now asked to say.
    pub changed: bool,
}

/// What a removal deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExtensionRemoval {
    /// Namespaced identifier.
    pub id: ExtensionId,
    /// Versions removed, oldest first.
    pub versions: Vec<String>,
}

/// The result of rehashing every installed version of one id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExtensionVerify {
    /// Whether every version still matches its recorded checksum.
    pub ready: bool,
    /// The versions, with their verdicts.
    pub extensions: Vec<ExtensionSummary>,
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
    /// A different index: an https URL or a file. The one this build ships when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// Activate it in the daemon's profile after verifying.
    #[serde(default)]
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
    /// Surfaces whose settings name this theme right now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected: Vec<ThemeSurface>,
    /// Every UI token, resolved, as `#rrggbb`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ui: BTreeMap<String, String>,
    /// Every syntax token, resolved, as `#rrggbb`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub syntax: BTreeMap<String, String>,
}
