//! Which profile a launch opens, in one place.
//!
//! The order is fixed and deliberate:
//!
//! 1. `--profile`
//! 2. `ACPILOT_PROFILE`
//! 3. the project's `profile` key in `.acpilot/config.toml`
//! 4. `active_profile` in the global registry
//! 5. `default`
//!
//! The first four are names a person or a project chose; `default` is the only
//! fallback, and it is created on first use so a fresh install has somewhere to
//! write. A name that does not resolve is an error, never a quieter fallback:
//! guessing here means sending a work prompt with a personal key.
//!
//! A `--profile` argument may still be a directory. A path-shaped argument
//! (absolute, or more than one segment) opens that directory, which is what
//! existing scripts and the desktop app's repeatable flag already do. A single
//! segment is a profile name.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::catalog::{ProfileId, ProfileIdError, Registry, RegistryError};
use crate::project::Root;
use crate::settings::SettingsError;
use crate::{CONFIG_FILE, Profile, ProfileError};

/// The profile a launch uses when nothing names one.
pub const DEFAULT_PROFILE: &str = "default";

/// The settings a first-run profile needs to open.
const DEFAULT_CONFIG: &str = "schema_version = 1\n";

/// Where a profile name came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileSource {
    /// `--profile`.
    Flag,
    /// A path-shaped `--profile`, which is a directory rather than a name.
    FlagPath(PathBuf),
    /// `ACPILOT_PROFILE`.
    Environment,
    /// The project's `.acpilot/config.toml`.
    Project(PathBuf),
    /// `active_profile` in the global registry.
    Active(PathBuf),
    /// The built-in fallback, created on first use.
    Default,
}

impl ProfileSource {
    /// Whether this is the automatic fallback rather than something named.
    #[must_use]
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }
}

impl std::fmt::Display for ProfileSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flag => formatter.write_str("--profile"),
            Self::FlagPath(path) => write!(formatter, "--profile {}", path.display()),
            Self::Environment => formatter.write_str("ACPILOT_PROFILE"),
            Self::Project(path) => formatter.write_str(&path.display().to_string()),
            Self::Active(path) => {
                write!(formatter, "active_profile in {}", path.display())
            }
            Self::Default => formatter.write_str("the built-in default"),
        }
    }
}

/// A profile and the layer that chose it.
#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    profile: Profile,
    source: ProfileSource,
}

impl ResolvedProfile {
    /// The opened profile directory.
    #[must_use]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Where the choice came from.
    #[must_use]
    pub fn source(&self) -> &ProfileSource {
        &self.source
    }

    /// Whether this is the automatic `default` fallback.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.source.is_default()
    }
}

/// The layers that resolve a profile, with their paths made explicit so a test
/// can own them without changing the process environment.
#[derive(Debug, Clone)]
pub struct ProfileResolver {
    profiles: PathBuf,
    user_config: Option<PathBuf>,
    flag: Option<PathBuf>,
    environment: Option<String>,
    create_default: bool,
}

impl ProfileResolver {
    /// A resolver over this machine's config root.
    #[must_use]
    pub fn new() -> Self {
        Self::new_in(
            crate::profiles_dir(),
            Some(crate::user_config_path().as_path()),
        )
    }

    /// A resolver over an explicit profiles directory and global registry.
    ///
    /// `user_config` is `None` for a caller that owns no global file — a test, or
    /// a portable install with only a profile root. It still gets the flag,
    /// environment, marker and default layers.
    #[must_use]
    pub fn new_in(profiles: impl Into<PathBuf>, user_config: Option<&Path>) -> Self {
        Self {
            profiles: profiles.into(),
            user_config: user_config.map(Path::to_path_buf),
            flag: None,
            environment: None,
            create_default: true,
        }
    }

    /// The `--profile` argument, when it was given.
    #[must_use]
    pub fn flag(mut self, flag: Option<&Path>) -> Self {
        self.flag = flag.map(Path::to_path_buf);
        self
    }

    /// `ACPILOT_PROFILE`, when it is set to a non-empty value.
    #[must_use]
    pub fn environment(mut self, value: Option<&str>) -> Self {
        self.environment = value.filter(|value| !value.is_empty()).map(str::to_owned);
        self
    }

    /// Read `ACPILOT_PROFILE` from this process.
    #[must_use]
    pub fn process_environment(self) -> Self {
        let value = std::env::var("ACPILOT_PROFILE").ok();
        self.environment(value.as_deref())
    }

    /// Whether a missing `default` is created. On by default; `doctor` and other
    /// read-only callers can turn it off.
    #[must_use]
    pub fn create_default(mut self, create: bool) -> Self {
        self.create_default = create;
        self
    }

    /// The profiles directory this resolver addresses.
    #[must_use]
    pub fn profiles_root(&self) -> &Path {
        &self.profiles
    }

    /// Resolve `root`'s project marker and every layer around it.
    ///
    /// The project is read with the file-only cascade: the environment is a
    /// resolution layer here, not a settings file, and letting the settings
    /// cascade read it first would make `root.settings().profile` look like the
    /// marker when it was `ACPILOT_PROFILE`.
    ///
    /// # Errors
    ///
    /// A settings file or registry that does not parse, a name that is not a
    /// profile id, a name that does not resolve, or a `default` that could not be
    /// created.
    pub fn resolve(&self, root: &Root) -> Result<ResolvedProfile, ProfileResolveError> {
        if let Some(flag) = &self.flag {
            if is_path_argument(flag) {
                let profile = Profile::open(flag)?;
                return Ok(ResolvedProfile {
                    profile,
                    source: ProfileSource::FlagPath(flag.clone()),
                });
            }
            return self.named(flag.to_str().unwrap_or_default(), ProfileSource::Flag);
        }

        if let Some(name) = &self.environment {
            return self.named(name, ProfileSource::Environment);
        }

        let marker = if root.project().is_some() {
            root.settings_with(None)?.profile
        } else {
            None
        };
        if let Some(name) = marker {
            let config = root.project().map_or_else(
                || root.root().to_path_buf(),
                |project| project.config_path(),
            );
            return self.named(&name, ProfileSource::Project(config));
        }

        if let Some(config) = &self.user_config {
            let registry = Registry::load(config)?;
            if let Some(id) = registry.active_profile {
                return self.named(id.as_str(), ProfileSource::Active(config.clone()));
            }
        }

        self.default_profile()
    }

    /// Open a name under the profiles root.
    fn named(
        &self,
        name: &str,
        source: ProfileSource,
    ) -> Result<ResolvedProfile, ProfileResolveError> {
        let id = ProfileId::parse(name).map_err(|error| ProfileResolveError::InvalidName {
            name: name.to_owned(),
            source: source.clone(),
            cause: error,
        })?;
        let directory = self.profiles.join(id.as_str());
        let profile = Profile::open(&directory).map_err(|cause| ProfileResolveError::Missing {
            name: id.as_str().to_owned(),
            source: source.clone(),
            cause: Box::new(cause),
        })?;
        Ok(ResolvedProfile { profile, source })
    }

    /// The fallback profile, created when it is not there yet.
    fn default_profile(&self) -> Result<ResolvedProfile, ProfileResolveError> {
        let directory = self.profiles.join(DEFAULT_PROFILE);
        if self.create_default {
            fs::create_dir_all(&directory).map_err(|source| {
                ProfileResolveError::CreateDefault {
                    path: directory.clone(),
                    source,
                }
            })?;
            let config = directory.join(CONFIG_FILE);
            if !config.exists() {
                fs::write(&config, DEFAULT_CONFIG).map_err(|source| {
                    ProfileResolveError::CreateDefault {
                        path: config.clone(),
                        source,
                    }
                })?;
            }
        }
        let profile = Profile::open(&directory).map_err(|cause| ProfileResolveError::Missing {
            name: DEFAULT_PROFILE.to_owned(),
            source: ProfileSource::Default,
            cause: Box::new(cause),
        })?;
        Ok(ResolvedProfile {
            profile,
            source: ProfileSource::Default,
        })
    }
}

impl Default for ProfileResolver {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `--profile` is a directory argument rather than a name.
///
/// A name is one segment per [`ProfileId`]; anything absolute or multi-segment
/// cannot be one, so it keeps the old `--profile <dir>` meaning. A single
/// segment is always a name, even if a directory by that name happens to exist
/// in the current directory: the profile namespace is the config root, not the
/// shell's working directory.
fn is_path_argument(path: &Path) -> bool {
    path.is_absolute() || path.components().count() > 1
}

/// Why a launch could not resolve its profile.
#[derive(Debug, Error)]
pub enum ProfileResolveError {
    /// The project's settings file does not parse.
    #[error(transparent)]
    Settings(#[from] SettingsError),

    /// The global registry does not parse or is newer than this build.
    #[error(transparent)]
    Registry(#[from] RegistryError),

    /// A path-shaped `--profile` is not a profile.
    #[error(transparent)]
    Profile(#[from] ProfileError),

    /// A named profile is not there.
    #[error(
        "{source} names profile `{name}`, but that profile is not there; run `acpilot profile new \
         {name}`"
    )]
    Missing {
        /// The name that was asked for.
        name: String,
        /// The layer that asked for it.
        source: ProfileSource,
        /// What the profile reader said about the directory.
        #[source]
        cause: Box<ProfileError>,
    },

    /// A name is not a shape a profile can have.
    #[error("{source} names `{name}`, which is not a profile name: {cause}")]
    InvalidName {
        /// The text that was given.
        name: String,
        /// The layer that gave it.
        source: ProfileSource,
        /// The shape rule it broke.
        #[source]
        cause: ProfileIdError,
    },

    /// The first-run `default` could not be written.
    #[error("cannot create the default profile at {}: {source}", path.display())]
    CreateDefault {
        /// The directory or file that failed.
        path: PathBuf,
        /// What the filesystem said.
        source: io::Error,
    },
}

impl ProfileResolveError {
    /// Whether the failure is a name that does not resolve.
    #[must_use]
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::PROJECT_DIR;

    struct Fixture {
        _directory: tempfile::TempDir,
        workspace: PathBuf,
        profiles: PathBuf,
        user_config: PathBuf,
    }

    impl Fixture {
        /// A workspace, a profiles root with `work`, `personal` and `default`,
        /// and a global registry. `marker` and `active` are optional.
        fn new(marker: Option<&str>, active: Option<&str>) -> Self {
            let directory = tempfile::tempdir().expect("temp dir");
            let workspace = directory.path().join("workspace");
            fs::create_dir_all(&workspace).expect("workspace");
            if let Some(profile) = marker {
                let marker_dir = workspace.join(PROJECT_DIR);
                fs::create_dir_all(&marker_dir).expect("marker");
                fs::write(
                    marker_dir.join("config.toml"),
                    format!("profile = {profile:?}\n"),
                )
                .expect("marker file");
            }

            let profiles = directory.path().join("profiles");
            for name in [DEFAULT_PROFILE, "work", "personal"] {
                let root = profiles.join(name);
                fs::create_dir_all(&root).expect("profile directory");
                fs::write(root.join(CONFIG_FILE), DEFAULT_CONFIG).expect("profile config");
            }

            let user_config = directory.path().join("config.toml");
            let active_line = active
                .map(|name| format!("active_profile = {name:?}\n"))
                .unwrap_or_default();
            fs::write(&user_config, format!("schema_version = 1\n{active_line}"))
                .expect("registry");

            Self {
                _directory: directory,
                workspace,
                profiles,
                user_config,
            }
        }

        fn root(&self) -> Root {
            Root::resolve(Some(&self.workspace)).expect("root")
        }

        fn resolve(
            &self,
            flag: Option<&str>,
            environment: Option<&str>,
        ) -> Result<ResolvedProfile, ProfileResolveError> {
            ProfileResolver::new_in(&self.profiles, Some(&self.user_config))
                .flag(flag.map(Path::new))
                .environment(environment)
                .resolve(&self.root())
        }
    }

    #[test]
    fn the_resolution_order_is_a_table() {
        struct Case {
            name: &'static str,
            marker: Option<&'static str>,
            active: Option<&'static str>,
            flag: Option<&'static str>,
            environment: Option<&'static str>,
            expected: &'static str,
            source: ProfileSource,
        }

        let cases = [
            Case {
                name: "only the fallback",
                marker: None,
                active: None,
                flag: None,
                environment: None,
                expected: DEFAULT_PROFILE,
                source: ProfileSource::Default,
            },
            Case {
                name: "active_profile",
                marker: None,
                active: Some("personal"),
                flag: None,
                environment: None,
                expected: "personal",
                source: ProfileSource::Active(PathBuf::new()),
            },
            Case {
                name: "the project marker beats active_profile",
                marker: Some("work"),
                active: Some("personal"),
                flag: None,
                environment: None,
                expected: "work",
                source: ProfileSource::Project(PathBuf::new()),
            },
            Case {
                name: "the environment beats the project marker",
                marker: Some("work"),
                active: Some("default"),
                flag: None,
                environment: Some("personal"),
                expected: "personal",
                source: ProfileSource::Environment,
            },
            Case {
                name: "the flag beats the environment",
                marker: Some("work"),
                active: Some("work"),
                flag: Some("personal"),
                environment: Some("work"),
                expected: "personal",
                source: ProfileSource::Flag,
            },
            Case {
                name: "the flag and the marker disagree",
                marker: Some("personal"),
                active: None,
                flag: Some("work"),
                environment: None,
                expected: "work",
                source: ProfileSource::Flag,
            },
        ];

        for case in cases {
            let fixture = Fixture::new(case.marker, case.active);
            let resolved = fixture
                .resolve(case.flag, case.environment)
                .unwrap_or_else(|error| panic!("{}: {error}", case.name));
            assert_eq!(
                resolved.profile().name(),
                Some(case.expected),
                "{}",
                case.name
            );
            assert_eq!(
                std::mem::discriminant(resolved.source()),
                std::mem::discriminant(&case.source),
                "{}: {:?}",
                case.name,
                resolved.source()
            );
        }
    }

    #[test]
    fn a_missing_marker_is_not_found_and_names_the_command_that_fixes_it() {
        let fixture = Fixture::new(Some("work"), None);
        fs::remove_dir_all(fixture.profiles.join("work")).expect("remove profile");

        let error = fixture.resolve(None, None).expect_err("missing profile");
        assert!(error.is_missing(), "{error}");
        assert!(
            error.to_string().contains("acpilot profile new work"),
            "the hint is exact: {error}"
        );
        assert!(
            error.to_string().contains(".acpilot/config.toml"),
            "the file that asked is named: {error}"
        );
    }

    #[test]
    fn a_missing_environment_profile_does_not_fall_back() {
        let fixture = Fixture::new(None, None);
        fs::remove_dir_all(fixture.profiles.join("personal")).expect("remove profile");

        let error = fixture
            .resolve(None, Some("personal"))
            .expect_err("missing profile");
        assert!(error.is_missing(), "{error}");
        assert!(error.to_string().contains("ACPILOT_PROFILE"), "{error}");
        assert!(
            error.to_string().contains("acpilot profile new personal"),
            "{error}"
        );
    }

    #[test]
    fn the_default_profile_is_created_on_first_use() {
        let fixture = Fixture::new(None, None);
        fs::remove_dir_all(fixture.profiles.join(DEFAULT_PROFILE)).expect("remove default");

        let resolved = fixture.resolve(None, None).expect("created default");
        assert_eq!(resolved.profile().name(), Some(DEFAULT_PROFILE));
        assert!(resolved.is_default());
        assert!(
            fixture
                .profiles
                .join(DEFAULT_PROFILE)
                .join(CONFIG_FILE)
                .is_file(),
            "the profile can open on the next launch"
        );
    }

    #[test]
    fn a_path_shaped_flag_still_opens_a_directory() {
        let fixture = Fixture::new(None, None);
        let directory = fixture.profiles.join("personal");
        let resolved = fixture
            .resolve(Some(directory.to_str().expect("utf-8")), None)
            .expect("directory flag");
        assert_eq!(resolved.profile().root(), directory);
        assert!(matches!(resolved.source(), ProfileSource::FlagPath(_)));
    }

    #[test]
    fn a_missing_default_is_not_created_when_the_caller_says_no() {
        let fixture = Fixture::new(None, None);
        fs::remove_dir_all(fixture.profiles.join(DEFAULT_PROFILE)).expect("remove default");
        let error = ProfileResolver::new_in(&fixture.profiles, Some(&fixture.user_config))
            .create_default(false)
            .resolve(&fixture.root())
            .expect_err("default is not created");
        assert!(error.is_missing(), "{error}");
        assert!(!fixture.profiles.join(DEFAULT_PROFILE).exists());
    }
}
