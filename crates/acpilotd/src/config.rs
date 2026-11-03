//! The daemon's settings: two files, then the flags.
//!
//! The service phase's task P3-1 asked for a `figment` cascade, layered the way the
//! profiles phase's task P4-2 layers one for every Acpilot binary: the user's
//! `<config>/config.toml`, then the profile's own `config.toml`, then whatever the
//! command line said. Flags last because they are the only layer a person typed
//! this time, and a file edited months ago does not get to overrule that.
//!
//! Only the `[daemon]` table is read here. The rest of a profile's `config.toml` is
//! the permission policy, the language servers and the preferences, and each has an
//! owner that parses it — a second reader of the same file in the daemon would be a
//! second opinion about what a key means.
//!
//! ```toml
//! [daemon]
//! addr = "127.0.0.1:7777"   # opt-in TCP; the unix socket is the default
//! workspace = "/srv/project"
//! foreground = true
//! ```
//!
//! No `figment` environment provider: the two flags that have one (`--addr` via
//! `ACPILOT_ADDR`, and `--profile`) are already read by `clap`, and a second spelling
//! of an environment variable is a variable nobody can find in either place.

use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Format, Toml};
use serde::{Deserialize, Serialize};

use crate::platform::{Target, resolve_target};

/// The `[daemon]` table, as a file writes it.
///
/// Every field is optional, because each one has a layer below it: the table is an
/// override, not a full description of the daemon.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Daemon {
    /// A unix socket to bind instead of the platform default.
    pub socket: Option<PathBuf>,
    /// A TCP address to bind instead of the socket.
    pub addr: Option<String>,
    /// The workspace root sessions open in.
    pub workspace: Option<PathBuf>,
    /// The profile directory to serve.
    pub profile: Option<PathBuf>,
    /// The SQLite file to keep sessions in.
    pub database: Option<PathBuf>,
    /// Where the running daemon's pid is written.
    pub pidfile: Option<PathBuf>,
    /// Log to stderr only, writing no JSON log file.
    pub foreground: Option<bool>,
}

/// What the command line asked for.
///
/// A flag is `Some` only when it was given, so "not mentioned" stays distinct from
/// "set to the default" — which is what makes the cascade a merge rather than a
/// precedence rule with a sentinel in it.
#[derive(Debug, Clone, Default)]
pub struct Flags {
    /// `--addr`.
    pub addr: Option<String>,
    /// `--socket`.
    pub socket: Option<PathBuf>,
    /// `--profile`.
    pub profile: Option<PathBuf>,
    /// `--workspace`.
    pub workspace: Option<PathBuf>,
    /// `--database`.
    pub database: Option<PathBuf>,
    /// `--pidfile`.
    pub pidfile: Option<PathBuf>,
    /// `--foreground`.
    pub foreground: bool,
}

/// The daemon, once every layer has had its say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// What to bind.
    pub target: Target,
    /// The profile to load, when one was named.
    pub profile: Option<PathBuf>,
    /// The workspace root sessions open in.
    pub workspace: PathBuf,
    /// The SQLite file the profile's sessions are kept in.
    pub database: PathBuf,
    /// Where this process writes its pid.
    pub pidfile: PathBuf,
    /// Log to stderr only, and write no JSON log file.
    pub foreground: bool,
}

/// Resolve the daemon's settings from the files and the flags.
///
/// # Errors
///
/// A settings file exists and does not parse, both endpoints were named, or the
/// profile names a database directory that cannot be placed. Every message names
/// the file or the flag it came from.
pub fn resolve(flags: Flags) -> Result<Config, ConfigError> {
    let user = acpilot_profile::user_config_path();
    // Read twice: the first pass names the profile, and the profile's file is a
    // layer of the second. Reading one file twice is cheaper than a rule that says
    // "a profile can only be named in a file if the file is the user's".
    let first = effective(read(std::slice::from_ref(&user))?, &flags);
    let layers = match &first.profile {
        Some(directory) => read(&[user, profile_file(directory)])?,
        None => first,
    };
    let settings = effective(layers, &flags);

    let target = resolve_target(settings.addr.clone(), settings.socket.clone())
        .map_err(ConfigError::Endpoint)?;
    let profile = settings.profile.clone();
    let database = settings
        .database
        .clone()
        .unwrap_or_else(|| default_database(profile.as_deref()));
    let pidfile = settings
        .pidfile
        .clone()
        .unwrap_or_else(|| acpilot_host::token::path().with_file_name("acpilotd.pid"));
    Ok(Config {
        target,
        profile,
        workspace: settings.workspace.unwrap_or_else(|| PathBuf::from(".")),
        database,
        pidfile,
        foreground: settings.foreground.unwrap_or(false),
    })
}

/// Flags over layers, field by field.
///
/// `pub(crate)` so the tests can merge without a home directory: the file layer is
/// a value here, which is the whole reason this is a function rather than four
/// lines inside [`resolve`].
pub(crate) fn effective(layers: Daemon, flags: &Flags) -> Daemon {
    Daemon {
        socket: flags.socket.clone().or(layers.socket),
        addr: flags.addr.clone().or(layers.addr),
        workspace: flags.workspace.clone().or(layers.workspace),
        profile: flags.profile.clone().or(layers.profile),
        database: flags.database.clone().or(layers.database),
        pidfile: flags.pidfile.clone().or(layers.pidfile),
        foreground: flags.foreground.then_some(true).or(layers.foreground),
    }
}

/// The merged `[daemon]` tables of `paths`, later files winning.
///
/// A missing file is not an error: every layer is optional by definition, and a
/// machine that never wrote a `config.toml` runs on the defaults.
fn read(paths: &[PathBuf]) -> Result<Daemon, ConfigError> {
    let mut figment = Figment::new();
    for path in paths {
        figment = figment.merge(Toml::file(path));
    }
    let layered: Layered = figment.extract().map_err(|source| ConfigError::File {
        message: source.to_string(),
    })?;
    Ok(layered.daemon)
}

/// The file a profile keeps its `[daemon]` table in.
fn profile_file(directory: &Path) -> PathBuf {
    directory.join(acpilot_profile::CONFIG_FILE)
}

/// Where a profile's database goes when no layer names one.
///
/// A named profile gets `<data>/acpilot/profiles/<name>/acpilot.db`, so two profiles
/// cannot read each other's conversations; a daemon with no profile gets the shared
/// root, which is the profile-less answer to the same question.
fn default_database(profile: Option<&Path>) -> PathBuf {
    profile
        .and_then(|directory| acpilot_profile::Profile::open(directory).ok())
        .map_or_else(
            || acpilot_profile::data_dir().join(acpilot_store::FILE_NAME),
            |profile| profile.database_path(),
        )
}

/// What `figment` extracts: the table under its key.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Layered {
    daemon: Daemon,
}

/// Why the settings could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A settings file exists and is not usable.
    #[error("a settings file is not usable: {message}")]
    File {
        /// figment's message, which names the file and line.
        message: String,
    },

    /// `--addr` and `--socket` were both named.
    #[error("{0}")]
    Endpoint(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write");
    }

    #[test]
    fn a_later_file_wins_and_a_flag_beats_both() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = directory.path().join("config.toml");
        let profile = directory.path().join("profile.toml");
        write(
            &user,
            "[daemon]\nworkspace = \"/srv/user\"\nforeground = false\n",
        );
        write(
            &profile,
            "[daemon]\nworkspace = \"/srv/profile\"\nforeground = true\n",
        );

        let layers = read(&[user, profile]).expect("the cascade");
        assert_eq!(layers.workspace, Some(PathBuf::from("/srv/profile")));
        assert_eq!(layers.foreground, Some(true));

        let merged = effective(
            layers,
            &Flags {
                workspace: Some(PathBuf::from("/srv/flag")),
                ..Flags::default()
            },
        );
        assert_eq!(merged.workspace, Some(PathBuf::from("/srv/flag")));
        assert_eq!(
            merged.foreground,
            Some(true),
            "an unmentioned flag keeps the file"
        );
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let directory = tempfile::tempdir().expect("temp dir");
        let layers = read(&[directory.path().join("absent.toml")]).expect("no file, no error");
        assert_eq!(layers, Daemon::default());
    }

    #[test]
    fn a_file_that_does_not_parse_says_so() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("config.toml");
        write(&path, "[daemon\nworkspace = 1\n");
        let error = read(std::slice::from_ref(&path)).expect_err("a broken file is an error");
        assert!(
            error.to_string().contains("daemon") || error.to_string().contains("config"),
            "{error}"
        );
    }

    #[test]
    fn an_absent_flag_does_not_erase_a_file_value() {
        let layers = Daemon {
            addr: Some("127.0.0.1:9000".into()),
            foreground: Some(true),
            ..Daemon::default()
        };
        let merged = effective(layers, &Flags::default());
        assert_eq!(merged.addr.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(merged.foreground, Some(true));
    }

    #[test]
    fn the_daemon_table_is_not_the_only_table_read() {
        // A profile's `config.toml` also holds `[policy]` and `[lsp.*]`; an extract
        // that refused unknown keys would make every real profile unusable.
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("config.toml");
        write(
            &path,
            "schema_version = 1\n[policy]\ndefault = \"ask\"\n[daemon]\nforeground = true\n",
        );
        let layers = read(&[path]).expect("the other tables are ignored");
        assert_eq!(layers.foreground, Some(true));
    }
}
