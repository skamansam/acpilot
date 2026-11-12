//! The profile's declared model providers: `providers.toml`.
//!
//! A provider is a URL and a dialect, and declaring one is a deliberate act, so
//! the file is read as written: an unknown key is an error rather than a silently
//! ignored intention, which is the rule the settings cascade and the agent
//! manifest already follow. A table with a typo in it is a person who expected a
//! server to be reachable, and answering "no providers" would hide that.
//!
//! Nothing here dials anything. This module answers "what did someone declare";
//! asking a server what it has is `acpilotd::providers`, on an explicit request,
//! and a startup that probed every row would be a startup that hangs behind a
//! firewall.

use std::path::{Path, PathBuf};

use acpilot_proto::model::{AddProviderRequest, ProviderKind, ProviderSummary};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The file a profile declares its model servers in.
pub const PROVIDERS_FILE: &str = "providers.toml";

/// The file a profile remembers its last probe answers in.
///
/// Kept apart from `providers.toml` on purpose. That file is somebody's
/// declaration, and rewriting it after every probe would undo a hand edit and mix
/// "what I declared" with "what I was told". This one is derived data: an
/// unreadable copy is forgotten rather than failing a startup, because losing a
/// memory costs one probe and losing a declaration costs the provider.
pub const PROVIDERS_STATE_FILE: &str = "providers.state.toml";

/// The schema this build reads and writes.
pub const PROVIDERS_SCHEMA: u32 = 1;

/// Why a `providers.toml` could not be read or written.
#[derive(Debug, Error)]
pub enum ProvidersError {
    /// The file exists and could not be read.
    #[error("{path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The file is not TOML, or not this document.
    #[error("{path}: {message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parser's own message, with the line it reached.
        message: String,
    },
    /// The file declares a schema version this build does not read.
    #[error("{path}: schema_version {found} is not this build's {expected}")]
    Schema {
        /// The file.
        path: PathBuf,
        /// What the file says.
        found: u32,
        /// What this build reads.
        expected: u32,
    },
    /// One id appears twice, so a recipe naming it would mean two servers.
    #[error("{path}: `{id}` is declared twice")]
    Duplicate {
        /// The file.
        path: PathBuf,
        /// The id that repeats.
        id: String,
    },
    /// The file could not be written.
    #[error("{path}: {source}")]
    Write {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
}

/// A profile's whole `providers.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersFile {
    /// Schema version. A newer file is refused rather than half-read.
    pub schema_version: u32,
    /// The servers this profile declares, in the order they were added.
    #[serde(default)]
    pub providers: Vec<DeclaredProvider>,
}

/// One `[[providers]]` row: a URL, a dialect, and whether it may be contacted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredProvider {
    /// Identifier a recipe names.
    pub id: String,
    /// Wire dialect.
    pub kind: ProviderKind,
    /// Display label. Absent reads as the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Base URL, for example `http://127.0.0.1:1234`.
    pub base_url: String,
    /// Keyring entry holding the bearer token, when the server wants one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// Whether it may be contacted. Absent means yes.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

/// A row without `enabled` was written by somebody who expected it to work, so
/// the default is `true`: defaulting to `false` would make a declaration look
/// made and behave inert.
fn enabled_by_default() -> bool {
    true
}

impl DeclaredProvider {
    /// This declaration as the request that adds it.
    ///
    /// The API's `label` is a string and this file's is optional, so an absent
    /// label becomes empty rather than being invented here: the row builder turns
    /// an empty label into the id, and that rule stays in one place.
    #[must_use]
    pub fn request(&self) -> AddProviderRequest {
        AddProviderRequest {
            id: self.id.clone(),
            kind: self.kind,
            label: self.label.clone().unwrap_or_default(),
            base_url: self.base_url.clone(),
            credential_ref: self.credential_ref.clone(),
            enabled: self.enabled,
        }
    }
}

impl From<&DeclaredProvider> for AddProviderRequest {
    fn from(provider: &DeclaredProvider) -> Self {
        provider.request()
    }
}

/// One remembered answer, with the declaration it belongs to.
///
/// The declaration's fields are repeated here rather than looked up by id alone: a
/// `providers.toml` edit that points an id at another port must not keep showing
/// the old server's model count as though the new one had answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeRecord {
    /// Identifier the declaration uses.
    pub id: String,
    /// Dialect when the answer was recorded.
    pub kind: ProviderKind,
    /// Base URL when the answer was recorded.
    pub base_url: String,
    /// Credential reference when the answer was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// Whether the provider was enabled when the answer was recorded.
    pub enabled: bool,
    /// The row the probe produced.
    pub summary: ProviderSummary,
}

impl ProbeRecord {
    /// Whether this record still describes `declaration`.
    fn describes(&self, declaration: &DeclaredProvider) -> bool {
        self.id == declaration.id
            && self.kind == declaration.kind
            && self.base_url == declaration.base_url
            && self.credential_ref == declaration.credential_ref
            && self.enabled == declaration.enabled
    }
}

/// The answers this profile last received, by provider id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersState {
    /// Schema version, written so a later reader can tell what it found.
    #[serde(default = "state_schema")]
    pub schema_version: u32,
    /// One row per provider that was asked, answered or failed.
    #[serde(default)]
    pub probes: Vec<ProbeRecord>,
}

fn state_schema() -> u32 {
    PROVIDERS_SCHEMA
}

impl Default for ProvidersState {
    fn default() -> Self {
        Self {
            schema_version: PROVIDERS_SCHEMA,
            probes: Vec::new(),
        }
    }
}

impl ProvidersState {
    /// Where a profile directory remembers its answers.
    #[must_use]
    pub fn path_in(directory: &Path) -> PathBuf {
        directory.join(PROVIDERS_STATE_FILE)
    }

    /// Read the remembered answers. Anything unreadable is forgotten.
    ///
    /// Deliberately infallible: this file is a memory, and a daemon that refused to
    /// start over a stale one would trade a missing fact for a missing product. A
    /// probe writes a whole new copy, so the damage is one question.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(state) = toml::from_str::<Self>(&text) else {
            return Self::default();
        };
        if state.schema_version != PROVIDERS_SCHEMA {
            return Self::default();
        }
        state
    }

    /// The row remembered for `declaration`, when it still describes it.
    #[must_use]
    pub fn row(&self, declaration: &DeclaredProvider) -> Option<&ProviderSummary> {
        self.probes
            .iter()
            .find(|record| record.describes(declaration))
            .map(|record| &record.summary)
    }

    /// Remember `summary` for `declaration`, replacing an older answer in place.
    ///
    /// Replacing rather than appending keeps the file in the order the declarations
    /// were probed, so two readers comparing it do not see a reshuffle.
    pub fn record(&mut self, declaration: &DeclaredProvider, summary: &ProviderSummary) {
        let record = ProbeRecord {
            id: declaration.id.clone(),
            kind: declaration.kind,
            base_url: declaration.base_url.clone(),
            credential_ref: declaration.credential_ref.clone(),
            enabled: declaration.enabled,
            summary: summary.clone(),
        };
        match self
            .probes
            .iter_mut()
            .find(|existing| existing.id == declaration.id)
        {
            Some(existing) => *existing = record,
            None => self.probes.push(record),
        }
    }

    /// Write the file, creating its directory.
    ///
    /// # Errors
    ///
    /// The document cannot be serialised or the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), ProvidersError> {
        let rendered = toml::to_string_pretty(self).map_err(|error| ProvidersError::Write {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()),
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ProvidersError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, rendered).map_err(|source| ProvidersError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        std::fs::rename(&temporary, path).map_err(|source| {
            let _ = std::fs::remove_file(&temporary);
            ProvidersError::Write {
                path: path.to_path_buf(),
                source,
            }
        })
    }
}

impl ProvidersFile {
    /// Where a profile directory keeps its declarations.
    #[must_use]
    pub fn path_in(directory: &Path) -> PathBuf {
        directory.join(PROVIDERS_FILE)
    }

    /// An empty file at this build's schema.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema_version: PROVIDERS_SCHEMA,
            providers: Vec::new(),
        }
    }

    /// Read a file. A missing file is an empty declaration, not an error: a
    /// profile with no provider is the ordinary first-run state.
    ///
    /// # Errors
    ///
    /// The file cannot be read, is not this document, is a newer schema, or
    /// declares one id twice.
    pub fn load(path: &Path) -> Result<Self, ProvidersError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::empty());
            }
            Err(source) => {
                return Err(ProvidersError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let file: Self = toml::from_str(&text).map_err(|error| ProvidersError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        if file.schema_version != PROVIDERS_SCHEMA {
            return Err(ProvidersError::Schema {
                path: path.to_path_buf(),
                found: file.schema_version,
                expected: PROVIDERS_SCHEMA,
            });
        }
        // Two rows with one id would make a recipe's `provider_id` ambiguous, and
        // picking the first would be a silent choice between two servers.
        let mut seen = std::collections::BTreeSet::new();
        for provider in &file.providers {
            if !seen.insert(provider.id.as_str()) {
                return Err(ProvidersError::Duplicate {
                    path: path.to_path_buf(),
                    id: provider.id.clone(),
                });
            }
        }
        Ok(file)
    }

    /// Read the declarations a profile directory holds.
    ///
    /// # Errors
    ///
    /// As [`Self::load`].
    pub fn load_in(directory: &Path) -> Result<Self, ProvidersError> {
        Self::load(&Self::path_in(directory))
    }

    /// Write the file, creating its directory.
    ///
    /// Written beside the target and renamed: an editor that saves by rename and
    /// a reader that arrives mid-write both see one whole document.
    ///
    /// # Errors
    ///
    /// The document cannot be serialised or the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), ProvidersError> {
        let rendered = toml::to_string_pretty(self).map_err(|error| ProvidersError::Write {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()),
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ProvidersError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, rendered).map_err(|source| ProvidersError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        std::fs::rename(&temporary, path).map_err(|source| {
            let _ = std::fs::remove_file(&temporary);
            ProvidersError::Write {
                path: path.to_path_buf(),
                source,
            }
        })
    }

    /// The declaration with this id, when there is one.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&DeclaredProvider> {
        self.providers.iter().find(|provider| provider.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_proto::model::ProviderStatus;

    fn summary(id: &str, status: ProviderStatus) -> ProviderSummary {
        ProviderSummary {
            id: id.to_owned(),
            kind: ProviderKind::LmStudio,
            label: id.to_owned(),
            base_url: "http://127.0.0.1:1234".to_owned(),
            status,
            model_count: 2,
            credential_ref: None,
            last_probed_at: Some("2026-11-12T07:00:00Z".to_owned()),
            detail: Some("answered with 2 models".to_owned()),
        }
    }

    fn provider(id: &str) -> DeclaredProvider {
        DeclaredProvider {
            id: id.to_owned(),
            kind: ProviderKind::LmStudio,
            label: None,
            base_url: "http://127.0.0.1:1234".to_owned(),
            credential_ref: None,
            enabled: true,
        }
    }

    #[test]
    fn a_profile_with_no_file_declares_nothing() {
        let directory = tempfile::tempdir().expect("temp dir");
        let file = ProvidersFile::load_in(directory.path()).expect("a missing file is empty");
        assert_eq!(file, ProvidersFile::empty());
    }

    #[test]
    fn a_typo_is_an_error_and_not_a_provider_that_quietly_lost_a_field() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            ProvidersFile::path_in(directory.path()),
            "schema_version = 1\n\n[[providers]]\nid = \"lm\"\nkind = \"lm_studio\"\nbase_ur = \"http://127.0.0.1:1234\"\n",
        )
        .expect("write");
        let error = ProvidersFile::load_in(directory.path()).expect_err("a typo is a refusal");
        assert!(
            matches!(error, ProvidersError::Parse { .. }),
            "the unknown key is named: {error}"
        );
        assert!(error.to_string().contains("base_ur") || error.to_string().contains("unknown"));
    }

    #[test]
    fn one_id_declared_twice_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            ProvidersFile::path_in(directory.path()),
            "schema_version = 1\n\n[[providers]]\nid = \"lm\"\nkind = \"lm_studio\"\nbase_url = \"http://127.0.0.1:1234\"\n\n[[providers]]\nid = \"lm\"\nkind = \"open_ai_compatible\"\nbase_url = \"http://127.0.0.1:9999\"\n",
        )
        .expect("write");
        let error = ProvidersFile::load_in(directory.path()).expect_err("a duplicate is refused");
        assert!(matches!(error, ProvidersError::Duplicate { .. }), "{error}");
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_half_read() {
        let directory = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            ProvidersFile::path_in(directory.path()),
            "schema_version = 2\n",
        )
        .expect("write");
        let error = ProvidersFile::load_in(directory.path()).expect_err("a newer file is refused");
        assert!(
            matches!(error, ProvidersError::Schema { found: 2, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_written_table_reads_back_and_keeps_its_order() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut file = ProvidersFile::empty();
        file.providers.push(DeclaredProvider {
            label: Some("LM Studio".to_owned()),
            credential_ref: Some("keyring:work/lmstudio".to_owned()),
            ..provider("lm")
        });
        file.providers.push(DeclaredProvider {
            kind: ProviderKind::OpenAiCompatible,
            base_url: "http://127.0.0.1:11434/v1".to_owned(),
            enabled: false,
            ..provider("ollama")
        });
        file.save(&ProvidersFile::path_in(directory.path()))
            .expect("save");

        let read = ProvidersFile::load_in(directory.path()).expect("read back");
        assert_eq!(read, file);
        assert_eq!(read.providers[0].id, "lm");
        assert_eq!(read.providers[1].id, "ollama");
        assert!(!read.providers[1].enabled);
        let text = std::fs::read_to_string(ProvidersFile::path_in(directory.path())).expect("text");
        assert!(text.contains("schema_version = 1"));
        assert!(text.contains("keyring:work/lmstudio"), "{text}");
    }

    #[test]
    fn a_remembered_answer_comes_back_for_the_declaration_it_belongs_to() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = ProvidersState::path_in(directory.path());
        let declared = provider("lm");
        let mut state = ProvidersState::default();
        state.record(&declared, &summary("lm", ProviderStatus::Reachable));
        state.save(&path).expect("save");

        let read = ProvidersState::load(&path);
        let row = read.row(&declared).expect("the answer came back");
        assert_eq!(row.status, ProviderStatus::Reachable);
        assert_eq!(row.model_count, 2);
        assert_eq!(row.last_probed_at.as_deref(), Some("2026-11-12T07:00:00Z"));

        let moved = DeclaredProvider {
            base_url: "http://127.0.0.1:9999".to_owned(),
            ..provider("lm")
        };
        assert!(
            read.row(&moved).is_none(),
            "the same id on another port is a server nobody asked"
        );
    }

    #[test]
    fn a_state_file_that_does_not_parse_is_forgotten_and_not_an_error() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = ProvidersState::path_in(directory.path());
        std::fs::write(&path, "this is not toml").expect("write");
        assert_eq!(ProvidersState::load(&path), ProvidersState::default());

        std::fs::write(&path, "schema_version = 9\nprobes = []\n").expect("write");
        assert_eq!(
            ProvidersState::load(&path),
            ProvidersState::default(),
            "a newer memory is not half-read"
        );
    }

    #[test]
    fn a_declaration_becomes_the_request_that_adds_it() {
        let declaration = DeclaredProvider {
            label: Some("LM Studio".to_owned()),
            credential_ref: Some("keyring:work/lmstudio".to_owned()),
            ..provider("lm")
        };
        let request = declaration.request();
        assert_eq!(request.id, "lm");
        assert_eq!(request.kind, ProviderKind::LmStudio);
        assert_eq!(request.label, "LM Studio");
        assert_eq!(
            request.credential_ref.as_deref(),
            Some("keyring:work/lmstudio")
        );
        assert!(request.enabled);

        let unlabelled = provider("bare").request();
        assert!(
            unlabelled.label.is_empty(),
            "an absent label is the row builder's fallback, not a made-up name"
        );
    }
}
