//! Which distros are workspaces — and what to do when there are none.
//!
//! Two decisions, both cheap and both easy to get wrong in a way a user feels:
//!
//! * **Absence is not an error.** Most Windows machines have no WSL at all, and
//!   those users must never see a prompt, a warning or a broken feature for it.
//!   A missing `wsl.exe`, a non-zero exit, and an empty listing all mean the
//!   same thing: no hosts. `distros` therefore returns an empty vector instead
//!   of a `Result` — the failure has no user-visible form.
//! * **Not every distro is a workspace.** `docker-desktop` is a real distro name
//!   and a terrible place to open a project. It is filtered here, once, instead
//!   of in every UI that lists hosts.
//!
//! The process call is generic (`exe` plus args) so the absence cases can be
//! tested with real processes on both operating systems, rather than with a
//! mock that agrees with the mock.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::listing::decode_listing;

/// Every distro name the command printed, decoded; empty on any failure.
///
/// Spawn error, non-zero exit and no output are deliberately collapsed: a
/// caller that wanted to distinguish them would be building the error the
/// product must not show.
pub fn names(exe: &Path, args: &[&str]) -> Vec<String> {
    let Ok(output) = Command::new(exe).args(args).stdin(Stdio::null()).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    decode_listing(&output.stdout)
}

/// Drop the distros that exist to run containers, not to hold a checkout.
pub fn workspaces(names: Vec<String>) -> Vec<String> {
    names
        .into_iter()
        .filter(|name| is_workspace(name))
        .collect()
}

pub fn is_workspace(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    // Prefix, not equality: WSL registers both `docker-desktop` and
    // `docker-desktop-data`, and a future Docker may add a third.
    !name.to_ascii_lowercase().starts_with("docker-desktop")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The product's answer, not the raw listing: what a workspace picker shows.
    fn distros(exe: &Path, args: &[&str]) -> Vec<String> {
        workspaces(names(exe, args))
    }

    #[test]
    fn a_missing_binary_is_no_hosts() {
        // The path is absolute-ish and cannot exist; the spawn fails outright.
        let missing = PathBuf::from("acpilot-no-such-wsl-binary-2f6c1");
        assert!(distros(&missing, &["-l", "-q"]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_command_is_no_hosts() {
        assert!(distros(Path::new("sh"), &["-c", "exit 3"]).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn a_failing_command_is_no_hosts() {
        assert!(distros(Path::new("cmd"), &["/c", "exit 3"]).is_empty());
    }

    #[test]
    fn a_silent_command_is_no_hosts() {
        #[cfg(unix)]
        let names = distros(Path::new("sh"), &["-c", "true"]);
        #[cfg(windows)]
        let names = distros(Path::new("cmd"), &["/c", "rem nothing"]);
        assert!(names.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn output_is_decoded_then_filtered() {
        let names = distros(
            Path::new("sh"),
            &["-c", "printf 'Ubuntu-24.04\\ndocker-desktop\\n'"],
        );
        assert_eq!(names, ["Ubuntu-24.04"]);
    }

    #[cfg(windows)]
    #[test]
    fn output_is_decoded_then_filtered() {
        let names = distros(
            Path::new("cmd"),
            &["/c", "echo Ubuntu-24.04& echo docker-desktop"],
        );
        assert_eq!(names, ["Ubuntu-24.04"]);
    }

    #[test]
    fn only_real_distros_survive() {
        let kept = workspaces(vec![
            "Ubuntu-24.04".to_string(),
            "docker-desktop".to_string(),
            "docker-desktop-data".to_string(),
            "   ".to_string(),
            "Debian".to_string(),
        ]);
        assert_eq!(kept, ["Ubuntu-24.04", "Debian"]);
    }
}
