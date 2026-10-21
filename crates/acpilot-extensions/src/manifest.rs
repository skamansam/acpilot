//! `extension.toml`: the entire interface between an extension and the daemon.
//!
//! There is no install script and no build step. The manifest names files; the
//! loader reads them with the same parsers that read a user's own configuration.
//! Everything here is checked *before* an asset is opened, because "probably
//! fine" is not a category a path check gets to have.

use acpilot_proto::extension::ExtensionKind;
use serde::{Deserialize, Serialize};

use crate::error::ExtensionError;

/// The manifest schema this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Name of the manifest at an extension's root.
pub const MANIFEST_FILE: &str = "extension.toml";

/// The version of this build, for `min_acpilot` comparisons.
const ACPILOT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// An `extension.toml`, parsed and validated.
///
/// Unknown keys are a parse error rather than a shrug: an extension that spills
/// a typo should fail on the developer's machine, not quietly on a user's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Schema version; `1` today.
    pub schema_version: u32,
    /// Namespaced identifier, `publisher.id`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Semantic version. The store keeps versions side by side.
    pub version: String,
    /// Who published it, matched against the index entry.
    pub publisher: String,
    /// One sentence, shown by `ext list`.
    pub description: String,
    /// Project page, reviewed by a human before the index accepts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Oldest acpilot this extension supports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_acpilot: Option<String>,
    /// Every asset this extension contributes.
    pub contributes: Vec<Contribution>,
}

/// One asset named by the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contribution {
    /// Which registry the asset lands in.
    pub kind: ExtensionKind,
    /// Path relative to the extension root.
    pub path: String,
}

impl Manifest {
    /// Parse and validate a manifest.
    ///
    /// `path` is the manifest's path as it should appear in errors.
    pub fn parse(path: &str, text: &str) -> Result<Self, ExtensionError> {
        let manifest: Self = toml::from_str(text).map_err(|error| ExtensionError::Toml {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        manifest.validate(path)?;
        Ok(manifest)
    }

    /// Check every rule that does not need a filesystem.
    pub fn validate(&self, path: &str) -> Result<(), ExtensionError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ExtensionError::SchemaVersion {
                path: path.to_owned(),
                found: u64::from(self.schema_version),
                expected: SCHEMA_VERSION,
            });
        }

        validate_namespaced_id("extension id", &self.id)?;
        validate_segment("publisher", &self.publisher)?;
        if !self.id.starts_with(&format!("{}.", self.publisher)) {
            return Err(ExtensionError::InvalidId {
                what: "extension id",
                id: self.id.clone(),
                why: "must start with the `publisher` field followed by a dot",
            });
        }

        if self.name.trim().is_empty() {
            return Err(ExtensionError::EmptyField {
                path: path.to_owned(),
                field: "name",
            });
        }
        if self.description.trim().is_empty() {
            return Err(ExtensionError::EmptyField {
                path: path.to_owned(),
                field: "description",
            });
        }

        if !is_version(&self.version) {
            return Err(ExtensionError::InvalidId {
                what: "version",
                id: self.version.clone(),
                why: "must be MAJOR.MINOR.PATCH with numeric parts",
            });
        }

        if let Some(required) = &self.min_acpilot {
            let (Some(wanted), Some(current)) =
                (version_tuple(required), version_tuple(ACPILOT_VERSION))
            else {
                return Err(ExtensionError::InvalidId {
                    what: "min_acpilot",
                    id: required.clone(),
                    why: "must be MAJOR.MINOR.PATCH with numeric parts",
                });
            };
            if wanted > current {
                return Err(ExtensionError::NeedsNewerAcpilot {
                    path: path.to_owned(),
                    required: required.clone(),
                    current: ACPILOT_VERSION,
                });
            }
        }

        if self.contributes.is_empty() {
            return Err(ExtensionError::EmptyField {
                path: path.to_owned(),
                field: "contributes",
            });
        }

        let mut seen: Vec<(ExtensionKind, &str)> = Vec::new();
        for contribution in &self.contributes {
            validate_contribution_path(&contribution.path)?;
            let key = (contribution.kind, contribution.path.as_str());
            if seen.contains(&key) {
                return Err(ExtensionError::DuplicateContribution {
                    path: path.to_owned(),
                    kind: kind_name(contribution.kind),
                    contribution: contribution.path.clone(),
                });
            }
            seen.push(key);
        }

        Ok(())
    }
}

/// The name of a contribution kind, as written in a manifest.
#[must_use]
pub const fn kind_name(kind: ExtensionKind) -> &'static str {
    match kind {
        ExtensionKind::Theme => "theme",
        ExtensionKind::LanguageServers => "language_servers",
        ExtensionKind::Agents => "agents",
        ExtensionKind::Grammars => "grammars",
        ExtensionKind::Snippets => "snippets",
        ExtensionKind::Keymap => "keymap",
        ExtensionKind::TranscriptParser => "transcript_parser",
    }
}

/// Check that an id is `publisher.id`-shaped: at least two dot-separated
/// lowercase segments.
///
/// Namespacing is what keeps an extension from shadowing a built-in, so it is
/// enforced at parse time rather than discovered at merge time.
pub fn validate_namespaced_id(what: &'static str, id: &str) -> Result<(), ExtensionError> {
    if id.len() > 128 {
        return Err(ExtensionError::InvalidId {
            what,
            id: id.to_owned(),
            why: "must be at most 128 characters",
        });
    }
    let segments: Vec<&str> = id.split('.').collect();
    if segments.len() < 2 {
        return Err(ExtensionError::InvalidId {
            what,
            id: id.to_owned(),
            why: "must be namespaced `publisher.name`",
        });
    }
    for segment in &segments {
        if let Err(why) = segment_error(segment) {
            return Err(ExtensionError::InvalidId {
                what,
                id: id.to_owned(),
                why,
            });
        }
    }
    Ok(())
}

/// Check a single lowercase segment, used for `publisher` and preset ids.
pub fn validate_segment(what: &'static str, value: &str) -> Result<(), ExtensionError> {
    segment_error(value).map_err(|why| ExtensionError::InvalidId {
        what,
        id: value.to_owned(),
        why,
    })
}

/// Why one dotted segment is not a valid id part.
fn segment_error(segment: &str) -> Result<(), &'static str> {
    if segment.is_empty() {
        return Err("may not contain empty segments");
    }
    if segment.len() > 40 {
        return Err("each segment must be at most 40 characters");
    }
    if !segment
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err("segments are lowercase letters, digits, `-`, or `_`");
    }
    if !segment.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("segments must start with a letter or digit");
    }
    Ok(())
}

/// Refuse a contribution path that could reach outside the extension directory.
///
/// This is the one check without a "probably fine" branch. It is lexical on
/// purpose: it must fail *before* anything is opened, and a filesystem source
/// repeats it after resolving symlinks so a link cannot smuggle a file out.
pub fn validate_contribution_path(path: &str) -> Result<(), ExtensionError> {
    if path.trim().is_empty() {
        return Err(ExtensionError::UnsafePath {
            path: path.to_owned(),
            why: "may not be empty",
        });
    }
    if path.contains('\0') {
        return Err(ExtensionError::UnsafePath {
            path: path.to_owned(),
            why: "may not contain a NUL byte",
        });
    }
    if path.starts_with('/') || path.starts_with('\\') || looks_like_drive_prefix(path) {
        return Err(ExtensionError::UnsafePath {
            path: path.to_owned(),
            why: "must be relative to the extension directory",
        });
    }
    if path.contains('\\') {
        return Err(ExtensionError::UnsafePath {
            path: path.to_owned(),
            why: "must use forward slashes, so a manifest means the same thing on every host",
        });
    }
    for segment in path.split('/') {
        match segment {
            "" => {
                return Err(ExtensionError::UnsafePath {
                    path: path.to_owned(),
                    why: "may not contain empty segments",
                });
            }
            "." => {
                return Err(ExtensionError::UnsafePath {
                    path: path.to_owned(),
                    why: "may not contain `.` segments",
                });
            }
            ".." => {
                return Err(ExtensionError::UnsafePath {
                    path: path.to_owned(),
                    why: "may not escape the extension directory",
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// `true` when the string looks like `C:` at the front, which some platforms
/// treat as absolute even without a leading slash.
fn looks_like_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// A version as three numeric parts.
#[must_use]
pub fn version_tuple(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn is_version(version: &str) -> bool {
    // `1.0.0` is a version; `1.0` is not, because the store sorts on three
    // parts and a missing patch would sort as zero while looking absent.
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && version_tuple(version).is_some()
}
