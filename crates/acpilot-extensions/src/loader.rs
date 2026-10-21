//! Loading an extension is a pure function of its directory.
//!
//! No install-time script, no network, no code execution: read the manifest,
//! check it, and parse what it points at with the same parsers the user's own
//! configuration goes through. If any contribution fails, the whole extension
//! fails — a half-loaded extension is one whose theme works and whose preset
//! silently does not.

use acpilot_proto::extension::ExtensionKind;

use crate::error::ExtensionError;
use crate::manifest::{MANIFEST_FILE, Manifest, kind_name};
use crate::preset::{AgentPreset, LanguageServerPreset, parse_agents, parse_language_servers};
use crate::source::{AssetSource, DirSource, EmbeddedSource};
use crate::theme::Theme;

/// A manifest and everything it successfully loaded.
#[derive(Debug, Clone)]
pub struct LoadedExtension {
    /// The validated manifest.
    pub manifest: Manifest,
    /// `true` when the files came out of the binary rather than the store.
    pub built_in: bool,
    /// Contributions, in manifest order.
    pub contributions: Vec<Contribution>,
    /// Things worth telling the user that are not errors.
    pub warnings: Vec<String>,
}

/// One parsed contribution.
#[derive(Debug, Clone)]
pub enum Contribution {
    /// A theme, before inheritance is resolved.
    Theme(Box<Theme>),
    /// One or more language-server presets.
    LanguageServers(Vec<LanguageServerPreset>),
    /// One or more agent presets.
    Agents(Vec<AgentPreset>),
    /// A kind the manifest may declare but this build cannot read yet.
    ///
    /// The path is still checked; the file is not parsed. Saying so out loud is
    /// better than a contribution that appears installed and does nothing.
    Unloaded {
        /// The declared kind.
        kind: ExtensionKind,
        /// The declared path.
        path: String,
        /// Why it was not read.
        reason: &'static str,
    },
}

impl Contribution {
    /// The kind this contribution belongs to.
    #[must_use]
    pub const fn kind(&self) -> ExtensionKind {
        match self {
            Self::Theme(_) => ExtensionKind::Theme,
            Self::LanguageServers(_) => ExtensionKind::LanguageServers,
            Self::Agents(_) => ExtensionKind::Agents,
            Self::Unloaded { kind, .. } => *kind,
        }
    }
}

/// Load an extension from any source.
pub fn load(source: &dyn AssetSource) -> Result<LoadedExtension, ExtensionError> {
    let manifest_text = source.read_text(MANIFEST_FILE)?;
    let manifest = Manifest::parse(MANIFEST_FILE, &manifest_text)?;
    let mut contributions = Vec::with_capacity(manifest.contributes.len());
    let mut warnings = Vec::new();

    for entry in &manifest.contributes {
        if !source.exists(&entry.path)? {
            return Err(ExtensionError::MissingContribution {
                path: entry.path.clone(),
                kind: kind_name(entry.kind),
            });
        }

        match entry.kind {
            ExtensionKind::Theme => {
                let text = source.read_text(&entry.path)?;
                let theme = Theme::parse(&entry.path, &text)?;
                let prefix = format!("{}.", manifest.publisher);
                if !theme.id.starts_with(&prefix) {
                    return Err(ExtensionError::InvalidId {
                        what: "theme id",
                        id: theme.id,
                        why: "must be namespaced under the publishing extension",
                    });
                }
                contributions.push(Contribution::Theme(Box::new(theme)));
            }
            ExtensionKind::LanguageServers => {
                let text = source.read_text(&entry.path)?;
                contributions.push(Contribution::LanguageServers(parse_language_servers(
                    &entry.path,
                    &text,
                    &manifest.id,
                )?));
            }
            ExtensionKind::Agents => {
                let text = source.read_text(&entry.path)?;
                contributions.push(Contribution::Agents(parse_agents(
                    &entry.path,
                    &text,
                    &manifest.id,
                )?));
            }
            kind @ (ExtensionKind::Grammars
            | ExtensionKind::Snippets
            | ExtensionKind::Keymap
            | ExtensionKind::TranscriptParser) => {
                let reason = match kind {
                    ExtensionKind::TranscriptParser => {
                        "transcript parsers are Tier 2 and not in v0.1 (decision D18)"
                    }
                    ExtensionKind::Grammars => "grammar loading arrives in P4.6-8",
                    ExtensionKind::Snippets | ExtensionKind::Keymap => {
                        "snippet and keymap loading arrives in P4.6-9"
                    }
                    _ => unreachable!("the match arm only covers the deferred kinds"),
                };
                warnings.push(format!(
                    "{} {}: installed but not read — {reason}",
                    kind_name(kind),
                    entry.path
                ));
                contributions.push(Contribution::Unloaded {
                    kind,
                    path: entry.path.clone(),
                    reason,
                });
            }
        }
    }

    Ok(LoadedExtension {
        manifest,
        built_in: false,
        contributions,
        warnings,
    })
}

/// Load an extension from a directory on this machine.
pub fn load_dir(root: impl AsRef<std::path::Path>) -> Result<LoadedExtension, ExtensionError> {
    let source = DirSource::new(root)?;
    load(&source)
}

/// Load an embedded extension — a built-in.
pub fn load_embedded(
    files: &'static [(&'static str, &'static str)],
) -> Result<LoadedExtension, ExtensionError> {
    let source = EmbeddedSource::new(files);
    let mut extension = load(&source)?;
    extension.built_in = true;
    Ok(extension)
}
