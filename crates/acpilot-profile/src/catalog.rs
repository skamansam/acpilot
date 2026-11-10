//! Profiles as records: the name, the roots, and the global registry.
//!
//! [`crate::Profile`] is the opened directory — a handle the daemon can ask for a
//! policy and a database path. This module is the data around that handle: the
//! validated name a directory is addressed by, the five roots a profile owns, and
//! the one `config.toml` that says which profile is active and what each one is
//! called. Keeping the two apart keeps `Profile::open` a filesystem question and
//! makes a registry row something a `profile new` can write without inventing a
//! directory first.
//!
//! Nothing here stores a secret. The registry is a table of names and paths; a
//! credential belongs in the OS keyring under `<profile>/<name>`, which P4-5 owns.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// The metadata file every profile directory holds.
const PROFILE_FILE: &str = "config.toml";

/// The registry's schema version.
///
/// A file with another number is refused rather than read partially: a newer
/// binary may have moved a key, and guessing is how a profile's settings are
/// silently lost.
pub const REGISTRY_SCHEMA: u32 = 1;

/// Names the product reserves for built-in behaviour.
///
/// `default` is the profile a launch uses when nothing names another one, so it
/// must resolve as a directory; `user` refuses it (and the other two) for a
/// *new* name without making the built-in unusable.
const RESERVED: [&str; 3] = ["default", "global", "system"];

/// A profile name: the directory segment and keyring namespace.
///
/// The shape is deliberately narrow. It is a path segment, a TOML key and part of
/// a keyring account, so allowing `..`, a slash or whitespace would make one
/// spelling mean different things in three places.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileId(String);

impl ProfileId {
    /// Parse a name, accepting any case and storing the lower-case spelling.
    ///
    /// # Errors
    ///
    /// The name is not 2–32 characters, or contains anything but lower-case
    /// letters, digits and hyphens, or does not start with a letter or digit.
    pub fn parse(name: &str) -> Result<Self, ProfileIdError> {
        let name = name.to_ascii_lowercase();
        let bytes = name.as_bytes();
        let shape_ok = (2..=32).contains(&bytes.len())
            && bytes
                .first()
                .is_some_and(|first| first.is_ascii_alphanumeric())
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-');
        if !shape_ok {
            return Err(ProfileIdError::Shape);
        }
        Ok(Self(name))
    }

    /// Parse a name for a profile a person is creating.
    ///
    /// [`ProfileId::parse`] knows the shape; this also refuses the three names the
    /// product reserves, because `acpilot profile new default` is a request to
    /// shadow behaviour rather than to add a profile.
    ///
    /// # Errors
    ///
    /// The shape error [`ProfileId::parse`] gives, or the name is reserved.
    pub fn user(name: &str) -> Result<Self, ProfileIdError> {
        let id = Self::parse(name)?;
        if id.is_reserved() {
            return Err(ProfileIdError::Reserved(id.0));
        }
        Ok(id)
    }

    /// The name as a path segment or key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is one of the names the product reserves.
    #[must_use]
    pub fn is_reserved(&self) -> bool {
        RESERVED.contains(&self.0.as_str())
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self("default".to_owned())
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for ProfileId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for ProfileId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProfileId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::parse(&name).map_err(D::Error::custom)
    }
}

/// Why a name could not become a [`ProfileId`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProfileIdError {
    /// The name is empty, too long, or uses a character a path or key cannot carry.
    #[error(
        "a profile name must be 2 to 32 characters using lower-case letters, digits or \
         hyphens, and start with a letter or digit"
    )]
    Shape,

    /// The name is one the product reserves.
    #[error("`{0}` is reserved for a built-in profile; choose another name")]
    Reserved(String),
}

/// One profile as the registry records it.
///
/// The id is the table key (`[profiles.work]`), not a field repeated inside it:
/// two spellings of the same name in one row is a disagreement waiting to happen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileRecord {
    /// The human label; the id when empty.
    pub label: String,
    /// The profile this one inherits beneath, when it does.
    pub extends: Option<ProfileId>,
    /// When the profile was created, as an RFC 3339 string.
    pub created_at: Option<String>,
}

impl ProfileRecord {
    /// A record with a label, no parent and no creation time yet.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Self::default()
        }
    }

    /// The label to draw, falling back to the id when none was given.
    #[must_use]
    pub fn label_or<'a>(&'a self, id: &'a ProfileId) -> &'a str {
        if self.label.is_empty() {
            id.as_str()
        } else {
            &self.label
        }
    }

    /// The label to draw when the id is already a string.
    #[must_use]
    pub fn label_or_name(&self, id: &str) -> String {
        if self.label.is_empty() {
            id.to_owned()
        } else {
            self.label.clone()
        }
    }
}

/// The five directories one profile owns.
///
/// Config and data are separate on a platform that draws that line; they are the
/// same directory under `ACPILOT_HOME`, because a portable install asks for every
/// file under one root rather than reproducing the platform's split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRoots {
    /// Settings: `config.toml`, `agents.toml`, `lsp.toml`, `policy.toml`.
    pub config: PathBuf,
    /// State that must survive: the database and transcripts.
    pub data: PathBuf,
    /// Regenerable bytes: caches and downloaded models.
    pub cache: PathBuf,
    /// Logs and other machine-local state.
    pub state: PathBuf,
    /// The socket, lock and token for this profile, when it needs its own.
    pub runtime: PathBuf,
}

impl ProfileRoots {
    /// The roots for `id` on this machine.
    ///
    /// # Panics
    ///
    /// Never: a platform with no project directories falls back under the
    /// temporary directory, which is the same answer [`crate::config_dir`] gives.
    #[must_use]
    pub fn for_id(id: &ProfileId) -> Self {
        if let Some(home) = std::env::var_os("ACPILOT_HOME") {
            return Self::under(PathBuf::from(home), id);
        }
        match ProjectDirs::from("", "", "acpilot") {
            Some(dirs) => {
                let name = id.as_str();
                let runtime = dirs
                    .runtime_dir()
                    .map_or_else(|| std::env::temp_dir().join("acpilot"), Path::to_path_buf)
                    .join("profiles")
                    .join(name);
                Self {
                    config: dirs.config_dir().join("profiles").join(name),
                    data: dirs.data_dir().join("profiles").join(name),
                    cache: dirs.cache_dir().join("profiles").join(name),
                    state: dirs
                        .state_dir()
                        .unwrap_or_else(|| dirs.data_local_dir())
                        .join("profiles")
                        .join(name),
                    runtime,
                }
            }
            None => Self::under(std::env::temp_dir().join("acpilot"), id),
        }
    }

    /// The roots for `id` under one home directory.
    ///
    /// The shape `ACPILOT_HOME` uses, and the shape a test can own without
    /// touching the process environment.
    #[must_use]
    pub fn under(home: impl Into<PathBuf>, id: &ProfileId) -> Self {
        let home = home.into();
        let name = id.as_str();
        Self {
            config: home.join("profiles").join(name),
            data: home.join("profiles").join(name),
            cache: home.join("cache").join("profiles").join(name),
            state: home.join("state").join("profiles").join(name),
            runtime: home.join("runtime").join("profiles").join(name),
        }
    }

    /// Create every directory, so a caller can write the first file.
    ///
    /// # Errors
    ///
    /// The filesystem refused a `create_dir_all`; the error names nothing because
    /// the caller has the paths and the message would repeat one of them.
    pub fn create(&self) -> io::Result<()> {
        for path in [
            &self.config,
            &self.data,
            &self.cache,
            &self.state,
            &self.runtime,
        ] {
            fs::create_dir_all(path)?;
        }
        Ok(())
    }

    /// The profile's settings file.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config.join(PROFILE_FILE)
    }

    /// The profile's database file.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.data.join(acpilot_store::FILE_NAME)
    }

    /// Where this profile's JSONL transcripts live.
    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.data.join("sessions")
    }
}

/// The global registry: `config.toml` at the config root.
///
/// It is the same file the settings cascade reads, so a save here must not erase
/// `[preferences]` or a table a newer binary wrote. Unknown top-level tables are
/// kept in [`Registry::other`] for exactly that reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Registry {
    /// The schema this file was written with.
    #[serde(default = "registry_schema")]
    pub schema_version: u32,
    /// The profile a launch uses when nothing else names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<ProfileId>,
    /// `[daemon]`, as the daemon's own settings cascade reads it.
    pub daemon: BTreeMap<String, toml::Value>,
    /// One table per profile, keyed by the id.
    pub profiles: BTreeMap<ProfileId, ProfileRecord>,
    /// Tables this version does not know, preserved across a save.
    #[serde(flatten)]
    pub other: BTreeMap<String, toml::Value>,
}

fn registry_schema() -> u32 {
    REGISTRY_SCHEMA
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA,
            active_profile: None,
            daemon: BTreeMap::new(),
            profiles: BTreeMap::new(),
            other: BTreeMap::new(),
        }
    }
}

impl Registry {
    /// Read the registry at `path`, or the defaults when it does not exist.
    ///
    /// A missing file is not an error: a fresh install has no profiles and no
    /// active name, and creating one is `profile new`'s job rather than a read's.
    ///
    /// # Errors
    ///
    /// The file exists and cannot be read, is not TOML, or was written with a
    /// schema this binary does not know.
    pub fn load(path: &Path) -> Result<Self, RegistryError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(RegistryError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let registry: Self = toml::from_str(&text).map_err(|error| RegistryError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        if registry.schema_version != REGISTRY_SCHEMA {
            return Err(RegistryError::Unsupported {
                path: path.to_path_buf(),
                found: registry.schema_version,
            });
        }
        Ok(registry)
    }

    /// Write the registry, creating the config root when it is absent.
    ///
    /// The file is not `0600`: it holds names and settings, never a secret. A
    /// secret in this file is the failure P4-5's keyring namespace exists to
    /// prevent.
    ///
    /// # Errors
    ///
    /// The directory or file could not be written, or the values could not be
    /// represented as TOML.
    pub fn save(&self, path: &Path) -> Result<(), RegistryError> {
        let text = toml::to_string_pretty(self).map_err(|error| RegistryError::Write {
            path: path.to_path_buf(),
            source: io::Error::other(error.to_string()),
        })?;
        if let Some(directory) = path.parent()
            && !directory.as_os_str().is_empty()
        {
            fs::create_dir_all(directory).map_err(|source| RegistryError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        fs::write(path, text).map_err(|source| RegistryError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Put `record` under `id`, replacing any existing row.
    pub fn insert(&mut self, id: ProfileId, record: ProfileRecord) {
        self.profiles.insert(id, record);
    }

    /// The row for `id`, when it exists.
    #[must_use]
    pub fn profile(&self, id: &ProfileId) -> Option<&ProfileRecord> {
        self.profiles.get(id)
    }

    /// The row for the active profile, when one is named and present.
    #[must_use]
    pub fn active(&self) -> Option<(&ProfileId, &ProfileRecord)> {
        let id = self.active_profile.as_ref()?;
        self.profiles.get_key_value(id)
    }
}

/// Why the registry could not be read or written.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// The file is there and could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The registry path.
        path: PathBuf,
        /// The read failure.
        source: io::Error,
    },

    /// The file is not the TOML the registry expects.
    #[error("{} is not a usable profile registry: {message}", path.display())]
    Parse {
        /// The registry path.
        path: PathBuf,
        /// TOML's message, which names the line.
        message: String,
    },

    /// The file was written with another schema.
    #[error(
        "{} uses profile registry schema {found}; this build reads {REGISTRY_SCHEMA}",
        path.display()
    )]
    Unsupported {
        /// The registry path.
        path: PathBuf,
        /// The version that was found.
        found: u32,
    },

    /// The directory or file could not be written.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// The registry path.
        path: PathBuf,
        /// The write failure.
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> ProfileId {
        ProfileId::parse(name).expect("a valid profile name")
    }

    #[test]
    fn a_name_is_case_insensitive_and_shape_checked() {
        assert_eq!(id("Work").as_str(), "work");
        assert_eq!(id("client-acme").as_str(), "client-acme");
        assert_eq!(id("a1").as_str(), "a1");

        for bad in ["", "a", "-work", "work_space", "wörk", "../escape"] {
            assert!(
                ProfileId::parse(bad).is_err(),
                "`{bad}` is not a path segment and a key"
            );
        }
        assert!(ProfileId::parse(&"a".repeat(33)).is_err(), "over 32 bytes");
    }

    #[test]
    fn the_reserved_names_are_usable_but_not_creatable() {
        assert!(id("Default").is_reserved());
        assert!(ProfileId::user("global").is_err());
        assert!(ProfileId::user("system").is_err());
        assert_eq!(
            ProfileId::user("Client-Acme")
                .expect("a user name")
                .as_str(),
            "client-acme"
        );
    }

    #[test]
    fn the_roots_are_five_directories_under_one_home() {
        let home = tempfile::tempdir().expect("temp dir");
        let roots = ProfileRoots::under(home.path(), &id("work"));

        assert_eq!(roots.config, home.path().join("profiles/work"));
        assert_eq!(roots.data, home.path().join("profiles/work"));
        assert_eq!(roots.cache, home.path().join("cache/profiles/work"));
        assert_eq!(roots.state, home.path().join("state/profiles/work"));
        assert_eq!(roots.runtime, home.path().join("runtime/profiles/work"));
        assert_eq!(roots.config_file(), roots.config.join("config.toml"));
        assert!(roots.database_path().ends_with(acpilot_store::FILE_NAME));
        assert!(
            roots.database_path().ends_with("profile.sqlite"),
            "the P4-4 name: {}",
            roots.database_path().display()
        );
        assert!(roots.sessions_dir().ends_with("sessions"));

        roots.create().expect("the roots are writable");
        for path in [
            roots.config,
            roots.data,
            roots.cache,
            roots.state,
            roots.runtime,
        ] {
            assert!(path.is_dir(), "{} was not created", path.display());
        }
    }

    #[test]
    fn a_registry_round_trip_keeps_tables_it_does_not_own() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "schema_version = 1\nactive_profile = \"work\"\n\n\
             [preferences]\ntab_width = 2\n\n\
             [daemon]\nforeground = true\n\n\
             [profiles.work]\nlabel = \"Work\"\nextends = \"default\"\n\
             created_at = \"2026-11-08T00:00:00Z\"\n",
        )
        .expect("write registry");

        let registry = Registry::load(&path).expect("the registry loads");
        let work = registry
            .profile(&id("work"))
            .expect("the work row is there");
        assert_eq!(work.label, "Work");
        assert_eq!(
            work.extends.as_ref().map(ProfileId::as_str),
            Some("default")
        );
        assert_eq!(
            registry.active().map(|(id, _)| id.as_str()),
            Some("work"),
            "the active profile resolves to its row"
        );
        assert!(
            registry.other.contains_key("preferences"),
            "a table this type does not own is kept"
        );

        registry.save(&path).expect("the registry saves");
        let text = fs::read_to_string(&path).expect("read back");
        assert!(text.contains("[preferences]"), "{text}");
        assert!(text.contains("tab_width = 2"), "{text}");
        assert_eq!(
            Registry::load(&path).expect("reload"),
            registry,
            "a save must not lose what a load accepted"
        );
    }

    #[test]
    fn a_missing_registry_is_the_defaults_and_another_schema_is_refused() {
        let directory = tempfile::tempdir().expect("temp dir");
        let missing = directory.path().join("absent.toml");
        let registry = Registry::load(&missing).expect("missing is defaults");
        assert_eq!(registry.schema_version, REGISTRY_SCHEMA);
        assert!(registry.profiles.is_empty());

        let future = directory.path().join("future.toml");
        fs::write(&future, "schema_version = 2\n").expect("write");
        let error = Registry::load(&future).expect_err("a newer schema is refused");
        assert!(
            error.to_string().contains("schema 2"),
            "the message names the version: {error}"
        );
    }
}
