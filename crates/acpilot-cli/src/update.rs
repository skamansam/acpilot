//! `acpilot update`: who may replace this copy of acpilot, and replacing it.
//!
//! A version mismatch between a client and a daemon is a fact the user has to act on,
//! and the action is different for every way acpilot can arrive on a machine. A copy
//! `apt` unpacked is `apt`'s to upgrade; a tarball somebody unzipped is theirs. The
//! installation says which it is in `install.json` (ADR 0010), and
//! [`acpilot_host::install`] is the only thing that reads it.
//!
//! This module never guesses. No marker means nobody's to replace, and the command
//! says so instead of writing over a file a package manager will put back on its next
//! upgrade.
//!
//! # What `--from` is for
//!
//! Fetching a release is somebody else's job for now: the download, the checksum and
//! the archive extraction belong to the installer scripts a release publishes, and a
//! client that reimplemented them would be a second answer to "which bytes are
//! correct". What a client *must* own is the replacement itself — write beside the old
//! binary, then rename over it — because that is the step that needs to know it may
//! touch the file at all. So `acpilot update --from <path>` performs that step on a
//! binary somebody else downloaded, and refuses when the copy is not the user's.

use std::path::{Path, PathBuf};

use acpilot_host::install::{Install, Replaces, install_of};

/// The binary running this command, and what installed it.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The `acpilot` that is running.
    pub exe: PathBuf,
    /// The marker its installation left, when one was found.
    pub install: Option<Install>,
}

impl Plan {
    /// Note this process's binary and the marker beside it.
    ///
    /// # Errors
    ///
    /// The platform would not say where the running binary is, which is the one thing
    /// this command cannot guess: `PATH` may hold a second copy, and updating that one
    /// would report success about a program nobody ran.
    pub fn here() -> Result<Self, String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("this binary does not know where it is: {error}"))?;
        Ok(Self::at(exe))
    }

    /// The same, for a binary known by path.
    #[must_use]
    pub fn at(exe: PathBuf) -> Self {
        let install = install_of(&exe);
        Self { exe, install }
    }

    /// Who may replace this copy.
    #[must_use]
    pub fn replaces(&self) -> Replaces {
        self.install
            .as_ref()
            .map_or(Replaces::Nobody, Install::replaces)
    }

    /// The lines a person reads instead of a log.
    #[must_use]
    pub fn report(&self) -> Vec<String> {
        let exe = self.exe.display();
        let version = acpilot_core::VERSION;
        match &self.install {
            None => vec![
                format!("acpilot {version} at {exe}"),
                "no install.json was found beside it, under /usr/share, or in this user's data directory"
                    .to_owned(),
                "so this copy will not be replaced: put a marker there, or reinstall with an installer"
                    .to_owned(),
            ],
            Some(install) => {
                let how = match &install.version {
                    Some(installed) if installed != version => {
                        format!("installed by {} as {installed}", install.method)
                    }
                    Some(installed) => format!("installed by {}, {installed}", install.method),
                    None => format!("installed by {}", install.method),
                };
                let mut lines = vec![format!("acpilot {version} at {exe}"), how];
                match self.replaces() {
                    Replaces::PackageManager => {
                        let command = install
                            .update_command()
                            .unwrap_or("upgrade it with the package manager that installed it");
                        lines.push(format!(
                            "{} owns these files, so it is the one to upgrade them:",
                            install.method
                        ));
                        lines.push(format!("  {command}"));
                    }
                    Replaces::SelfOwned => {
                        lines.push(format!(
                            "this copy is yours to replace, because {} is not a package manager:",
                            install.method
                        ));
                        lines.push("  acpilot update --from <the new acpilot binary>".to_owned());
                    }
                    Replaces::Nobody => {
                        lines.push(format!(
                            "{} is not an installation method this build knows, so nothing here will replace it",
                            install.method
                        ));
                    }
                }
                lines
            }
        }
    }
}

/// Put `from` in place of `exe`, atomically.
///
/// The new binary is written beside the old one and renamed over it, so a copy that
/// fails halfway is not a half-written program: `rename` on one filesystem is atomic,
/// and a reader either sees the old binary or the new one. The executable bit is set
/// before the rename for the same reason — a moment where the name points at a file
/// nobody may run is a moment a client started by a script fails.
///
/// # Errors
///
/// `exe` cannot be replaced in place (a different filesystem has no atomic rename), or
/// the write failed.
pub fn replace(exe: &Path, from: &Path) -> Result<(), String> {
    let staged = exe.with_extension("new");
    std::fs::copy(from, &staged)
        .map_err(|error| format!("{} could not be read: {error}", from.display()))?;
    // Through the platform seam: whether there is a bit to set at all is a property of
    // the operating system, and every such branch in this workspace lives in
    // `acpilot-host/src/platform`.
    acpilot_host::make_executable(&staged)
        .map_err(|error| format!("{} could not be made executable: {error}", staged.display()))?;
    std::fs::rename(&staged, exe).map_err(|error| {
        let _ = std::fs::remove_file(&staged);
        format!(
            "{} could not be replaced by {}: {error}",
            exe.display(),
            from.display()
        )
    })
}

/// `acpilot update [--from PATH]`, as an exit status.
pub fn run(from: Option<PathBuf>) -> u8 {
    let plan = match Plan::here() {
        Ok(plan) => plan,
        Err(message) => {
            eprintln!("{message}");
            return acpilot_core::error::exit::FAILURE;
        }
    };
    let Some(from) = from else {
        for line in plan.report() {
            println!("{line}");
        }
        // A copy nobody may replace is not an update that succeeded; it is a machine
        // that has to be fixed by hand, and the exit status says so to a script.
        return match plan.replaces() {
            Replaces::Nobody => acpilot_core::error::exit::FAILURE,
            Replaces::PackageManager | Replaces::SelfOwned => acpilot_core::error::exit::SUCCESS,
        };
    };
    match plan.replaces() {
        Replaces::SelfOwned => {}
        Replaces::PackageManager => {
            let method = plan
                .install
                .as_ref()
                .map_or("a package manager", |install| install.method.as_str());
            eprintln!(
                "{method} owns this copy, so it is the one to upgrade it; upgrading it here would be undone by its next upgrade"
            );
            if let Some(command) = plan.install.as_ref().and_then(Install::update_command) {
                eprintln!("  {command}");
            }
            return acpilot_core::error::exit::FAILURE;
        }
        Replaces::Nobody => {
            eprintln!(
                "no installation marker says this copy may be replaced, so {} was not touched",
                plan.exe.display()
            );
            return acpilot_core::error::exit::FAILURE;
        }
    }
    match replace(&plan.exe, &from) {
        Ok(()) => {
            println!("replaced {} with {}", plan.exe.display(), from.display());
            acpilot_core::error::exit::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            acpilot_core::error::exit::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use acpilot_host::install::Replaces;

    use super::{Plan, replace};

    fn marker(directory: &Path, body: &str) -> PathBuf {
        std::fs::write(directory.join("install.json"), body).expect("the marker");
        directory.to_path_buf()
    }

    #[test]
    fn a_copy_with_no_marker_is_nobody_s_to_replace() {
        let directory = tempfile::tempdir().expect("temp dir");
        let plan = Plan::at(directory.path().join("acpilot"));
        assert_eq!(plan.replaces(), Replaces::Nobody);
        let report = plan.report().join("\n");
        assert!(report.contains("no install.json was found"), "{report}");
        assert!(report.contains("will not be replaced"), "{report}");
    }

    #[test]
    fn a_package_managed_copy_reports_the_command_and_not_a_self_update() {
        let directory = tempfile::tempdir().expect("temp dir");
        marker(directory.path(), r#"{ "method": "apt" }"#);
        let plan = Plan::at(directory.path().join("acpilot"));
        assert_eq!(plan.replaces(), Replaces::PackageManager);
        let report = plan.report().join("\n");
        assert!(
            report.contains("sudo apt install --only-upgrade acpilot"),
            "{report}"
        );
        assert!(!report.contains("--from"), "{report}");
    }

    #[test]
    fn a_tarball_is_offered_the_replacement_verb() {
        let directory = tempfile::tempdir().expect("temp dir");
        marker(directory.path(), r#"{ "method": "tarball" }"#);
        let plan = Plan::at(directory.path().join("acpilot"));
        let report = plan.report().join("\n");
        assert!(report.contains("acpilot update --from"), "{report}");
    }

    #[test]
    fn replacing_writes_the_new_bytes_over_the_old_name() {
        let directory = tempfile::tempdir().expect("temp dir");
        let exe = directory.path().join("acpilot");
        std::fs::write(&exe, b"old").expect("the old binary");
        let from = directory.path().join("downloaded");
        std::fs::write(&from, b"new").expect("the new binary");
        replace(&exe, &from).expect("the replacement happens");
        assert_eq!(std::fs::read(&exe).expect("the replaced binary"), b"new");
        assert!(
            !exe.with_extension("new").exists(),
            "the staged name is not left behind"
        );
        assert!(from.exists(), "the source is left where the caller put it");
    }
}
