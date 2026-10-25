//! The settings a launch runs under: defaults, then the user's file, then the
//! project's.
//!
//! This is the bottom half of the profiles phase's config cascade (P4-2), and it is
//! deliberately the smallest honest version of it: three layers, merged per key,
//! with the whole of the built-in defaults written as data in `defaults.toml`. The
//! layers above it — profile files, `ACPILOT_*` variables, CLI flags — arrive with
//! the rest of P4-2; each one is another `merge` on the same [`Figment`], which is
//! why this is figment rather than a hand-rolled chain of `Option`s.
//!
//! Nothing here writes. A project's settings file exists because someone saved
//! settings, never because a client opened a directory — that is what makes
//! "open a file and start typing" cost nothing on disk.

use std::path::Path;

use acpilot_proto::profile::Preferences;
use figment::Figment;
// `Format` is the trait that gives `Toml` its `file` and `string` constructors;
// without it the provider is just a marker struct.
use figment::providers::{Format, Toml};
use thiserror::Error;

/// The settings every launch starts from.
///
/// Data rather than code: adding a preference is a line here plus a field on
/// [`Preferences`], and every UI picks it up without a second set of fallbacks.
pub const DEFAULTS: &str = include_str!("../defaults.toml");

/// What a launch resolved to, after merging.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
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
    /// Missing files are skipped rather than created: a person with no settings and
    /// a project with no settings both mean "defaults". A file that exists and does
    /// not parse is an error naming it, because running on a subset of what someone
    /// wrote is how a setting silently does nothing.
    ///
    /// # Errors
    ///
    /// A file that does not parse, or a value of the wrong type. The message carries
    /// the file and line figment recorded.
    pub fn load(user: Option<&Path>, project: Option<&Path>) -> Result<Self, SettingsError> {
        let mut figment = Figment::from(Toml::string(DEFAULTS));
        for path in [user, project].into_iter().flatten() {
            if path.is_file() {
                figment = figment.merge(Toml::file(path));
            }
        }
        figment.extract().map_err(|error| SettingsError::Invalid {
            message: error.to_string(),
        })
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn write(directory: &Path, name: &str, text: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, text).expect("settings file");
        path
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
}
