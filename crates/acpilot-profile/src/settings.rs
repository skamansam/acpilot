//! The settings a launch runs under, from the defaults to the flags.
//!
//! This is the profiles phase's config cascade (P4-2). The layers are, in order:
//! built-in defaults, the global `config.toml`, the profile's files (a parent's
//! beneath a child's when `extends` names one), the workspace's
//! `.acpilot/config.toml`, `ACPILOT_*` variables, and finally the flags a caller
//! typed. Merging is per key: a layer that says nothing about a key leaves the
//! layer below it standing.
//!
//! Two merge rules are not the default one. Arrays replace, except a `disabled` or
//! `deny` array, which unions: a workspace adding one disabled agent must not
//! silently re-enable the ones the profile disabled. `extends` is a layer order
//! rather than a copied value, so a change in the parent is a change in the child
//! on the next read.
//!
//! Every leaf keeps the layer that last wrote it. That is what
//! [`Cascade::explain`] answers, and it is why this uses figment rather than a
//! hand-rolled chain of `Option`s: the value and its origin come from the same
//! provider.
//!
//! Nothing here writes. A project's settings file exists because someone saved
//! settings, never because a client opened a directory — that is what makes
//! "open a file and start typing" cost nothing on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use acpilot_proto::profile::Preferences;
use figment::Figment;
use figment::providers::{Format, Serialized, Toml};
use serde::Deserialize;
use thiserror::Error;

use crate::Profile;
use crate::catalog::{ProfileId, Registry};

/// The settings every launch starts from.
///
/// Data rather than code: adding a preference is a line here plus a field on
/// [`Preferences`], and every UI picks it up without a second set of fallbacks.
pub const DEFAULTS: &str = include_str!("../defaults.toml");

/// How many profiles an `extends` chain may name, the child included.
///
/// A limit is a sentence a person can act on; an unbounded walk is a launch that
/// hangs on a registry someone edited by hand. Four is enough for "personal,
/// work, a client, a one-off" and small enough to read.
pub const MAX_EXTENDS_DEPTH: usize = 4;

/// The file a profile keeps preferences in.
///
/// `config.toml` is still a layer below it: profiles written before this file
/// existed keep working, and a profile can move one key at a time.
pub const PROFILE_SETTINGS_FILE: &str = "profile.toml";

/// The top-level tables and keys this cascade knows.
///
/// A key outside this list and outside `[preferences]` is reported rather than
/// refused: a newer binary's file must still run on an older one. The domain
/// tables are listed even though this cascade does not read them, because their
/// owners do — calling `[policy]` unknown would be a warning on every profile.
const KNOWN_TOP_LEVEL: &[&str] = &[
    "schema_version",
    "profile",
    "preferences",
    "policy",
    "lsp",
    "agents",
    "hosts",
    "daemon",
    "providers",
    "acp",
    "profiles",
    "active_profile",
    "disabled",
    "deny",
    "editor",
    "ui",
    "theme",
    "keymap",
];

/// What a launch resolved to, after merging.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Settings {
    /// The profile this project runs under, when it names one.
    ///
    /// Absent means "whatever the user's file says", and absent from both means the
    /// daemon's own default — so a project that says nothing is not a project that
    /// says `default`.
    #[serde(default)]
    pub profile: Option<String>,

    /// The editor and UI preferences, complete: the defaults guarantee every field.
    pub preferences: Preferences,
}

impl Settings {
    /// Merge the layers that exist, in order: defaults, `user`, `project`.
    ///
    /// This is the file-only entry point every existing caller uses. A launch that
    /// knows its profile uses [`Settings::for_launch`] instead, which adds the
    /// profile files, the environment and the flags.
    ///
    /// # Errors
    ///
    /// A file that does not parse, or a value of the wrong type. The message carries
    /// the file and line figment recorded.
    pub fn load(user: Option<&Path>, project: Option<&Path>) -> Result<Self, SettingsError> {
        Cascade::load(user, project)?.settings()
    }

    /// Resolve the whole cascade for a launch under `profile`.
    ///
    /// `profile` is the directory the launch resolved; `None` is a profile-less
    /// launch, which is the first-run state and reads no profile files.
    ///
    /// # Errors
    ///
    /// A settings file that does not parse, an inheritance chain that loops or is
    /// too deep, or a parent profile that does not open.
    pub fn for_launch(
        user: Option<&Path>,
        project: Option<&Path>,
        profile: Option<&Profile>,
    ) -> Result<Self, SettingsError> {
        Cascade::for_launch(user, project, profile)?.settings()
    }
}

/// Every file a watcher should read for this launch.
///
/// The list follows `extends`: a parent's preferences are a layer of the child's
/// cascade, so editing the parent has to reach a client watching the child. The
/// child is still watched when the inheritance chain cannot be resolved, because
/// a broken parent is a save to report, not a reason to stop watching the file
/// the person is editing.
#[must_use]
pub fn watch_paths(
    user: Option<&Path>,
    project: Option<&Path>,
    profile: Option<&Profile>,
) -> Vec<PathBuf> {
    let mut builder = CascadeBuilder::new().user(user).project(project);
    if let Some(profile) = profile {
        let registry = Registry::load(&crate::user_config_path()).ok();
        builder = builder.profile(profile, registry.as_ref());
    }
    builder.watch_paths()
}

/// A value and the layer that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Explanation {
    /// The dotted key, normalized to `preferences.*` when it had no prefix.
    pub key: String,
    /// The value that won.
    pub value: toml::Value,
    /// The file, environment or flag that wrote it.
    pub origin: String,
}

impl Explanation {
    /// The value as a person would write it in TOML.
    #[must_use]
    pub fn render_value(&self) -> String {
        render(&self.value)
    }
}

/// One key this build does not know, and the file that wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// The dotted key, for example `preferences.tab_widths`.
    pub key: String,
    /// The layer that wrote it.
    pub origin: String,
}

/// The flags layer.
///
/// A flag is `Some` only when it was given, so "not mentioned" stays distinct from
/// "set to the default". Preferences are values rather than strings because the
/// caller has already parsed them; `acpilot config explain --set` is not a
/// feature, and a command line that accepted one would need the same parser twice.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Flags {
    /// `--profile`.
    pub profile: Option<String>,
    /// Preference overrides, keyed by the field name (`tab_width`, not
    /// `preferences.tab_width`).
    pub preferences: BTreeMap<String, toml::Value>,
}

/// A builder for a cascade, because layer order is not the order a caller learns
/// its paths in.
///
/// `new()` reads no process environment. [`CascadeBuilder::process_environment`]
/// is explicit, so a test can own an environment without changing the one every
/// other test shares.
#[derive(Debug, Clone, Default)]
pub struct CascadeBuilder {
    user: Option<PathBuf>,
    project: Option<PathBuf>,
    profile: Option<Profile>,
    registry: Option<Registry>,
    /// The directory named profiles live under; the platform's when absent.
    profiles_root: Option<PathBuf>,
    environment: Option<BTreeMap<String, String>>,
    flags: Flags,
}

impl CascadeBuilder {
    /// An empty builder: the defaults, and nothing else.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The user's `config.toml`.
    #[must_use]
    pub fn user(mut self, path: Option<&Path>) -> Self {
        self.user = path.map(Path::to_path_buf);
        self
    }

    /// The workspace's `.acpilot/config.toml`, when it has one.
    #[must_use]
    pub fn project(mut self, path: Option<&Path>) -> Self {
        self.project = path.map(Path::to_path_buf);
        self
    }

    /// The profile whose files are a layer, and the registry that says what it
    /// extends.
    #[must_use]
    pub fn profile(mut self, profile: &Profile, registry: Option<&Registry>) -> Self {
        self.profile = Some(profile.clone());
        self.registry = registry.cloned();
        self
    }

    /// Where a named parent profile is resolved.
    ///
    /// The platform's `<config>/profiles` unless a test or a portable install
    /// owns another root. `extends` names a profile, not a path, so this is the
    /// one place the name becomes a directory.
    #[must_use]
    pub fn profiles_root(mut self, root: Option<&Path>) -> Self {
        self.profiles_root = root.map(Path::to_path_buf);
        self
    }

    /// Read `ACPILOT_*` variables as a layer.
    ///
    /// Only `ACPILOT_PROFILE` and variables naming a known preference are read;
    /// the endpoint, token and home variables belong to other layers and must not
    /// become settings keys by accident.
    #[must_use]
    pub fn process_environment(mut self) -> Self {
        let vars = std::env::vars_os()
            .filter_map(|(key, value)| {
                let key = key.into_string().ok()?;
                let value = value.into_string().ok()?;
                key.starts_with("ACPILOT_").then_some((key, value))
            })
            .collect();
        self.environment = Some(vars);
        self
    }

    /// An explicit environment, for a test or a caller that owns one.
    #[must_use]
    pub fn environment(mut self, vars: impl IntoIterator<Item = (String, String)>) -> Self {
        self.environment = Some(vars.into_iter().collect());
        self
    }

    /// The flags a caller typed.
    #[must_use]
    pub fn flags(mut self, flags: Flags) -> Self {
        self.flags = flags;
        self
    }

    /// Read every layer and produce the cascade.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or parsed, a profile parent that does not open,
    /// or an inheritance chain that loops or is too deep.
    pub fn build(self) -> Result<Cascade, SettingsError> {
        let mut figment = Figment::from(Toml::string(DEFAULTS));
        let mut layers: Vec<(String, toml::Value)> = Vec::new();
        let defaults: toml::Value =
            toml::from_str(DEFAULTS).map_err(|error| SettingsError::Invalid {
                message: format!("the built-in defaults are not valid TOML: {error}"),
            })?;
        layers.push(("built-in defaults".to_owned(), defaults.clone()));

        let mut files: Vec<PathBuf> = Vec::new();
        if let Some(user) = &self.user {
            files.push(user.clone());
        }
        for profile in self.profile_chain()? {
            files.push(profile.config_path());
            files.push(profile.preferences_path());
        }
        if let Some(project) = &self.project {
            files.push(project.clone());
        }

        for path in files {
            if !path.is_file() {
                continue;
            }
            let text = fs::read_to_string(&path).map_err(|source| SettingsError::Read {
                path: path.clone(),
                source,
            })?;
            let value: toml::Value =
                toml::from_str(&text).map_err(|error| SettingsError::Toml {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
            let origin = path.display().to_string();
            figment = figment.merge(Toml::string(&text));
            layers.push((origin, value));
        }

        let known = preferences_in(&defaults);
        if let Some(environment) = &self.environment
            && let Some(table) = environment_layer(environment, &known)
        {
            let origin = "ACPILOT_* environment".to_owned();
            figment = figment.merge(Serialized::defaults(table.clone()));
            layers.push((origin, table));
        }
        if let Some(table) = flags_layer(&self.flags) {
            let origin = "command-line flags".to_owned();
            figment = figment.merge(Serialized::defaults(table.clone()));
            layers.push((origin, table));
        }

        let mut raw: toml::Value = figment.extract().map_err(|error| SettingsError::Invalid {
            message: error.to_string(),
        })?;
        let unions = apply_unions(&mut raw, &layers);
        let origins = origins_of(&layers);
        let warnings = unknown_keys(&raw, &known, &origins);
        warn_once(&warnings);
        Ok(Cascade {
            figment,
            raw,
            origins,
            unions,
            warnings,
        })
    }

    /// The files this builder's layers read, in cascade order.
    #[must_use]
    pub fn watch_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(user) = &self.user {
            paths.push(user.clone());
        }
        let chain = self
            .profile_chain()
            .unwrap_or_else(|_| self.profile.iter().cloned().collect());
        for profile in chain {
            paths.push(profile.config_path());
            paths.push(profile.preferences_path());
        }
        if let Some(project) = &self.project {
            paths.push(project.clone());
        }
        let mut unique = Vec::new();
        for path in paths {
            if !unique.contains(&path) {
                unique.push(path);
            }
        }
        unique
    }

    /// The profile chain, parent first, with the child last.
    fn profile_chain(&self) -> Result<Vec<Profile>, SettingsError> {
        let Some(child) = &self.profile else {
            return Ok(Vec::new());
        };
        let mut chain = vec![child.clone()];
        let mut seen: BTreeSet<ProfileId> = BTreeSet::new();
        if let Some(id) = child.id() {
            seen.insert(id);
        }
        while let Some(id) = chain.last().and_then(Profile::id) {
            let Some(parent) = self
                .registry
                .as_ref()
                .and_then(|registry| registry.profile(&id))
                .and_then(|record| record.extends.clone())
            else {
                break;
            };
            if chain.len() >= MAX_EXTENDS_DEPTH {
                return Err(SettingsError::Inheritance {
                    message: format!(
                        "profile `{id}` is deeper than {MAX_EXTENDS_DEPTH} profiles; shorten the \
                         `extends` chain"
                    ),
                });
            }
            if !seen.insert(parent.clone()) {
                let mut names: Vec<String> = chain
                    .iter()
                    .filter_map(Profile::id)
                    .map(|id| id.as_str().to_owned())
                    .collect();
                names.push(parent.as_str().to_owned());
                return Err(SettingsError::Inheritance {
                    message: format!("a profile extends itself: {}", names.join(" -> ")),
                });
            }
            let root = self
                .profiles_root
                .clone()
                .unwrap_or_else(crate::profiles_dir);
            let directory = root.join(parent.as_str());
            let profile =
                Profile::open(&directory).map_err(|error| SettingsError::Inheritance {
                    message: format!(
                        "profile `{id}` extends `{parent}`, which does not open: {error}"
                    ),
                })?;
            chain.push(profile);
        }
        chain.reverse();
        Ok(chain)
    }
}

/// A resolved cascade: the merged values and what wrote each one.
#[derive(Debug, Clone)]
pub struct Cascade {
    figment: Figment,
    raw: toml::Value,
    origins: BTreeMap<String, Vec<String>>,
    unions: BTreeSet<String>,
    warnings: Vec<Warning>,
}

impl Cascade {
    /// Read the three file layers, without the environment or the profile files.
    ///
    /// # Errors
    ///
    /// A file that does not parse, or a value of the wrong type.
    pub fn load(user: Option<&Path>, project: Option<&Path>) -> Result<Self, SettingsError> {
        CascadeBuilder::new().user(user).project(project).build()
    }

    /// Read every layer for a launch under `profile`.
    ///
    /// The registry is read from the global config root because `extends` lives
    /// there; a registry that does not parse is an error rather than an ignored
    /// parent, since the child would otherwise run on a subset of its settings.
    ///
    /// # Errors
    ///
    /// A file that does not parse, a registry that does not parse, or a profile
    /// chain that loops or is too deep.
    pub fn for_launch(
        user: Option<&Path>,
        project: Option<&Path>,
        profile: Option<&Profile>,
    ) -> Result<Self, SettingsError> {
        let mut builder = CascadeBuilder::new()
            .user(user)
            .project(project)
            .process_environment();
        // A profile-less launch has nothing to inherit, so it must not fail on a
        // registry it never reads. The registry is what says what a profile
        // extends, and only a launch that has one asks that question.
        if let Some(profile) = profile {
            let registry = Registry::load(&crate::user_config_path()).map_err(|source| {
                SettingsError::Registry {
                    message: source.to_string(),
                }
            })?;
            builder = builder.profile(profile, Some(&registry));
        }
        builder.build()
    }

    /// The typed settings this cascade produces.
    ///
    /// # Errors
    ///
    /// A merged value that does not fit [`Settings`]. The message names the key
    /// and the file figment recorded.
    pub fn settings(&self) -> Result<Settings, SettingsError> {
        self.figment
            .extract()
            .map_err(|error| SettingsError::Invalid {
                message: error.to_string(),
            })
    }

    /// The keys this build does not know.
    #[must_use]
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// The winner for `key`, and the layer that wrote it.
    ///
    /// A key without a dot is read as a preference, so `tab_width` and
    /// `preferences.tab_width` name the same value.
    ///
    /// # Errors
    ///
    /// The key is not in the merged settings.
    pub fn explain(&self, key: &str) -> Result<Explanation, SettingsError> {
        let key = normalize_key(key);
        let Some(value) = lookup(&self.raw, &key) else {
            return Err(SettingsError::UnknownKey { key });
        };
        let origins = self.origins.get(&key).cloned().unwrap_or_default();
        let origin = if self.unions.contains(&key) {
            let mut unique = Vec::new();
            for origin in origins {
                if !unique.contains(&origin) {
                    unique.push(origin);
                }
            }
            unique.join(" + ")
        } else {
            origins
                .last()
                .cloned()
                .unwrap_or_else(|| "built-in defaults".to_owned())
        };
        Ok(Explanation {
            key,
            value: value.clone(),
            origin,
        })
    }

    /// The merged value at `key`, when it exists.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<&toml::Value> {
        lookup(&self.raw, &normalize_key(key))
    }
}

fn normalize_key(key: &str) -> String {
    let key = key.trim();
    if key == "profile" || key.contains('.') {
        key.to_owned()
    } else {
        format!("preferences.{key}")
    }
}

/// The preference field names the defaults declare.
fn preferences_in(defaults: &toml::Value) -> BTreeSet<String> {
    let mut known: BTreeSet<String> = defaults
        .get("preferences")
        .and_then(toml::Value::as_table)
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default();
    // The two union keys are part of the schema even though `Preferences` does
    // not model them yet: a file that sets one is not a typo, and warning about
    // it would train people to ignore the warning that matters.
    known.insert("disabled".to_owned());
    known.insert("deny".to_owned());
    known
}

/// Build the environment layer from the variables that name a setting.
fn environment_layer(
    vars: &BTreeMap<String, String>,
    known: &BTreeSet<String>,
) -> Option<toml::Value> {
    let mut preferences = toml::Table::new();
    let mut table = toml::Table::new();
    for (name, text) in vars {
        let Some(rest) = name.strip_prefix("ACPILOT_") else {
            continue;
        };
        if rest == "PROFILE" {
            table.insert("profile".to_owned(), toml::Value::String(text.clone()));
            continue;
        }
        let rest = rest
            .strip_prefix("PREFERENCES__")
            .or_else(|| rest.strip_prefix("PREFERENCES_"))
            .unwrap_or(rest)
            .to_ascii_lowercase();
        if !known.contains(&rest) {
            // `ACPILOT_API_URL`, `ACPILOT_TOKEN` and friends are other layers.
            continue;
        }
        preferences.insert(rest, parse_environment_value(text));
    }
    if !preferences.is_empty() {
        table.insert("preferences".to_owned(), toml::Value::Table(preferences));
    }
    (!table.is_empty()).then_some(toml::Value::Table(table))
}

/// A TOML scalar when the text is one, otherwise the text as a string.
fn parse_environment_value(text: &str) -> toml::Value {
    toml::from_str::<toml::Value>(text)
        .ok()
        .filter(|value| !value.is_table() && !value.is_array())
        .unwrap_or_else(|| toml::Value::String(text.to_owned()))
}

/// Build the flags layer.
fn flags_layer(flags: &Flags) -> Option<toml::Value> {
    let mut table = toml::Table::new();
    if let Some(profile) = &flags.profile {
        table.insert("profile".to_owned(), toml::Value::String(profile.clone()));
    }
    if !flags.preferences.is_empty() {
        table.insert(
            "preferences".to_owned(),
            toml::Value::Table(flags.preferences.clone().into_iter().collect()),
        );
    }
    (!table.is_empty()).then_some(toml::Value::Table(table))
}

/// Union every `disabled` and `deny` array, in layer order.
///
/// The return is the set of paths that were unioned rather than replaced;
/// [`Cascade::explain`] uses it to name every layer that contributed instead of
/// only the last one.
fn apply_unions(raw: &mut toml::Value, layers: &[(String, toml::Value)]) -> BTreeSet<String> {
    let mut unions: BTreeMap<String, Vec<toml::Value>> = BTreeMap::new();
    for (_, layer) in layers {
        collect_unions(layer, "", &mut unions);
    }
    for (path, values) in &unions {
        let mut merged = Vec::new();
        for value in values {
            if !merged.contains(value) {
                merged.push(value.clone());
            }
        }
        set_path(raw, path, toml::Value::Array(merged));
    }
    unions.into_keys().collect()
}

fn collect_unions(
    value: &toml::Value,
    prefix: &str,
    unions: &mut BTreeMap<String, Vec<toml::Value>>,
) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if matches!(key.as_str(), "disabled" | "deny")
            && let Some(array) = value.as_array()
        {
            unions
                .entry(path)
                .or_default()
                .extend(array.iter().cloned());
            continue;
        }
        collect_unions(value, &path, unions);
    }
}

/// Which layer last wrote each leaf.
fn origins_of(layers: &[(String, toml::Value)]) -> BTreeMap<String, Vec<String>> {
    let mut origins: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (origin, layer) in layers {
        collect_origins(layer, "", origin, &mut origins);
    }
    origins
}

fn collect_origins(
    value: &toml::Value,
    prefix: &str,
    origin: &str,
    origins: &mut BTreeMap<String, Vec<String>>,
) {
    let Some(table) = value.as_table() else {
        if !prefix.is_empty() {
            let entry = origins.entry(prefix.to_owned()).or_default();
            if !entry.iter().any(|seen| seen == origin) {
                entry.push(origin.to_owned());
            }
        }
        return;
    };
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if !prefix.is_empty() {
            let entry = origins.entry(prefix.to_owned()).or_default();
            if !entry.iter().any(|seen| seen == origin) {
                entry.push(origin.to_owned());
            }
        }
        collect_origins(value, &path, origin, origins);
    }
}

/// Keys outside the known shape, so an older binary says what it ignored.
fn unknown_keys(
    raw: &toml::Value,
    known_preferences: &BTreeSet<String>,
    origins: &BTreeMap<String, Vec<String>>,
) -> Vec<Warning> {
    let mut warnings = Vec::new();
    let Some(table) = raw.as_table() else {
        return warnings;
    };
    for (key, value) in table {
        if !KNOWN_TOP_LEVEL.contains(&key.as_str()) {
            warnings.push(warning(key, origins));
            continue;
        }
        if key != "preferences" {
            continue;
        }
        let Some(preferences) = value.as_table() else {
            continue;
        };
        for field in preferences.keys() {
            if !known_preferences.contains(field) {
                warnings.push(warning(&format!("preferences.{field}"), origins));
            }
        }
    }
    warnings
}

fn warning(key: &str, origins: &BTreeMap<String, Vec<String>>) -> Warning {
    Warning {
        key: key.to_owned(),
        origin: origins
            .get(key)
            .and_then(|origins| origins.last())
            .cloned()
            .unwrap_or_else(|| "an unknown layer".to_owned()),
    }
}

/// Print each unknown key once per process.
///
/// The watcher re-reads the cascade on every save; a warning per read would turn
/// one typo into a stream of them. A newer file with a key this build added later
/// still runs, which is the reason the warning is not an error.
fn warn_once(warnings: &[Warning]) {
    static SEEN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for warning in warnings {
        if seen.insert(warning.key.clone()) {
            eprintln!(
                "warning: unknown setting `{}` in {}; this build ignores it",
                warning.key, warning.origin
            );
        }
    }
}

fn lookup<'a>(value: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut current = value;
    for part in key.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

fn set_path(root: &mut toml::Value, path: &str, value: toml::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    set_parts(root, &parts, value);
}

fn set_parts(root: &mut toml::Value, parts: &[&str], value: toml::Value) {
    let Some((first, rest)) = parts.split_first() else {
        *root = value;
        return;
    };
    let Some(table) = root.as_table_mut() else {
        return;
    };
    if rest.is_empty() {
        table.insert((*first).to_owned(), value);
        return;
    }
    let child = table
        .entry((*first).to_owned())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    set_parts(child, rest, value);
}

fn render(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => format!("{text:?}"),
        toml::Value::Integer(number) => number.to_string(),
        toml::Value::Float(number) => number.to_string(),
        toml::Value::Boolean(boolean) => boolean.to_string(),
        toml::Value::Datetime(datetime) => datetime.to_string(),
        toml::Value::Array(values) => {
            let values: Vec<String> = values.iter().map(render).collect();
            format!("[{}]", values.join(", "))
        }
        toml::Value::Table(table) => {
            let values: Vec<String> = table
                .iter()
                .map(|(key, value)| format!("{key} = {}", render(value)))
                .collect();
            format!("{{{}}}", values.join(", "))
        }
    }
}

/// Why a cascade could not be produced.
#[derive(Debug, Error)]
pub enum SettingsError {
    /// A settings file exists and does not hold what it claims.
    #[error("cannot read settings: {message}")]
    Invalid {
        /// figment's message, which names the file and line.
        message: String,
    },

    /// A settings file exists and is not TOML.
    #[error("{} is not valid TOML: {message}", path.display())]
    Toml {
        /// The file that failed.
        path: PathBuf,
        /// TOML's message, which names the line and column.
        message: String,
    },

    /// A settings file exists and could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file that failed.
        path: PathBuf,
        /// The read failure.
        #[source]
        source: io::Error,
    },

    /// The global registry, which says what a profile extends, is not usable.
    #[error("cannot read profile inheritance: {message}")]
    Registry {
        /// The registry error's message.
        message: String,
    },

    /// An `extends` chain loops, is too deep, or names a profile that does not
    /// open.
    #[error("cannot resolve profile inheritance: {message}")]
    Inheritance {
        /// Which profile and which parent.
        message: String,
    },

    /// `acpilot config explain` asked for a key no layer set.
    #[error("no setting named `{key}`")]
    UnknownKey {
        /// The normalized key.
        key: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_proto::profile::ThemeSurface;
    use std::path::PathBuf;

    fn write(directory: &Path, name: &str, text: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, text).expect("settings file");
        path
    }

    fn profile(directory: &Path, name: &str, config: &str, preferences: &str) -> Profile {
        let root = directory.join(name);
        fs::create_dir_all(&root).expect("profile directory");
        fs::write(root.join(crate::CONFIG_FILE), config).expect("profile config");
        fs::write(root.join(PROFILE_SETTINGS_FILE), preferences).expect("profile preferences");
        Profile::open(root).expect("the profile opens")
    }

    #[test]
    fn with_no_files_every_setting_is_the_default() {
        let directory = tempfile::tempdir().expect("temp dir");
        let settings = Settings::load(
            Some(&directory.path().join("absent.toml")),
            Some(&directory.path().join("also-absent.toml")),
        )
        .expect("defaults");
        assert_eq!(settings.preferences.tab_width, 4);
        assert_eq!(settings.preferences.theme, "acpilot.night-flight");
        assert!(settings.profile.is_none());
    }

    #[test]
    fn the_user_file_overrides_a_default_one_key_at_a_time() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "user.toml",
            "[preferences]\ntab_width = 2\n",
        );
        let settings = Settings::load(Some(&user), None).expect("merged");
        assert_eq!(settings.preferences.tab_width, 2);
        assert_eq!(
            settings.preferences.theme, "acpilot.night-flight",
            "a key the user did not set keeps its default"
        );
    }

    #[test]
    fn the_project_file_overrides_the_user_file() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "user.toml",
            "[preferences]\ntab_width = 2\ntheme = \"acpilot.night-flight\"\n",
        );
        let project = write(
            directory.path(),
            "project.toml",
            "profile = \"work\"\n\n[preferences]\ntab_width = 8\n",
        );
        let settings = Settings::load(Some(&user), Some(&project)).expect("merged");
        assert_eq!(settings.preferences.tab_width, 8, "the project wins");
        assert_eq!(
            settings.preferences.theme, "acpilot.night-flight",
            "the user's value survives where the project says nothing"
        );
        assert_eq!(settings.profile.as_deref(), Some("work"));
    }

    #[test]
    fn a_project_that_says_nothing_about_a_profile_does_not_invent_one() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "user.toml",
            "[preferences]\nwrap = true\n",
        );
        let project = write(directory.path(), "project.toml", "schema_version = 1\n");
        let settings = Settings::load(Some(&user), Some(&project)).expect("merged");
        assert!(settings.profile.is_none());
        assert!(settings.preferences.wrap);
    }

    #[test]
    fn a_surface_that_names_its_own_theme_keeps_it() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "user.toml",
            "[preferences]\ntheme = \"acpilot.paper\"\ntheme_tui = \"acpilot.night-flight\"\n",
        );
        let settings = Settings::load(Some(&user), None).expect("merged");
        assert_eq!(
            settings.preferences.theme_for(ThemeSurface::Tui),
            "acpilot.night-flight",
            "the terminal's own key wins for the terminal"
        );
        assert_eq!(
            settings.preferences.theme_for(ThemeSurface::Gui),
            "acpilot.paper",
            "and the window falls back to the shared one"
        );
        assert_eq!(
            settings.preferences.theme_for(ThemeSurface::Web),
            "acpilot.paper"
        );
    }

    #[test]
    fn a_project_can_theme_one_surface_without_touching_the_others() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "user.toml",
            "[preferences]\ntheme = \"acpilot.night-flight\"\n",
        );
        let project = write(
            directory.path(),
            "project.toml",
            "[preferences]\ntheme_web = \"acpilot.paper\"\n",
        );
        let settings = Settings::load(Some(&user), Some(&project)).expect("merged");
        assert_eq!(
            settings.preferences.theme_for(ThemeSurface::Web),
            "acpilot.paper",
            "the project overrides the browser's theme for that project"
        );
        assert_eq!(
            settings.preferences.theme_for(ThemeSurface::Tui),
            "acpilot.night-flight",
            "and the terminal, which it said nothing about, keeps the user's"
        );
        assert_eq!(
            settings.preferences.theme_for(ThemeSurface::Gui),
            "acpilot.night-flight"
        );
    }

    #[test]
    fn following_the_terminal_is_off_unless_someone_turns_it_on() {
        let defaults = Settings::load(None, None).expect("defaults");
        assert!(
            !defaults.preferences.theme_follow_terminal,
            "a terminal's colours are not taken over unless asked"
        );

        let directory = tempfile::tempdir().expect("temp dir");
        let project = write(
            directory.path(),
            "project.toml",
            "[preferences]\ntheme_follow_terminal = true\n",
        );
        let settings = Settings::load(None, Some(&project)).expect("merged");
        assert!(
            settings.preferences.theme_follow_terminal,
            "the project turned it on"
        );
        assert_eq!(
            settings.preferences.theme, "acpilot.night-flight",
            "and the named theme is still there to fall back to"
        );
    }

    #[test]
    fn a_broken_file_names_itself_rather_than_running_on_a_subset() {
        let directory = tempfile::tempdir().expect("temp dir");
        let project = write(
            directory.path(),
            "project.toml",
            "[preferences\ntab_width = 2\n",
        );
        let error = Settings::load(None, Some(&project)).expect_err("invalid TOML");
        assert!(error.to_string().contains("project.toml"), "{error}");
    }

    #[test]
    fn the_profile_layer_sits_above_the_global_file_and_below_the_project() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "config.toml",
            "[preferences]\ntab_width = 2\ntheme = \"acpilot.paper\"\n",
        );
        let work = profile(
            directory.path(),
            "work",
            "schema_version = 1\n",
            "[preferences]\ntab_width = 3\n",
        );
        let project = write(
            directory.path(),
            "project.toml",
            "[preferences]\ntab_width = 5\n",
        );
        let cascade = CascadeBuilder::new()
            .user(Some(&user))
            .project(Some(&project))
            .profile(&work, None)
            .build()
            .expect("cascade");
        let settings = cascade.settings().expect("settings");
        assert_eq!(
            settings.preferences.tab_width, 5,
            "the project is above the profile"
        );
        assert_eq!(
            settings.preferences.theme, "acpilot.paper",
            "the global file survives where the profile says nothing"
        );
    }

    #[test]
    fn environment_and_flags_are_the_last_two_layers() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "config.toml",
            "[preferences]\ntheme = \"acpilot.paper\"\ntab_width = 2\n",
        );
        let project = write(
            directory.path(),
            "project.toml",
            "[preferences]\ntheme = \"acpilot.project\"\n",
        );
        let environment = BTreeMap::from([
            (
                "ACPILOT_PREFERENCES__THEME".to_owned(),
                "acpilot.environment".to_owned(),
            ),
            ("ACPILOT_API_URL".to_owned(), "http://ignored".to_owned()),
        ]);
        let flags = Flags {
            preferences: BTreeMap::from([(
                "theme".to_owned(),
                toml::Value::String("acpilot.flags".to_owned()),
            )]),
            ..Flags::default()
        };
        let cascade = CascadeBuilder::new()
            .user(Some(&user))
            .project(Some(&project))
            .environment(environment)
            .flags(flags)
            .build()
            .expect("cascade");
        let settings = cascade.settings().expect("settings");
        assert_eq!(
            settings.preferences.theme, "acpilot.flags",
            "flags are above the environment"
        );
        assert_eq!(
            settings.preferences.tab_width, 2,
            "an unrelated environment variable is not a setting"
        );
        assert_eq!(
            cascade.explain("theme").expect("the winner").origin,
            "command-line flags"
        );
    }

    #[test]
    fn an_extends_parent_is_read_beneath_the_child() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(directory.path(), "config.toml", "schema_version = 1\n");
        let parent = profile(
            directory.path(),
            "base",
            "schema_version = 1\n",
            "[preferences]\ntheme = \"acpilot.paper\"\n",
        );
        let child = profile(
            directory.path(),
            "work",
            "schema_version = 1\n",
            "[preferences]\ntab_width = 2\n",
        );
        let mut registry = Registry::default();
        registry.insert(
            parent.id().expect("a parent id"),
            crate::ProfileRecord::new("Base"),
        );
        registry.insert(
            child.id().expect("a child id"),
            crate::ProfileRecord {
                extends: parent.id(),
                ..crate::ProfileRecord::new("Work")
            },
        );
        let cascade = CascadeBuilder::new()
            .user(Some(&user))
            .profile(&child, Some(&registry))
            .profiles_root(Some(directory.path()))
            .build()
            .expect("cascade");
        let settings = cascade.settings().expect("settings");
        assert_eq!(settings.preferences.tab_width, 2, "the child's key");
        assert_eq!(
            settings.preferences.theme, "acpilot.paper",
            "the parent's key is inherited"
        );
    }

    #[test]
    fn the_watcher_follows_an_extends_chain() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(directory.path(), "config.toml", "schema_version = 1\n");
        let parent = profile(
            directory.path(),
            "base",
            "schema_version = 1\n",
            "[preferences]\ntheme = \"acpilot.paper\"\n",
        );
        let child = profile(
            directory.path(),
            "work",
            "schema_version = 1\n",
            "[preferences]\ntab_width = 2\n",
        );
        let mut registry = Registry::default();
        registry.insert(
            parent.id().expect("a parent id"),
            crate::ProfileRecord::new("Base"),
        );
        registry.insert(
            child.id().expect("a child id"),
            crate::ProfileRecord {
                extends: parent.id(),
                ..crate::ProfileRecord::new("Work")
            },
        );
        let paths = CascadeBuilder::new()
            .user(Some(&user))
            .profile(&child, Some(&registry))
            .profiles_root(Some(directory.path()))
            .watch_paths();
        assert!(paths.contains(&child.preferences_path()));
        assert!(
            paths.contains(&parent.preferences_path()),
            "a parent preference reaches the child, so the parent file is watched"
        );
    }

    #[test]
    fn a_cycle_and_a_chain_that_is_too_deep_are_errors() {
        let directory = tempfile::tempdir().expect("temp dir");
        let base = profile(
            directory.path(),
            "base",
            "schema_version = 1\n",
            "[preferences]\nwrap = true\n",
        );
        let mut registry = Registry::default();
        registry.insert(
            base.id().expect("id"),
            crate::ProfileRecord {
                extends: base.id(),
                ..crate::ProfileRecord::new("Base")
            },
        );
        let error = CascadeBuilder::new()
            .profile(&base, Some(&registry))
            .profiles_root(Some(directory.path()))
            .build()
            .expect_err("a self-extension is a cycle");
        assert!(error.to_string().contains("extends itself"), "{error}");

        // Five profiles, each extending the one before: the fifth is one too
        // many. Two-character names keep every one a valid profile id.
        let mut deep = Registry::default();
        let mut child = None;
        for index in 0..5 {
            let id = format!("p{index}");
            let current = profile(
                directory.path(),
                &id,
                "schema_version = 1\n",
                "[preferences]\nwrap = true\n",
            );
            let parent = child.take();
            deep.insert(
                current.id().expect("id"),
                crate::ProfileRecord {
                    extends: parent,
                    ..crate::ProfileRecord::new(&id)
                },
            );
            child = current.id();
            if index == 4 {
                let error = CascadeBuilder::new()
                    .profile(&current, Some(&deep))
                    .profiles_root(Some(directory.path()))
                    .build()
                    .expect_err("a fifth profile is too deep");
                assert!(error.to_string().contains("deeper than"), "{error}");
            }
        }
    }

    #[test]
    fn disabled_and_deny_arrays_union_instead_of_replacing() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "config.toml",
            "[preferences]\ndisabled = [\"one\"]\ndeny = [\"read\"]\n",
        );
        let project = write(
            directory.path(),
            "project.toml",
            "[preferences]\ndisabled = [\"two\"]\ndeny = [\"write\"]\n",
        );
        let cascade = Cascade::load(Some(&user), Some(&project)).expect("cascade");
        let disabled = cascade
            .value("preferences.disabled")
            .and_then(toml::Value::as_array)
            .expect("the union");
        assert_eq!(disabled.len(), 2);
        assert!(disabled.contains(&toml::Value::String("one".to_owned())));
        assert!(disabled.contains(&toml::Value::String("two".to_owned())));
        assert!(
            cascade
                .explain("preferences.disabled")
                .expect("explained")
                .origin
                .contains("project.toml"),
            "both layers are named"
        );
    }

    #[test]
    fn an_unknown_key_warns_with_its_file_and_does_not_fail_the_read() {
        let directory = tempfile::tempdir().expect("temp dir");
        let project = write(
            directory.path(),
            "project.toml",
            "[preferences]\ntab_widths = 2\n",
        );
        let cascade = Cascade::load(None, Some(&project)).expect("the read still works");
        assert_eq!(cascade.warnings().len(), 1);
        assert_eq!(cascade.warnings()[0].key, "preferences.tab_widths");
        assert!(cascade.warnings()[0].origin.contains("project.toml"));
    }

    #[test]
    fn explain_names_the_project_file_as_the_winner() {
        let directory = tempfile::tempdir().expect("temp dir");
        let user = write(
            directory.path(),
            "config.toml",
            "[preferences]\ntab_width = 2\n",
        );
        let project = write(
            directory.path(),
            "project.toml",
            "[preferences]\ntab_width = 8\n",
        );
        let cascade = Cascade::load(Some(&user), Some(&project)).expect("cascade");
        let explanation = cascade.explain("tab_width").expect("the winner");
        assert_eq!(explanation.key, "preferences.tab_width");
        assert_eq!(explanation.render_value(), "8");
        assert!(explanation.origin.contains("project.toml"));
    }
}
