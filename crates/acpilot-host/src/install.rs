//! How this copy of Acpilot was installed, and who may therefore replace it.
//!
//! Every method except a bare binary drop leaves `install.json` beside the program or
//! under a shared prefix saying which method it was (ADR 0010). The marker is shipped by
//! the package rather than written by the program: the installations that matter are the
//! ones a package manager owns, and a binary cannot tell that from its own directory.
//!
//! Nothing here decides whether an update happens. It decides *who is asked*: a package
//! manager's files are the package manager's, so the answer there is a command; a tarball
//! or a `cargo install` is somebody's own copy, so the answer is bytes to download.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file an installation leaves behind.
pub const MARKER: &str = "install.json";

/// Who may replace the binary an installation put here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Replaces {
    /// A package manager owns the files; it is asked, not bypassed.
    #[serde(rename = "package-manager")]
    PackageManager,
    /// The running user may download a release and replace the binary in place.
    #[serde(rename = "self")]
    SelfOwned,
    /// Nobody knows how this copy arrived, so nobody touches it.
    #[serde(rename = "nobody")]
    Nobody,
}

/// A marker file, as the installation wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Install {
    /// How it was installed: `apt`, `brew`, `cargo`, `script`, …
    pub method: String,
    /// The version the installer put there, when it said.
    #[serde(default)]
    pub version: Option<String>,
}

impl Install {
    /// Who may replace this installation's binary.
    #[must_use]
    pub fn replaces(&self) -> Replaces {
        replaces(&self.method)
    }

    /// The command that reconciles a version, for the methods somebody else owns.
    ///
    /// `None` means the installation is the program's own to replace; a command for an
    /// unknown method would be a guess about somebody else's filesystem.
    #[must_use]
    pub fn update_command(&self) -> Option<&'static str> {
        match self.method.as_str() {
            "apt" => Some("sudo apt install --only-upgrade acpilot"),
            "dnf" => Some("sudo dnf upgrade acpilot"),
            "pacman" => Some("sudo pacman -Syu acpilot"),
            "brew" => Some("brew upgrade acpilot"),
            "msi" => Some("winget upgrade acpilot"),
            _ => None,
        }
    }
}

/// Which methods a package manager owns, and which the program may replace itself.
///
/// An unknown method is [`Replaces::Nobody`] rather than a benefit of the doubt: the
/// failure of guessing wrong is a binary rewritten behind a package manager's back.
#[must_use]
pub fn replaces(method: &str) -> Replaces {
    match method {
        "apt" | "dnf" | "pacman" | "brew" | "msi" => Replaces::PackageManager,
        "script" | "tarball" | "appimage" | "cargo" => Replaces::SelfOwned,
        _ => Replaces::Nobody,
    }
}

/// The installation `exe` came from, if it left a marker where this looks.
///
/// Order matters and is the ADR's: beside the program first (a tarball, an AppImage, a
/// `cargo install`), then the shared prefixes a system package uses, then per-user state.
/// The first marker wins; two markers is two installations disagreeing about one binary,
/// and merging them is how a daemon edits a file `dpkg` will overwrite on the next
/// upgrade.
#[must_use]
pub fn install_of(exe: &Path) -> Option<Install> {
    markers(exe).into_iter().find_map(read)
}

/// The places [`install_of`] looks, in order.
#[must_use]
pub fn markers(exe: &Path) -> Vec<PathBuf> {
    let mut places = Vec::new();
    if let Some(directory) = exe.parent() {
        places.push(directory.join(MARKER));
    }
    places.push(PathBuf::from("/usr/local/share/acpilot").join(MARKER));
    places.push(PathBuf::from("/usr/share/acpilot").join(MARKER));
    if let Some(state) = user_state() {
        places.push(state.join(MARKER));
    }
    places
}

/// `$XDG_DATA_HOME/acpilot`, else `~/.local/share/acpilot`.
fn user_state() -> Option<PathBuf> {
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(data).join("acpilot"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share").join("acpilot"))
}

/// One marker, or nothing when it is absent or does not parse.
///
/// A broken marker is not an error to report at startup: the answer it would have given
/// is "unknown", and that is the answer its absence gives too.
fn read(path: PathBuf) -> Option<Install> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Install, Replaces, install_of, replaces};

    fn write_marker(directory: &Path, body: &str) -> PathBuf {
        let marker = directory.join(super::MARKER);
        std::fs::write(&marker, body).expect("the marker is written");
        marker
    }

    #[test]
    fn a_marker_beside_the_program_is_the_installation() {
        let directory = tempfile::tempdir().expect("temp dir");
        write_marker(
            directory.path(),
            r#"{ "method": "cargo", "version": "0.4.0" }"#,
        );
        let exe = directory.path().join("acpilotd");
        let install = install_of(&exe).expect("the marker is found");
        assert_eq!(install.method, "cargo");
        assert_eq!(install.version.as_deref(), Some("0.4.0"));
        assert_eq!(install.replaces(), Replaces::SelfOwned);
        assert_eq!(install.update_command(), None, "it replaces itself");
    }

    #[test]
    fn a_package_manager_is_the_one_asked() {
        let directory = tempfile::tempdir().expect("temp dir");
        write_marker(directory.path(), r#"{ "method": "apt" }"#);
        let install = install_of(&directory.path().join("acpilot")).expect("the marker");
        assert_eq!(install.replaces(), Replaces::PackageManager);
        assert_eq!(
            install.update_command(),
            Some("sudo apt install --only-upgrade acpilot")
        );
    }

    #[test]
    fn a_method_nobody_recognises_is_nobody_s_to_replace() {
        assert_eq!(replaces("snap"), Replaces::Nobody);
        assert_eq!(replaces(""), Replaces::Nobody);
        let unknown = Install {
            method: "snap".to_owned(),
            version: None,
        };
        assert_eq!(unknown.update_command(), None);
    }

    #[test]
    fn every_documented_method_is_in_the_table() {
        for method in ["apt", "dnf", "pacman", "brew", "msi"] {
            assert_eq!(replaces(method), Replaces::PackageManager, "{method}");
        }
        for method in ["script", "tarball", "appimage", "cargo"] {
            assert_eq!(replaces(method), Replaces::SelfOwned, "{method}");
        }
    }

    #[test]
    fn no_marker_and_a_broken_marker_both_answer_nothing() {
        let directory = tempfile::tempdir().expect("temp dir");
        let exe = directory.path().join("acpilotd");
        // The shared prefixes on this machine may hold a real marker, so the question
        // asked here is about this directory's answer, not about the machine's.
        let beside = install_of(&exe);
        if beside.is_some() {
            // A machine with an installation in `/usr/share` or `~/.local/share` is a
            // machine whose answer is that installation, which is the fallback working.
            return;
        }
        write_marker(directory.path(), "{ not json");
        assert_eq!(install_of(&exe), None, "a broken marker is not an answer");
    }
}
