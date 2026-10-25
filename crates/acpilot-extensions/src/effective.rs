//! Precedence, spelled out.
//!
//! Three layers, lowest to highest:
//!
//! 1. the built-ins compiled into the binary;
//! 2. extensions the profile has activated;
//! 3. the profile's own `lsp.toml` / `agents.toml`.
//!
//! Within a layer, two things claiming the same id is still an error: two
//! activated packs that both ship `rust-analyzer` is a bug in one of them, and
//! picking a winner by sort order would make which bug you see depend on the
//! alphabet. Across layers the higher one wins, and the winners are recorded as
//! a chain so `ext why` can name every provider it overrode.
//!
//! Themes are not presets. A theme id is an identity, not a default, so no layer
//! may replace a built-in theme — that would make every screenshot in the manual
//! a lie. An activated theme may still `extends` a built-in and inherit from it.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::ExtensionError;
use crate::loader::{Contribution, LoadedExtension};
use crate::preset::{AgentPreset, LanguageServerPreset};
use crate::registry::resolve_themes;
use crate::theme::ResolvedTheme;

/// Where a value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layer {
    /// Compiled into the binary.
    BuiltIn {
        /// Extension id.
        extension: String,
        /// Extension version.
        version: String,
    },
    /// Installed in the store and activated for this profile.
    Activated {
        /// Extension id.
        extension: String,
        /// Extension version.
        version: String,
    },
    /// The profile's own config file.
    Profile {
        /// File name, `lsp.toml` or `agents.toml`.
        file: &'static str,
    },
}

impl Layer {
    fn rank(&self) -> Rank {
        match self {
            Self::BuiltIn { .. } => Rank::BuiltIn,
            Self::Activated { .. } => Rank::Activated,
            Self::Profile { .. } => Rank::Profile,
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuiltIn { extension, version } => {
                write!(f, "built-in {extension} {version}")
            }
            Self::Activated { extension, version } => {
                write!(f, "extension {extension} {version}")
            }
            Self::Profile { file } => write!(f, "profile {file}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rank {
    BuiltIn,
    Activated,
    Profile,
}

/// A preset plus the layers that provided it, winner first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace<T> {
    /// The value that won.
    pub value: T,
    /// Every provider, highest precedence first. Always at least one.
    pub layers: Vec<Layer>,
    rank: Rank,
}

impl<T: Preset> Trace<T> {
    /// The provider that won.
    #[must_use]
    pub fn winner(&self) -> &Layer {
        self.layers.first().expect("a trace always has a provider")
    }

    /// Render the winning values and the chain, winner first. This is what
    /// `acpilot ext why` prints.
    #[must_use]
    pub fn explain(&self) -> String {
        let mut out = String::new();
        for (label, value) in self.value.details() {
            out.push_str(&format!("  {label:<12}{value}\n"));
        }
        for (index, layer) in self.layers.iter().enumerate() {
            if index == 0 {
                out.push_str(&format!("  {:<12}{layer}   (wins)\n", "from"));
            } else {
                out.push_str(&format!("  {:<12}{layer}\n", "overridden"));
            }
        }
        out
    }
}

/// Anything that lives in an id-keyed preset registry.
pub trait Preset {
    /// Registry id.
    fn id(&self) -> &str;

    /// The winning values, as label/value pairs for `ext why`.
    fn details(&self) -> Vec<(&'static str, String)>;
}

impl Preset for LanguageServerPreset {
    fn id(&self) -> &str {
        &self.id
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        vec![
            ("command", self.command.clone()),
            ("args", format!("{:?}", self.args)),
            ("languages", self.languages.join(", ")),
        ]
    }
}

impl Preset for AgentPreset {
    fn id(&self) -> &str {
        &self.definition.id
    }

    fn details(&self) -> Vec<(&'static str, String)> {
        let mut details = vec![("transport", self.definition.transport.name().to_owned())];
        if let Some(command) = self.command() {
            details.push(("command", command.command_line()));
        }
        if let Some(description) = &self.definition.description {
            details.push(("description", description.clone()));
        }
        // Names only. A value is either a literal or a reference, and a literal
        // can be a key somebody pasted in before they had a keyring entry.
        if !self.definition.env.is_empty() {
            details.push((
                "env",
                self.definition
                    .env
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
        details
    }
}

/// The profile's own contributions — the highest layer.
#[derive(Debug, Clone, Default)]
pub struct ProfileLayer {
    /// Presets from the profile's `lsp.toml`.
    pub language_servers: Vec<LanguageServerPreset>,
    /// Presets from the profile's `agents.toml`.
    pub agents: Vec<AgentPreset>,
}

/// The registry one profile actually sees.
#[derive(Debug, Clone)]
pub struct EffectiveRegistry {
    extensions: Vec<LoadedExtension>,
    themes: Vec<ResolvedTheme>,
    language_servers: Vec<Trace<LanguageServerPreset>>,
    agents: Vec<Trace<AgentPreset>>,
}

impl EffectiveRegistry {
    /// Apply precedence across the three layers.
    pub fn resolve(
        built_ins: Vec<LoadedExtension>,
        activated: Vec<LoadedExtension>,
        profile: ProfileLayer,
    ) -> Result<Self, Vec<ExtensionError>> {
        let mut errors = Vec::new();

        let mut all_extensions = built_ins.clone();
        all_extensions.extend(activated.clone());
        let themes = resolve_themes(&all_extensions, &mut errors);

        let mut server_groups = Vec::new();
        let mut agent_groups = Vec::new();
        for (extension, built_in) in built_ins
            .iter()
            .map(|extension| (extension, true))
            .chain(activated.iter().map(|extension| (extension, false)))
        {
            let layer = if built_in {
                Layer::BuiltIn {
                    extension: extension.manifest.id.clone(),
                    version: extension.manifest.version.clone(),
                }
            } else {
                Layer::Activated {
                    extension: extension.manifest.id.clone(),
                    version: extension.manifest.version.clone(),
                }
            };
            for contribution in &extension.contributions {
                match contribution {
                    Contribution::LanguageServers(presets) => {
                        server_groups.push((layer.clone(), presets.clone()));
                    }
                    Contribution::Agents(presets) => {
                        agent_groups.push((layer.clone(), presets.clone()));
                    }
                    Contribution::Theme(_) | Contribution::Unloaded { .. } => {}
                }
            }
        }
        server_groups.push((
            Layer::Profile { file: "lsp.toml" },
            profile.language_servers,
        ));
        agent_groups.push((
            Layer::Profile {
                file: "agents.toml",
            },
            profile.agents,
        ));

        let language_servers = merge_presets(server_groups, "language-server preset", &mut errors);
        let agents = merge_presets(agent_groups, "agent preset", &mut errors);

        if !errors.is_empty() {
            return Err(errors);
        }

        let mut extensions = all_extensions;
        extensions.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));

        Ok(Self {
            extensions,
            themes,
            language_servers,
            agents,
        })
    }

    /// Every extension that contributed to this profile.
    #[must_use]
    pub fn extensions(&self) -> &[LoadedExtension] {
        &self.extensions
    }

    /// Resolved themes, collision-free.
    #[must_use]
    pub fn themes(&self) -> &[ResolvedTheme] {
        &self.themes
    }

    /// Language servers with their precedence chain.
    #[must_use]
    pub fn language_servers(&self) -> &[Trace<LanguageServerPreset>] {
        &self.language_servers
    }

    /// Agents with their precedence chain.
    #[must_use]
    pub fn agents(&self) -> &[Trace<AgentPreset>] {
        &self.agents
    }

    /// Explain a preset id, whichever registry it lives in. `None` when nothing
    /// provides it.
    #[must_use]
    pub fn explain(&self, id: &str) -> Option<String> {
        let mut out = String::new();
        if let Some(trace) = self
            .language_servers
            .iter()
            .find(|trace| trace.value.id == id)
        {
            out.push_str(&format!("language server {id}\n"));
            out.push_str(&trace.explain());
        }
        if let Some(trace) = self.agents.iter().find(|trace| trace.value.id() == id) {
            out.push_str(&format!("agent {id}\n"));
            out.push_str(&trace.explain());
        }
        if out.is_empty() { None } else { Some(out) }
    }
}

fn merge_presets<T: Preset + Clone>(
    groups: Vec<(Layer, Vec<T>)>,
    what: &'static str,
    errors: &mut Vec<ExtensionError>,
) -> Vec<Trace<T>> {
    let mut merged: BTreeMap<String, Trace<T>> = BTreeMap::new();
    for (layer, presets) in groups {
        let rank = layer.rank();
        for preset in presets {
            let id = preset.id().to_owned();
            match merged.get_mut(&id) {
                Some(existing) if existing.rank == rank => {
                    errors.push(ExtensionError::Duplicate {
                        what,
                        id,
                        owner: existing.layers[0].to_string(),
                    });
                }
                Some(existing) => {
                    existing.layers.insert(0, layer.clone());
                    existing.value = preset;
                    existing.rank = rank;
                }
                None => {
                    merged.insert(
                        id,
                        Trace {
                            value: preset,
                            layers: vec![layer.clone()],
                            rank,
                        },
                    );
                }
            }
        }
    }
    merged.into_values().collect()
}
