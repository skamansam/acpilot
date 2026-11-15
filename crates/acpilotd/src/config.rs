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
    /// Answer cross-origin requests, for a browser served from somewhere else.
    ///
    /// Off by default: the SPA is served by this daemon, so its requests are same
    /// origin, and the one other case — Vite on another port — proxies `/v1`
    /// (`web/vite.config.ts`), so it does not need CORS either. A daemon that
    /// answered every origin would let any page that learned the token read every
    /// stream, which is a decision to make deliberately.
    pub cors: Option<bool>,
}

/// The `[acp.bridge]` table: the remote driver endpoint.
///
/// Off unless `enabled`, and enabled needs a `bind`: a port is a decision a person
/// makes, not a default this daemon chooses. The cockpit's pairing routes are on
/// the daemon's own listener — this table is the driver-facing one.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct BridgeSettings {
    /// Listen for remote ACP drivers.
    pub enabled: bool,
    /// Address the bridge listener binds, for example `127.0.0.1:7778`.
    pub bind: Option<String>,
    /// Address a driver should use, when it differs from `bind` — a tunnel or a
    /// reverse proxy terminates TLS in front of the daemon (ADR 0004 §3).
    pub public_url: Option<String>,
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
    /// Answer cross-origin requests (`[daemon] cors`).
    pub cors: bool,
    /// The remote driver bridge (`[acp.bridge]`).
    pub bridge: BridgeSettings,
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
    let (first_layers, _) = read(std::slice::from_ref(&user))?;
    let first = effective(first_layers, &flags);
    let (layers, bridge) = match &first.profile {
        Some(directory) => read(&[user, profile_file(directory)])?,
        None => (first, BridgeSettings::default()),
    };
    let settings = effective(layers, &flags);
    if bridge.enabled && bridge.bind.is_none() {
        return Err(ConfigError::Endpoint(
            "`[acp.bridge] enabled = true` needs a `bind` address; nothing listens where it \
             was not told to"
                .to_owned(),
        ));
    }

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
        cors: settings.cors.unwrap_or(false),
        bridge,
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
        cors: layers.cors,
    }
}

/// The merged `[daemon]` tables of `paths`, later files winning.
///
/// A missing file is not an error: every layer is optional by definition, and a
/// machine that never wrote a `config.toml` runs on the defaults.
fn read(paths: &[PathBuf]) -> Result<(Daemon, BridgeSettings), ConfigError> {
    let mut figment = Figment::new();
    for path in paths {
        figment = figment.merge(Toml::file(path));
    }
    let layered: Layered = figment.extract().map_err(|source| ConfigError::File {
        message: source.to_string(),
    })?;
    Ok((layered.daemon, layered.acp.bridge))
}

/// The file a profile keeps its `[daemon]` table in.
fn profile_file(directory: &Path) -> PathBuf {
    directory.join(acpilot_profile::CONFIG_FILE)
}

/// Where a profile's database goes when no layer names one.
///
/// A named profile gets `<data>/profiles/<name>/profile.sqlite`, so two profiles
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
    #[serde(default)]
    acp: Acp,
}

/// The `[acp]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Acp {
    bridge: BridgeSettings,
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

        let (layers, _) = read(&[user, profile]).expect("the cascade");
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
        let (layers, bridge) =
            read(&[directory.path().join("absent.toml")]).expect("no file, no error");
        assert_eq!(layers, Daemon::default());
        assert_eq!(
            bridge,
            BridgeSettings::default(),
            "the bridge is off by default"
        );

        // The bridge table is its own section, and a bind without `enabled` is a
        // route nobody is serving: the resolution refuses the half-decision rather
        // than listening on a port the person did not ask for.
        let path = directory.path().join("bridge.toml");
        write(
            &path,
            "[acp.bridge]\nenabled = true\nbind = \"127.0.0.1:0\"\n",
        );
        let (_, bridge) = read(std::slice::from_ref(&path)).expect("the bridge table");
        assert!(bridge.enabled);
        assert_eq!(bridge.bind.as_deref(), Some("127.0.0.1:0"));
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
        let (layers, _) = read(&[path]).expect("the other tables are ignored");
        assert_eq!(layers.foreground, Some(true));
    }
}
