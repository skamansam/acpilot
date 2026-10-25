//! Activation: what one profile is allowed to see.
//!
//! Installing is global; activating is per profile. The activation file is a
//! list of ids and nothing else, deliberately — activation never rewrites your
//! `config.toml`, `lsp.toml`, or `agents.toml`, so `profile diff` can still name
//! the file that produced every value.
//!
//! Every call to [`ProfileExtensions::effective`] re-reads the store and the
//! profile's own fragments. That is what makes deactivation take effect without a
//! restart: there is no cached registry to invalidate.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::builtin;
use crate::effective::{EffectiveRegistry, ProfileLayer};
use crate::error::ExtensionError;
use crate::manifest::{SCHEMA_VERSION, validate_namespaced_id, validate_segment};
use crate::preset::{PROFILE_OWNER, parse_agents, parse_language_servers};
use crate::store::{ActiveSet, Store, StoredExtension};

/// Name of the activation file inside a profile directory.
pub const ACTIVATION_FILE: &str = "extensions.toml";

/// A profile's activation list, exactly as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    /// Schema version. `1` today.
    pub schema_version: u32,
    /// Active extension ids, sorted.
    #[serde(default)]
    pub active: Vec<String>,
}

impl Default for Activation {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            active: Vec::new(),
        }
    }
}

impl Activation {
    /// Parse and validate an activation file.
    pub fn parse(path: &str, text: &str) -> Result<Self, ExtensionError> {
        let activation: Self = toml::from_str(text).map_err(|error| ExtensionError::Toml {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        if activation.schema_version != SCHEMA_VERSION {
            return Err(ExtensionError::SchemaVersion {
                path: path.to_owned(),
                found: u64::from(activation.schema_version),
                expected: SCHEMA_VERSION,
            });
        }
        let mut seen: Vec<&str> = Vec::new();
        for id in &activation.active {
            validate_namespaced_id("extension id", id)?;
            if seen.contains(&id.as_str()) {
                return Err(ExtensionError::Duplicate {
                    what: "activation entry",
                    id: id.clone(),
                    owner: path.to_owned(),
                });
            }
            seen.push(id);
        }
        Ok(activation)
    }

    /// Read a profile's activation file. Missing means "nothing activated",
    /// which is the state of every fresh profile.
    pub fn load(path: &Path) -> Result<Self, ExtensionError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&path.display().to_string(), &text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ExtensionError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Write the file atomically: a profile is never left with a half-written
    /// list, because that would approve half an installation.
    pub fn save(&self, path: &Path) -> Result<(), ExtensionError> {
        let text = toml::to_string(self).map_err(|error| ExtensionError::Toml {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ExtensionError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let temporary = path.with_extension("toml.tmp");
        fs::write(&temporary, text).map_err(|source| ExtensionError::Io {
            path: temporary.display().to_string(),
            source,
        })?;
        fs::rename(&temporary, path).map_err(|source| ExtensionError::Io {
            path: temporary.display().to_string(),
            source,
        })
    }

    /// Approve an id. Returns whether the file changed.
    pub fn activate(&mut self, id: &str) -> Result<bool, ExtensionError> {
        validate_namespaced_id("extension id", id)?;
        if self.active.iter().any(|active| active == id) {
            return Ok(false);
        }
        self.active.push(id.to_owned());
        self.active.sort();
        Ok(true)
    }

    /// Withdraw an id. Returns whether the file changed.
    pub fn deactivate(&mut self, id: &str) -> bool {
        let before = self.active.len();
        self.active.retain(|active| active != id);
        self.active.len() != before
    }

    /// Whether an id is approved here.
    #[must_use]
    pub fn is_active(&self, id: &str) -> bool {
        self.active.iter().any(|active| active == id)
    }
}

/// What a profile resolves to right now.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// The effective registries for this profile.
    pub registry: EffectiveRegistry,
    /// Active versions that exist but are blocked. Invisible to every loader.
    pub blocked: Vec<StoredExtension>,
    /// Active ids with nothing installed.
    pub missing: Vec<String>,
}

/// A profile directory plus the store it activates from.
#[derive(Debug, Clone)]
pub struct ProfileExtensions {
    store: Store,
    directory: PathBuf,
    activation: Activation,
}

impl ProfileExtensions {
    /// Open a profile directory. A missing `extensions.toml` activates nothing.
    pub fn open(store: Store, directory: impl Into<PathBuf>) -> Result<Self, ExtensionError> {
        let directory = directory.into();
        let activation = Activation::load(&directory.join(ACTIVATION_FILE))?;
        Ok(Self {
            store,
            directory,
            activation,
        })
    }

    /// The profile directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The store this profile activates from.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The current activation list.
    #[must_use]
    pub fn activation(&self) -> &Activation {
        &self.activation
    }

    /// Re-read `extensions.toml`, for a daemon that noticed it changed.
    pub fn reload(&mut self) -> Result<(), ExtensionError> {
        self.activation = Activation::load(&self.directory.join(ACTIVATION_FILE))?;
        Ok(())
    }

    /// Approve an installed, verified extension for this profile.
    ///
    /// Refusing to activate a blocked or absent id is deliberate: approval is a
    /// statement about known bytes, and there are no known bytes here.
    pub fn activate(&mut self, id: &str) -> Result<bool, ExtensionError> {
        let scan = self.store.scan()?;
        if !scan
            .iter()
            .any(|stored| stored.id == id && stored.is_ready())
        {
            let blocked: Vec<String> = scan
                .iter()
                .filter(|stored| stored.id == id && stored.is_blocked())
                .map(|stored| match &stored.status {
                    crate::store::StoreStatus::Blocked(reason) => {
                        format!("{} ({reason})", stored.label())
                    }
                    crate::store::StoreStatus::Ready => stored.label(),
                })
                .collect();
            if blocked.is_empty() {
                return Err(ExtensionError::NotFound {
                    what: format!("extension {id} in the store"),
                });
            }
            return Err(ExtensionError::BlockedActivation {
                id: id.to_owned(),
                blocked: blocked.join("; "),
            });
        }
        if self.activation.activate(id)? {
            self.activation
                .save(&self.directory.join(ACTIVATION_FILE))?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Withdraw approval. The next [`Self::effective`] call no longer sees it.
    pub fn deactivate(&mut self, id: &str) -> Result<bool, ExtensionError> {
        if self.activation.deactivate(id) {
            self.activation
                .save(&self.directory.join(ACTIVATION_FILE))?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Resolve the profile: built-ins, then activated extensions, then the
    /// profile's own `lsp.toml` and `agents.toml`.
    pub fn effective(&self) -> Result<Resolution, Vec<ExtensionError>> {
        let built_ins = builtin::load_all()?;
        let active: ActiveSet = self.store.resolve_active(&self.activation.active)?;

        let mut errors = Vec::new();
        let profile = self.profile_layer(&mut errors);
        if !errors.is_empty() {
            return Err(errors);
        }

        let registry = EffectiveRegistry::resolve(built_ins, active.extensions, profile)?;
        Ok(Resolution {
            registry,
            blocked: active.blocked,
            missing: active.missing,
        })
    }

    /// Read the profile's own preset fragments, if the files exist.
    fn profile_layer(&self, errors: &mut Vec<ExtensionError>) -> ProfileLayer {
        let mut layer = ProfileLayer::default();
        match self.read("lsp.toml") {
            Ok(Some(text)) => match parse_language_servers("lsp.toml", &text, PROFILE_OWNER) {
                Ok(presets) => layer.language_servers = presets,
                Err(error) => errors.push(error),
            },
            Ok(None) => {}
            Err(error) => errors.push(error),
        }
        match self.read("agents.toml") {
            Ok(Some(text)) => match parse_agents("agents.toml", &text, PROFILE_OWNER) {
                Ok(presets) => layer.agents = presets,
                Err(error) => errors.push(error),
            },
            Ok(None) => {}
            Err(error) => errors.push(error),
        }
        layer
    }

    fn read(&self, file: &str) -> Result<Option<String>, ExtensionError> {
        let path = self.directory.join(file);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ExtensionError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }
}

/// The platform default config root: `<config dir>/acpilot`.
pub fn config_root() -> Result<PathBuf, ExtensionError> {
    let base = directories::BaseDirs::new().ok_or_else(|| ExtensionError::NotFound {
        what: "a config directory to put profiles in".to_owned(),
    })?;
    Ok(base.config_dir().join("acpilot"))
}

/// Root of every profile directory in the default config root.
pub fn profiles_root() -> Result<PathBuf, ExtensionError> {
    Ok(config_root()?.join("profiles"))
}

/// One profile's directory. The name is validated, so a name can never be a path.
pub fn profile_dir(name: &str) -> Result<PathBuf, ExtensionError> {
    profile_dir_in(&config_root()?, name)
}

/// One profile's directory under an explicit config root.
pub fn profile_dir_in(config_root: &Path, name: &str) -> Result<PathBuf, ExtensionError> {
    validate_segment("profile", name)?;
    Ok(config_root.join("profiles").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty_and_versioned() {
        let activation = Activation::default();
        assert_eq!(activation.schema_version, SCHEMA_VERSION);
        assert!(activation.active.is_empty());
    }

    #[test]
    fn profile_names_cannot_be_paths() {
        for name in ["../work", "work/../x", "Work", "", "a\\b"] {
            assert!(profile_dir(name).is_err(), "{name:?} should be refused");
        }
    }
}
