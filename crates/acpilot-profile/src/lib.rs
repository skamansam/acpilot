//! Profiles: one identity bundle per context (`work`, `personal`, `client-acme`).
//!
//! A profile is a directory, and `config.toml` inside it is what the daemon
//! reads at startup. Today this crate reads one table of that file — `[policy]`,
//! the rules a running [`Broker`](acpilot_agent::Broker) answers from — because
//! that is the part a daemon needs before it can open a session. The rest of the
//! file (preferences, language servers, the keyring namespace) is the profiles phase's (P4), and it
//! will be read here rather than somewhere else for the same reason.
//!
//! Resolution is explicit and loud: a directory that does not hold a
//! `config.toml` is an error, not an invitation to use the defaults. A profile
//! path that silently falls back is how a client's work ends up in a personal
//! profile, and that is the failure this crate exists to prevent. A `config.toml`
//! that *does* exist but has no `[policy]` table is a different thing: the
//! profile has said nothing about permissions, so the shipped policy applies.

pub mod catalog;
pub mod launch;
pub mod project;
pub mod settings;
pub mod watch;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use acpilot_agent::{Policy, PolicyError};
use thiserror::Error;

pub use catalog::{
    ProfileId, ProfileIdError, ProfileRecord, ProfileRoots, Registry, RegistryError,
};

/// The file a profile keeps its settings in.
pub const CONFIG_FILE: &str = "config.toml";

/// The platform's config root for Acpilot: `~/.config/acpilot` on Linux,
/// Application Support on macOS, `%APPDATA%\acpilot` on Windows.
///
/// `ACPILOT_HOME` overrides it, which is what tests, portable installs and CI use:
/// every one of them wants the same code path on a directory it owns. A machine
/// with no home directory at all falls back to a directory under the temporary
/// one — a config root that does not exist is still one a caller can name without
/// a second error path.
///
/// This crate is the only one allowed to call `directories`, so "where does this
/// file go" has one answer for the daemon, the CLI and the GUI alike.
#[must_use]
pub fn config_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("ACPILOT_HOME") {
        return PathBuf::from(home);
    }
    directories::ProjectDirs::from("", "", "acpilot").map_or_else(
        || std::env::temp_dir().join("acpilot"),
        |directories| directories.config_dir().to_path_buf(),
    )
}

/// The platform's data root for Acpilot: `~/.local/share/acpilot` on Linux,
/// Application Support on macOS, `%LOCALAPPDATA%\\acpilot` on Windows.
///
/// [`config_dir`] is where settings live and this is where state lives — a
/// transcript, a downloaded model, an installed extension. They are the same
/// directory on a platform that draws no line between them, and separate on one
/// that does; either way this crate is the only caller of `directories`, so
/// "where does this file go" has one answer.
///
/// `ACPILOT_HOME` overrides it together with [`config_dir`], because a test or a
/// portable install wants every file Acpilot writes under one directory it owns.
#[must_use]
pub fn data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("ACPILOT_HOME") {
        return PathBuf::from(home);
    }
    directories::ProjectDirs::from("", "", "acpilot").map_or_else(
        || std::env::temp_dir().join("acpilot"),
        |directories| directories.data_dir().to_path_buf(),
    )
}

/// Where profiles live: one directory per name under `<config>/profiles`.
#[must_use]
pub fn profiles_dir() -> PathBuf {
    config_dir().join("profiles")
}

/// The user's settings file: `<config>/config.toml`.
#[must_use]
pub fn user_config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE)
}

/// The global profile/workspace index: `<data>/index.sqlite`.
///
/// One file for every profile, because its question is cross-profile: a client
/// listing profiles must not open each profile's database to find them. It holds
/// no conversation, so it is safe to lose and rebuild.
#[must_use]
pub fn index_path() -> PathBuf {
    data_dir().join(acpilot_store::INDEX_FILE)
}

/// One profile's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    root: PathBuf,
}

impl Profile {
    /// Open the profile rooted at `directory`.
    ///
    /// The directory must exist and hold a `config.toml`: pointing the daemon at
    /// the wrong path is a startup failure with the path in the message, not a
    /// daemon that runs on defaults nobody chose.
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self, ProfileError> {
        let root = directory.into();
        let profile = Self { root };
        let config = profile.config_path();
        if !config.is_file() {
            return Err(ProfileError::NoConfig { path: config });
        }
        Ok(profile)
    }

    /// The profile a name resolves to: `<config>/profiles/<name>`.
    ///
    /// One resolver, so a marker, a CLI flag and a stored `active_profile` cannot
    /// disagree about which directory a name means. A name that is not there is an
    /// error rather than a directory made on demand: silently creating `work` when
    /// someone typed `wrk` is how a work prompt goes out under a default profile.
    ///
    /// # Errors
    ///
    /// The directory does not exist or does not hold a `config.toml`.
    pub fn named(name: &str) -> Result<Self, ProfileError> {
        Self::named_in(&profiles_dir(), name)
    }

    /// The profile a name resolves to under an explicit profiles root.
    ///
    /// The same name-to-directory rule as [`Self::named`], for a caller that
    /// owns the root: the launch resolver's tests, a portable install, and the
    /// desktop app's `--profile <name>` list.
    ///
    /// # Errors
    ///
    /// The name is not a profile id, or the directory does not exist or does not
    /// hold a `config.toml`.
    pub fn named_in(profiles: &Path, name: &str) -> Result<Self, ProfileError> {
        let id = ProfileId::parse(name)?;
        Self::open(profiles.join(id.as_str()))
    }

    /// The profile's directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where this profile's settings live.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
    }

    /// Where this profile's preferences live.
    ///
    /// `config.toml` is still a layer below it, because profiles written before
    /// this file existed have to keep working. The name belongs to the settings
    /// cascade that reads it, so a watcher asks this path rather than spelling
    /// `profile.toml` a second time.
    #[must_use]
    pub fn preferences_path(&self) -> PathBuf {
        self.root.join(settings::PROFILE_SETTINGS_FILE)
    }

    /// Where this profile keeps its state: transcripts, the database, caches.
    ///
    /// One directory per profile, under the platform's data root, so two profiles
    /// cannot share a conversation list or a session id that means two things. A
    /// path with no last segment (a filesystem root) names no profile and gets the
    /// shared root, which is the same answer [`Self::name`] gives.
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        match self.name() {
            Some(name) => data_dir().join("profiles").join(name),
            None => data_dir(),
        }
    }

    /// Where this profile declares its agents.
    ///
    /// The file name belongs to `acpilot-agent`: the reader that parses it is the
    /// thing that decides what it is called, so a daemon that needs the agents
    /// asks the profile for the path rather than spelling `agents.toml` a second
    /// time.
    #[must_use]
    pub fn agents_path(&self) -> PathBuf {
        self.root.join(acpilot_agent::MANIFEST_FILE)
    }

    /// The profile's name: the last segment of its directory.
    ///
    /// The keyring namespaces every secret by the profile name, and resolution in
    /// the profiles phase's task P4-2 turns a name into exactly
    /// `<config>/profiles/<name>`, so the
    /// segment the directory was opened by is the name those secrets were written
    /// under. `None` for a path with no last segment (`..`, a filesystem root).
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.root.file_name().and_then(std::ffi::OsStr::to_str)
    }

    /// The validated id this directory is addressed by, when its name is one.
    ///
    /// Temporary directories and hand-made paths are real profiles to the daemon,
    /// so an invalid segment is `None` rather than an error: the catalogue is what
    /// a named profile goes through, and this is how a caller asks whether this
    /// handle has a row there.
    #[must_use]
    pub fn id(&self) -> Option<ProfileId> {
        self.name().and_then(|name| ProfileId::parse(name).ok())
    }

    /// The five roots this profile owns, when its directory name is a valid id.
    #[must_use]
    pub fn roots(&self) -> Option<ProfileRoots> {
        self.id().map(|id| ProfileRoots::for_id(&id))
    }

    /// The database that holds this profile's conversations (the service phase's task P3-7).
    ///
    /// The file name belongs to `acpilot-store`, because the crate that migrates the
    /// schema is the thing that decides what it is called. Opening it through
    /// `Store::open_profile` adopts the pre-P4-4 `acpilot.db` name when needed.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.data_dir().join(acpilot_store::FILE_NAME)
    }

    /// Where this profile's JSONL transcripts live.
    ///
    /// Under the profile's own data directory, not the shared root: two profiles
    /// may use the same session id, and a shared directory would let one read the
    /// other's conversation.
    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir().join("sessions")
    }

    /// The `[policy]` table, or the shipped policy when the file has none.
    ///
    /// The whole file is handed to the parser, which is what makes a real
    /// `config.toml` — `schema_version`, `[policy]`, `[lsp.*]` — readable here
    /// without this crate knowing what the other tables mean.
    pub fn policy(&self) -> Result<Policy, ProfileError> {
        let path = self.config_path();
        let text = fs::read_to_string(&path).map_err(|source| ProfileError::Read {
            path: path.clone(),
            source,
        })?;
        Policy::from_toml(&text).map_err(|source| ProfileError::Policy { path, source })
    }
}

/// What went wrong reading a profile.
#[derive(Debug, Error)]
pub enum ProfileError {
    /// The profile directory has no `config.toml`.
    #[error("{} is not a profile: it has no {CONFIG_FILE}", path.display())]
    NoConfig {
        /// The path that was looked at.
        path: PathBuf,
    },

    /// The file is there and could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file that failed.
        path: PathBuf,
        /// Why it failed.
        source: io::Error,
    },

    /// The `[policy]` table is not valid.
    #[error("{} has an invalid [policy]: {source}", path.display())]
    Policy {
        /// The file that was read.
        path: PathBuf,
        /// Which key, and what it said.
        source: PolicyError,
    },

    /// A name that is not a profile id was used to resolve a named profile.
    #[error("not a profile name: {0}")]
    Name(#[from] ProfileIdError),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use acpilot_agent::{Decision, PermissionKind, Subject};

    use super::*;

    fn profile(config: &str) -> (tempfile::TempDir, Profile) {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join(CONFIG_FILE), config).expect("write config");
        let profile = Profile::open(dir.path()).expect("the profile opens");
        (dir, profile)
    }

    #[test]
    fn a_profile_without_a_config_is_an_error_naming_the_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = Profile::open(dir.path()).expect_err("an empty directory is not a profile");
        assert!(
            error.to_string().contains("config.toml"),
            "the message says what is missing: {error}"
        );
        assert!(matches!(error, ProfileError::NoConfig { .. }));
    }

    #[test]
    fn rules_in_the_config_reach_the_policy() {
        let (_dir, profile) = profile(
            r#"
schema_version = 1

[policy]
default = "deny"

[[policy.rules]]
id = "allow-format"
decision = "allow"
tool = "execute"
path = "/work/repo/**"

[lsp.rust]
command = "rust-analyzer"
"#,
        );

        let policy = profile.policy().expect("the policy parses");
        assert_eq!(policy.rules().len(), 1, "the rule reached the engine");

        let path = PathBuf::from("/work/repo/src/main.rs");
        let request = Subject::new(PermissionKind::Terminal, "cargo fmt")
            .tool("execute")
            .path(&path);
        assert_eq!(
            policy.decide(&request, true).decision,
            Decision::Allow,
            "a rule from the file decides the request"
        );

        let other = Subject::new(PermissionKind::Terminal, "cargo publish").tool("execute");
        assert_eq!(
            policy.decide(&other, true).decision,
            Decision::Deny,
            "`default = \"deny\"` governs what no rule names"
        );
    }

    #[test]
    fn a_config_with_no_policy_table_is_the_shipped_policy() {
        let (_dir, profile) = profile("schema_version = 1\n");
        let policy = profile.policy().expect("no [policy] is not an error");
        assert!(policy.rules().is_empty());
    }

    #[test]
    fn a_misspelled_decision_fails_the_open_rather_than_the_turn() {
        let (_dir, profile) = profile("[policy]\ndefault = \"askk\"\n");
        let error = profile.policy().expect_err("a typo is refused");
        assert!(
            error.to_string().contains("askk"),
            "the message quotes what it found: {error}"
        );
        assert!(matches!(error, ProfileError::Policy { .. }));
    }

    #[test]
    fn the_config_path_is_the_one_that_was_opened() {
        let (dir, profile) = profile("schema_version = 1\n");
        assert_eq!(profile.config_path(), dir.path().join(CONFIG_FILE));
        assert_eq!(profile.root(), dir.path());
    }

    #[test]
    fn the_agents_are_next_to_the_config_and_named_by_the_agent_crate() {
        let (dir, profile) = profile("schema_version = 1\n");
        assert_eq!(
            profile.agents_path(),
            dir.path().join(acpilot_agent::MANIFEST_FILE)
        );
        assert!(
            profile.agents_path().ends_with("agents.toml"),
            "the path a person would edit: {}",
            profile.agents_path().display()
        );
    }

    #[test]
    fn the_name_is_the_directory_the_profile_was_opened_by() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("work");
        fs::create_dir(&root).expect("profile directory");
        fs::write(root.join(CONFIG_FILE), "schema_version = 1\n").expect("write config");
        let profile = Profile::open(&root).expect("the profile opens");
        assert_eq!(
            profile.name(),
            Some("work"),
            "the keyring namespace is the directory name"
        );
        assert_eq!(
            profile.id().as_ref().map(ProfileId::as_str),
            Some("work"),
            "a valid directory name is also a profile id"
        );
    }

    #[test]
    fn a_named_profile_cannot_climb_out_of_the_profile_root() {
        let error = Profile::named("../escape").expect_err("a path is not a profile name");
        assert!(
            matches!(error, ProfileError::Name(_)),
            "the name is refused before any directory is opened: {error}"
        );
    }

    #[test]
    fn state_paths_are_scoped_to_the_profile() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("work");
        fs::create_dir(&root).expect("profile directory");
        fs::write(root.join(CONFIG_FILE), "schema_version = 1\n").expect("write config");
        let profile = Profile::open(&root).expect("the profile opens");

        assert!(
            profile
                .database_path()
                .ends_with("profiles/work/profile.sqlite"),
            "the P4-4 database name: {}",
            profile.database_path().display()
        );
        assert!(
            profile.sessions_dir().ends_with("profiles/work/sessions"),
            "transcripts are under this profile: {}",
            profile.sessions_dir().display()
        );
        assert!(
            crate::index_path().ends_with("index.sqlite"),
            "the index is global: {}",
            crate::index_path().display()
        );
    }
}
