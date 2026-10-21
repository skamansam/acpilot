//! The registries an extension contributes to.
//!
//! Everything here is derived from loaded extensions; nothing is written back.
//! Collisions are errors, not precedence: two things claiming `rust-analyzer` is
//! a bug in one of them, and picking a winner by sort order would make which bug
//! you see depend on the alphabet.

use std::collections::BTreeMap;

use acpilot_proto::extension::ThemeAppearance;

use crate::builtin;
use crate::error::ExtensionError;
use crate::loader::{Contribution, LoadedExtension, load_embedded};
use crate::preset::{AgentPreset, LanguageServerPreset};
use crate::theme::{Palette, ResolvedTheme, Theme};

/// Every extension the daemon can see, with inheritance resolved.
#[derive(Debug, Clone)]
pub struct Registry {
    extensions: Vec<LoadedExtension>,
    themes: Vec<ResolvedTheme>,
    language_servers: Vec<LanguageServerPreset>,
    agents: Vec<AgentPreset>,
}

struct ThemeEntry<'a> {
    theme: &'a Theme,
    extension_id: &'a str,
    built_in: bool,
}

impl Registry {
    /// Merge loaded extensions into one registry.
    ///
    /// All errors are collected rather than returned one at a time: fixing a
    /// stack of manifests should not require a rebuild per mistake.
    pub fn new(mut extensions: Vec<LoadedExtension>) -> Result<Self, Vec<ExtensionError>> {
        extensions.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        let mut errors = Vec::new();

        for pair in extensions.windows(2) {
            if pair[0].manifest.id == pair[1].manifest.id {
                errors.push(ExtensionError::Duplicate {
                    what: "extension",
                    id: pair[0].manifest.id.clone(),
                    owner: "another copy of the same extension".to_owned(),
                });
            }
        }

        let themes = resolve_themes(&extensions, &mut errors);

        let mut language_servers: Vec<LanguageServerPreset> = Vec::new();
        for extension in &extensions {
            for contribution in &extension.contributions {
                if let Contribution::LanguageServers(presets) = contribution {
                    for preset in presets {
                        if let Some(existing) = language_servers
                            .iter()
                            .find(|existing| existing.id == preset.id)
                        {
                            errors.push(ExtensionError::Duplicate {
                                what: "language-server preset",
                                id: preset.id.clone(),
                                owner: existing.extension_id.clone(),
                            });
                        } else {
                            language_servers.push(preset.clone());
                        }
                    }
                }
            }
        }
        language_servers.sort_by(|a, b| a.id.cmp(&b.id));

        let mut agents: Vec<AgentPreset> = Vec::new();
        for extension in &extensions {
            for contribution in &extension.contributions {
                if let Contribution::Agents(presets) = contribution {
                    for preset in presets {
                        if let Some(existing) =
                            agents.iter().find(|existing| existing.id == preset.id)
                        {
                            errors.push(ExtensionError::Duplicate {
                                what: "agent preset",
                                id: preset.id.clone(),
                                owner: existing.extension_id.clone(),
                            });
                        } else {
                            agents.push(preset.clone());
                        }
                    }
                }
            }
        }
        agents.sort_by(|a, b| a.id.cmp(&b.id));

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(Self {
            extensions,
            themes,
            language_servers,
            agents,
        })
    }

    /// The extensions compiled into this binary, loaded the long way round.
    pub fn built_in() -> Result<Self, Vec<ExtensionError>> {
        let mut loaded = Vec::new();
        let mut errors = Vec::new();
        for files in builtin::ALL {
            match load_embedded(files) {
                Ok(extension) => loaded.push(extension),
                Err(error) => errors.push(error),
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        Self::new(loaded)
    }

    /// Every loaded extension, sorted by id.
    #[must_use]
    pub fn extensions(&self) -> &[LoadedExtension] {
        &self.extensions
    }

    /// One extension by id.
    #[must_use]
    pub fn extension(&self, id: &str) -> Option<&LoadedExtension> {
        self.extensions
            .iter()
            .find(|extension| extension.manifest.id == id)
    }

    /// Every theme, fully resolved, sorted by id.
    #[must_use]
    pub fn themes(&self) -> &[ResolvedTheme] {
        &self.themes
    }

    /// A theme by id.
    #[must_use]
    pub fn theme(&self, id: &str) -> Option<&ResolvedTheme> {
        self.themes.iter().find(|theme| theme.id == id)
    }

    /// The light or dark theme to use when the user has no preference.
    #[must_use]
    pub fn default_theme(&self, appearance: ThemeAppearance) -> Option<&ResolvedTheme> {
        self.themes
            .iter()
            .find(|theme| theme.built_in && theme.appearance == appearance)
    }

    /// Every language-server preset, sorted by id.
    #[must_use]
    pub fn language_servers(&self) -> &[LanguageServerPreset] {
        &self.language_servers
    }

    /// Every agent preset, sorted by id.
    #[must_use]
    pub fn agents(&self) -> &[AgentPreset] {
        &self.agents
    }
}

/// Resolve every theme's `extends` chain, collecting collisions and cycles.
fn resolve_themes(
    extensions: &[LoadedExtension],
    errors: &mut Vec<ExtensionError>,
) -> Vec<ResolvedTheme> {
    let mut index: BTreeMap<&str, ThemeEntry<'_>> = BTreeMap::new();
    for extension in extensions {
        for contribution in &extension.contributions {
            if let Contribution::Theme(theme) = contribution {
                if let Some(existing) = index.get(theme.id.as_str()) {
                    errors.push(ExtensionError::Duplicate {
                        what: "theme",
                        id: theme.id.clone(),
                        owner: existing.extension_id.to_owned(),
                    });
                } else {
                    index.insert(
                        theme.id.as_str(),
                        ThemeEntry {
                            theme,
                            extension_id: &extension.manifest.id,
                            built_in: extension.built_in,
                        },
                    );
                }
            }
        }
    }

    if !errors.is_empty() {
        return Vec::new();
    }

    let mut memo: BTreeMap<String, Palette> = BTreeMap::new();
    let mut resolved = Vec::with_capacity(index.len());
    for (id, entry) in &index {
        let mut stack = Vec::new();
        match palette_for(id, id, &index, &mut stack, &mut memo) {
            Ok(palette) => resolved.push(ResolvedTheme {
                id: entry.theme.id.clone(),
                name: entry.theme.name.clone(),
                appearance: entry.theme.appearance,
                built_in: entry.built_in,
                extension_id: entry.extension_id.to_owned(),
                palette,
            }),
            Err(error) => errors.push(error),
        }
    }
    resolved
}

/// Compute the palette for one theme, following `extends`.
///
/// `asking` is the theme whose manifest named this one, so the error message can
/// name the file to edit rather than only the base that is missing.
fn palette_for(
    id: &str,
    asking: &str,
    index: &BTreeMap<&str, ThemeEntry<'_>>,
    stack: &mut Vec<String>,
    memo: &mut BTreeMap<String, Palette>,
) -> Result<Palette, ExtensionError> {
    if let Some(palette) = memo.get(id) {
        return Ok(palette.clone());
    }
    if stack.iter().any(|seen| seen == id) {
        let mut chain = stack.clone();
        chain.push(id.to_owned());
        return Err(ExtensionError::Theme {
            id: asking.to_owned(),
            message: format!("extends cycle: {}", chain.join(" -> ")),
        });
    }
    let Some(entry) = index.get(id) else {
        return Err(ExtensionError::Theme {
            id: asking.to_owned(),
            message: format!("extends {id:?}, which no extension provides"),
        });
    };

    stack.push(id.to_owned());
    let base = match &entry.theme.extends {
        Some(base_id) => Some(palette_for(base_id, asking, index, stack, memo)?),
        None => None,
    };
    stack.pop();

    let theme = entry
        .theme
        .resolve(base.as_ref(), entry.extension_id, entry.built_in);
    memo.insert(id.to_owned(), theme.palette.clone());
    Ok(theme.palette)
}
