//! The profiles the window opens on, as the list shows them.
//!
//! A launch names a path — a directory, or a file in one — and the profile follows
//! from it: the project's settings say which profile, and a project that says
//! nothing falls back to the daemon's default. `--profile` still wins when it is
//! given, because it is the one thing a person can say that a directory cannot.
//!
//! The point of implying it is that the usual launch has no flags at all:
//! `acpilot-gui` in a project, or `acpilot-gui src/main.rs`, opens on the profile
//! that project was set up with. Nothing is written to make that work.

use std::path::{Path, PathBuf};

use acpilot_profile::project::{PROJECT_DIR, PROJECT_FILE, Root};
use acpilot_profile::settings::SettingsError;
use acpilot_profile::{Profile, ProfileError};
use thiserror::Error;

/// One profile, read far enough to describe it in a sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellProfile {
    name: String,
    root: PathBuf,
    rules: usize,
    agents: bool,
}

impl ShellProfile {
    /// Open a profile directory and read what the list needs.
    ///
    /// A directory that is not a profile is an error rather than an empty entry,
    /// because the reason a window opens on the wrong profile is almost always
    /// that the path was wrong, and saying so is the whole fix.
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, ProfileError> {
        let profile = Profile::open(directory.as_ref())?;
        let policy = profile.policy()?;
        let name = profile.name().map_or_else(
            || profile.root().display().to_string(),
            std::string::ToString::to_string,
        );
        Ok(Self {
            name,
            root: profile.root().to_path_buf(),
            rules: policy.rules().len(),
            agents: profile.agents_path().is_file(),
        })
    }

    /// The profile's name, which is also its keyring namespace.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where it lives.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// How many policy rules the profile states.
    #[must_use]
    pub fn rules(&self) -> usize {
        self.rules
    }

    /// Whether it declares its own agents.
    #[must_use]
    pub fn has_agents_file(&self) -> bool {
        self.agents
    }

    /// One line for the sidebar: what this profile is, in the terms a person
    /// checks before running an agent in it.
    #[must_use]
    pub fn summary(&self) -> String {
        let rules = match self.rules {
            0 => "shipped policy".to_owned(),
            1 => "1 rule".to_owned(),
            count => format!("{count} rules"),
        };
        let agents = if self.agents {
            "own agents"
        } else {
            "no agents.toml"
        };
        format!("{rules} · {agents}")
    }
}

/// Why a launch could not decide which profile to open.
///
/// The profile and project errors are boxed: they carry enough detail to explain
/// themselves (a policy parse error names a key and a value), which makes them the
/// large half of a `Result` that is only ever returned once, at startup, to be
/// printed.
#[derive(Debug, Error)]
pub enum LaunchError {
    /// The path could not be read as a root.
    #[error(transparent)]
    Project(Box<acpilot_profile::project::ProjectError>),

    /// A settings file does not parse.
    #[error(transparent)]
    Settings(#[from] SettingsError),

    /// A directory named by `--profile` is not a profile.
    #[error(transparent)]
    Profile(Box<ProfileError>),

    /// The root names a profile that is not there.
    #[error("{source}\nthe settings at {} name profile `{name}`", config.display())]
    MissingProfile {
        /// The profile name the settings asked for.
        name: String,
        /// The file that asked for it.
        config: PathBuf,
        /// What the profile reader said.
        #[source]
        source: Box<ProfileError>,
    },

    /// There is no profile to open: no flag, no marker, no name.
    #[error(
        "{} is not a project with a profile: pass --profile <dir>, or set `profile` in its \
         {PROJECT_DIR}/{PROJECT_FILE}",
        root.display()
    )]
    NoProfile {
        /// The root that named nothing.
        root: PathBuf,
    },
}

/// The profiles a launch opens: `named` when it was given, else the one the
/// project's settings imply.
///
/// # Errors
///
/// The root cannot be resolved, its settings do not parse, the profile it names is
/// missing, or there is no name anywhere to follow.
pub fn open_for(path: Option<&Path>, named: &[PathBuf]) -> Result<Vec<ShellProfile>, LaunchError> {
    open_for_in(
        path,
        named,
        &acpilot_profile::profiles_dir(),
        Some(&acpilot_profile::user_config_path()),
    )
}

/// [`open_for`] with the profiles directory and the user's settings file made
/// explicit, so a test can own both rather than inherit this machine's.
fn open_for_in(
    path: Option<&Path>,
    named: &[PathBuf],
    profiles: &Path,
    user_config: Option<&Path>,
) -> Result<Vec<ShellProfile>, LaunchError> {
    if !named.is_empty() {
        return named
            .iter()
            .map(|directory| {
                ShellProfile::open(directory).map_err(|error| LaunchError::Profile(Box::new(error)))
            })
            .collect();
    }

    let root = Root::resolve(path).map_err(|error| LaunchError::Project(Box::new(error)))?;
    let settings = root.settings_with(user_config)?;
    let Some(name) = settings.profile else {
        return Err(LaunchError::NoProfile {
            root: root.root().to_path_buf(),
        });
    };
    let directory = profiles.join(&name);
    let profile = ShellProfile::open(&directory).map_err(|source| LaunchError::MissingProfile {
        name: name.clone(),
        config: root.project().map_or_else(
            || root.root().to_path_buf(),
            |project| project.config_path(),
        ),
        source: Box::new(source),
    })?;
    Ok(vec![profile])
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A profile directory named `work`, so the name assertion is about the
    /// segment rather than about whatever the temp directory was called.
    fn profile(config: &str) -> (tempfile::TempDir, ShellProfile) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("work");
        fs::create_dir(&root).expect("profile directory");
        fs::write(root.join(acpilot_profile::CONFIG_FILE), config).expect("write config");
        let profile = ShellProfile::open(&root).expect("the profile opens");
        (dir, profile)
    }

    /// A project at `root` naming `profile`, and a profiles directory holding it.
    fn project_naming(root: &Path, profile: &str) -> (PathBuf, PathBuf) {
        fs::create_dir_all(root.join(PROJECT_DIR)).expect("marker directory");
        fs::write(
            root.join(PROJECT_DIR).join(PROJECT_FILE),
            format!("profile = \"{profile}\"\n"),
        )
        .expect("project settings");
        let profiles = root.join("profiles");
        fs::create_dir_all(profiles.join(profile)).expect("profile directory");
        fs::write(
            profiles.join(profile).join(acpilot_profile::CONFIG_FILE),
            "schema_version = 1\n",
        )
        .expect("profile settings");
        (root.to_path_buf(), profiles)
    }

    #[test]
    fn a_path_implies_the_profile_the_project_names() {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = directory.path().join("thing");
        fs::create_dir_all(&root).expect("root");
        let (root, profiles) = project_naming(&root, "work");

        let opened = open_for_in(Some(&root), &[], &profiles, None).expect("implied");
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].name(), "work");
    }

    #[test]
    fn a_file_in_the_tree_implies_the_same_profile() {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = directory.path().join("thing");
        let deep = root.join("src/deep");
        fs::create_dir_all(&deep).expect("deep");
        let (root, profiles) = project_naming(&root, "work");
        let file = deep.join("main.rs");
        fs::write(&file, "fn main() {}").expect("file");

        let opened = open_for_in(Some(&file), &[], &profiles, None).expect("implied");
        assert_eq!(opened[0].name(), "work");
        assert_eq!(root, directory.path().join("thing"));
    }

    #[test]
    fn an_explicit_profile_wins_over_the_one_a_project_names() {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = directory.path().join("thing");
        fs::create_dir_all(&root).expect("root");
        let (_root, profiles) = project_naming(&root, "work");
        fs::create_dir_all(profiles.join("personal")).expect("other profile");
        fs::write(
            profiles.join("personal").join(acpilot_profile::CONFIG_FILE),
            "schema_version = 1\n",
        )
        .expect("other settings");

        let named = vec![profiles.join("personal")];
        let opened = open_for_in(None, &named, &profiles, None).expect("named");
        assert_eq!(opened[0].name(), "personal");
    }

    #[test]
    fn a_project_naming_a_profile_that_is_not_there_says_which_file_asked() {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = directory.path().join("thing");
        fs::create_dir_all(&root).expect("root");
        let (root, profiles) = project_naming(&root, "work");
        fs::remove_dir_all(profiles.join("work")).expect("remove profile");

        let error = open_for_in(Some(&root), &[], &profiles, None).expect_err("missing profile");
        let message = error.to_string();
        assert!(message.contains("work"), "{message}");
        assert!(message.contains(PROJECT_FILE), "{message}");
    }

    #[test]
    fn a_directory_with_no_project_is_told_how_to_get_a_profile() {
        let directory = tempfile::tempdir().expect("temp dir");
        let error = open_for_in(
            Some(directory.path()),
            &[],
            &directory.path().join("p"),
            None,
        )
        .expect_err("nothing to open");
        assert!(matches!(error, LaunchError::NoProfile { .. }));
        assert!(error.to_string().contains("--profile"), "{error}");
    }

    #[test]
    fn a_profile_is_named_by_its_directory_and_counts_its_rules() {
        let (_dir, profile) = profile(
            r#"
schema_version = 1

[policy]
default = "deny"

[[policy.rules]]
id = "allow-format"
decision = "allow"
tool = "execute"

[[policy.rules]]
id = "refuse-publish"
decision = "deny"
tool = "execute"
"#,
        );

        assert_eq!(profile.name(), "work");
        assert_eq!(profile.rules(), 2);
        assert!(!profile.has_agents_file());
        assert_eq!(profile.summary(), "2 rules · no agents.toml");
        assert!(profile.root().ends_with("work"));
    }

    #[test]
    fn a_profile_with_no_policy_says_so_instead_of_claiming_zero_rules() {
        let (_dir, profile) = profile("schema_version = 1\n");
        assert_eq!(profile.rules(), 0);
        assert_eq!(profile.summary(), "shipped policy · no agents.toml");
    }

    #[test]
    fn one_rule_is_singular_because_a_count_is_read_by_a_person() {
        let (_dir, profile) = profile(
            "schema_version = 1\n\n[policy]\n\n[[policy.rules]]\nid = \"one\"\ndecision = \"allow\"\ntool = \"execute\"\n",
        );
        assert_eq!(profile.summary(), "1 rule · no agents.toml");
    }

    #[test]
    fn a_declared_agents_file_reaches_the_list() {
        let (dir, profile) = profile("schema_version = 1\n");
        fs::write(
            dir.path().join("work").join("agents.toml"),
            "schema_version = 1\n",
        )
        .expect("write agents");
        let reopened = ShellProfile::open(dir.path().join("work")).expect("it reopens");
        assert!(reopened.has_agents_file());
        assert_eq!(reopened.summary(), "shipped policy · own agents");
        assert_eq!(profile.summary(), "shipped policy · no agents.toml");
    }

    #[test]
    fn a_directory_that_is_not_a_profile_fails_by_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error =
            ShellProfile::open(dir.path()).expect_err("an empty directory is not a profile");
        assert!(
            error.to_string().contains("config.toml"),
            "the error says what is missing: {error}"
        );
    }
}
