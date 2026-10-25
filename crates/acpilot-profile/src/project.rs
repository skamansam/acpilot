//! Which directory a launch is about, and whether it is a project yet.
//!
//! A client used to be told its workspace, and told that a profile is a directory
//! with a `config.toml`. That is right for a service and wrong for a person: the
//! directory they are in, or the file they named, already says which project they
//! mean. This module is that reading, and it is the only place it happens — a GUI
//! with a second opinion is how two windows open on two roots.
//!
//! The rules, in the order they apply:
//!
//! * No path → the current directory is the root.
//! * A directory → that directory is the root. It is not walked up: naming a
//!   directory is naming the workspace, and a client that quietly climbed out of
//!   `src/` to its parent would be editing a tree the person did not choose.
//! * A file → its directory, then each parent in turn, until one holds
//!   `.acpilot/config.toml`. Naming a file is naming something *in* a project, and
//!   the project is rarely the directory the file happens to sit in.
//! * A file with no project above it → a temporary root at the file's directory.
//!   Nothing is created on disk: `.acpilot/config.toml` appears when a person saves
//!   project settings, not because they opened a note.
//!
//! A root without a marker is [`Root::Temporary`] rather than an error, because
//! "open this file and start typing" is a supported way to use Acpilot. It is also
//! why the settings cascade treats a temporary root as having no project layer: the
//! user's settings and the built-in defaults are the whole answer until someone
//! says otherwise, on purpose.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::settings::{Settings, SettingsError};

/// The directory that marks a project, and holds everything project-scoped.
pub const PROJECT_DIR: &str = ".acpilot";

/// The file a project keeps its settings in, inside [`PROJECT_DIR`].
pub const PROJECT_FILE: &str = "config.toml";

/// A directory that holds `.acpilot/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    root: PathBuf,
}

impl Project {
    /// The project at `root`, or `None` when it has no marker.
    ///
    /// Only the directory itself is checked: a marker is a statement about a tree,
    /// and finding one by searching upwards here would make `Project::at` a second
    /// copy of [`Root::resolve`] with its own idea of when to stop.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Option<Self> {
        let root = root.into();
        Self::config_in(&root).is_file().then_some(Self { root })
    }

    fn config_in(root: &Path) -> PathBuf {
        root.join(PROJECT_DIR).join(PROJECT_FILE)
    }

    /// Where the project is.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where its settings live.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        Self::config_in(&self.root)
    }
}

/// What a launch resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    /// A directory with a `.acpilot/config.toml`.
    Project(Project),
    /// A directory with no marker, and nothing written to it.
    Temporary {
        /// The directory the launch is about.
        root: PathBuf,
    },
}

impl Root {
    /// Resolve `arg`, or the current directory when it is `None`.
    ///
    /// # Errors
    ///
    /// The current directory could not be read, or the path's directory does not
    /// exist — a root that is made up is a workspace the daemon would then refuse
    /// with a worse message.
    pub fn resolve(arg: Option<&Path>) -> Result<Self, ProjectError> {
        match arg {
            None => {
                let directory = std::env::current_dir().map_err(ProjectError::CurrentDirectory)?;
                Self::in_directory(directory)
            }
            Some(path) if path.is_dir() => Self::in_directory(path.to_path_buf()),
            Some(path) => Self::from_file(path),
        }
    }

    /// A root for `directory`, which is used as given rather than walked up.
    fn in_directory(directory: PathBuf) -> Result<Self, ProjectError> {
        let root = canonical(&directory).map_err(|source| ProjectError::Directory {
            path: directory.clone(),
            source,
        })?;
        Ok(match Project::at(&root) {
            Some(project) => Self::Project(project),
            None => Self::Temporary { root },
        })
    }

    /// A root for `file`: the nearest ancestor with a marker, else its directory.
    fn from_file(file: &Path) -> Result<Self, ProjectError> {
        let parent = match file.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        // The error names the path the caller gave, not the parent that failed:
        // a message about `/x/no/such` for an argument of `/x/no/such/dir` reads as
        // a second wrong guess.
        let parent = canonical(&parent).map_err(|source| ProjectError::Directory {
            path: file.to_path_buf(),
            source,
        })?;
        for candidate in parent.ancestors() {
            if let Some(project) = Project::at(candidate) {
                return Ok(Self::Project(project));
            }
        }
        Ok(Self::Temporary { root: parent })
    }

    /// The directory this launch is about.
    #[must_use]
    pub fn root(&self) -> &Path {
        match self {
            Self::Project(project) => project.root(),
            Self::Temporary { root } => root,
        }
    }

    /// The project, when there is one.
    #[must_use]
    pub fn project(&self) -> Option<&Project> {
        match self {
            Self::Project(project) => Some(project),
            Self::Temporary { .. } => None,
        }
    }

    /// The settings this root runs under: defaults, then the user's file, then the
    /// project's — the project layer being absent for a temporary root.
    ///
    /// # Errors
    ///
    /// A config file that exists and does not parse, or does not hold the types it
    /// declares. Never a missing file: a person with no settings has the defaults.
    pub fn settings(&self) -> Result<Settings, SettingsError> {
        let user = crate::user_config_path();
        self.settings_with(Some(user.as_path()))
    }

    /// [`Root::settings`] with the user layer made explicit.
    ///
    /// A caller that owns the user file — a test, a portable install, `--config`
    /// when it lands — passes it here rather than letting the environment decide
    /// what a launch reads.
    ///
    /// # Errors
    ///
    /// A config file that exists and does not parse.
    pub fn settings_with(&self, user: Option<&Path>) -> Result<Settings, SettingsError> {
        Settings::load(user, self.project().map(Project::config_path).as_deref())
    }
}

/// An absolute, symlink-free version of `path`.
///
/// Canonical, because this answer becomes the daemon's workspace root and the
/// broker decides "inside the workspace" by prefix: a root that is itself a symlink
/// answers that question wrongly for everything under it.
fn canonical(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path)
}

/// Why a root could not be resolved.
#[derive(Debug, Error)]
pub enum ProjectError {
    /// The current directory could not be read.
    #[error("cannot read the working directory: {0}")]
    CurrentDirectory(io::Error),

    /// A directory on the way to the root does not exist or cannot be read.
    #[error("cannot use {} as a root: {source}", path.display())]
    Directory {
        /// The directory that failed.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_at(directory: &Path) -> PathBuf {
        let marker = directory.join(PROJECT_DIR);
        fs::create_dir_all(&marker).expect("marker directory");
        fs::write(marker.join(PROJECT_FILE), "schema_version = 1\n").expect("marker file");
        directory.to_path_buf()
    }

    #[test]
    fn a_directory_is_its_own_root_and_is_not_walked_up() {
        let workspace = tempfile::tempdir().expect("temp dir");
        let project = project_at(workspace.path());
        let inner = workspace.path().join("src");
        fs::create_dir_all(&inner).expect("inner");

        let resolved = Root::resolve(Some(&inner)).expect("resolved");
        assert!(
            matches!(resolved, Root::Temporary { .. }),
            "naming a directory names the workspace, even inside a project"
        );
        assert_eq!(resolved.root(), inner.canonicalize().expect("canonical"));

        // And the project it sits in still resolves when named directly.
        let resolved = Root::resolve(Some(&project)).expect("resolved");
        assert_eq!(
            resolved.project().map(Project::root),
            Some(
                workspace
                    .path()
                    .canonicalize()
                    .expect("canonical")
                    .as_path()
            )
        );
    }

    #[test]
    fn a_file_climbs_to_the_nearest_project() {
        let workspace = tempfile::tempdir().expect("temp dir");
        project_at(workspace.path());
        let deep = workspace.path().join("a/b/c");
        fs::create_dir_all(&deep).expect("deep");
        let file = deep.join("notes.md");
        fs::write(&file, "hello").expect("file");

        let resolved = Root::resolve(Some(&file)).expect("resolved");
        assert_eq!(
            resolved.project().map(Project::root),
            Some(
                workspace
                    .path()
                    .canonicalize()
                    .expect("canonical")
                    .as_path()
            ),
            "the project is the one the file is in, not the file's directory"
        );
    }

    #[test]
    fn a_file_with_no_project_above_it_becomes_a_temporary_root() {
        let workspace = tempfile::tempdir().expect("temp dir");
        let deep = workspace.path().join("loose/deeper");
        fs::create_dir_all(&deep).expect("deep");
        let file = deep.join("note.md");
        fs::write(&file, "hello").expect("file");

        let resolved = Root::resolve(Some(&file)).expect("resolved");
        assert!(resolved.project().is_none());
        assert_eq!(
            resolved.root(),
            deep.canonicalize().expect("canonical"),
            "a temporary root is the file's directory"
        );
        assert!(
            !deep.join(PROJECT_DIR).exists(),
            "opening a file must not create project data"
        );
    }

    #[test]
    fn a_path_that_is_not_there_yet_is_read_as_a_file() {
        let workspace = tempfile::tempdir().expect("temp dir");
        project_at(workspace.path());
        let file = workspace.path().join("draft.md");

        let resolved = Root::resolve(Some(&file)).expect("resolved");
        assert!(
            resolved.project().is_some(),
            "\"open a file I am about to write\" still means the project it is in"
        );
    }

    #[test]
    fn a_directory_that_does_not_exist_is_an_error_naming_it() {
        let workspace = tempfile::tempdir().expect("temp dir");
        let missing = workspace.path().join("no/such/dir");
        let error = Root::resolve(Some(&missing)).expect_err("no such directory");
        assert!(error.to_string().contains("no/such/dir"), "{error}");
    }
}
