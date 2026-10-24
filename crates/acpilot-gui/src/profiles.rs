//! The profiles the window was launched with, as the list shows them.
//!
//! The shell does not go looking for profiles yet: a profile is a directory with
//! a `config.toml`, and *which* directories count is P4's resolution rule, not
//! something the GUI should invent a second version of. So the window shows the
//! ones it was handed, and `--profile` is where they come from — the same
//! explicit, loud rule the daemon follows.

use std::path::{Path, PathBuf};

use acpilot_profile::{Profile, ProfileError};

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
