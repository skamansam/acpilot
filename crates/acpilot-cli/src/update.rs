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
//! # What updating means
//!
//! For a copy nobody owns — a tarball, the installer script, an AppImage, `cargo
//! install` — the update is four steps and every one of them is checked: name the
//! release asset for this platform, download it, compare its sha256 with the one the
//! release published ([`crate::release`]), and only then put the programs in place.
//! The replacement writes beside the old binary and renames over it, so a failure
//! halfway leaves a program that still runs rather than a half-written one.
//!
//! `--from` exists for the machines a release cannot serve: an air-gapped box, a
//! build somebody made themselves, a platform the workflow does not package. It
//! performs the same replacement on bytes that were brought some other way, and still
//! refuses a copy that is not the user's.

use std::path::{Path, PathBuf};

use acpilot_host::install::{Install, Replaces, install_at};

use crate::release::{self, PROGRAMS};

/// The binary running this command, and what installed it.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The `acpilot` that is running.
    pub exe: PathBuf,
    /// The marker its installation left, when one was found.
    pub install: Option<Install>,
    /// Where that marker is, so an update can record the version it installed.
    pub marker: Option<PathBuf>,
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
        match install_at(&exe) {
            Some((marker, install)) => Self {
                exe,
                install: Some(install),
                marker: Some(marker),
            },
            None => Self {
                exe,
                install: None,
                marker: None,
            },
        }
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
                        lines.push("  acpilot update".to_owned());
                        lines.push(
                            "  acpilot update --from <a new acpilot binary>, for a machine that cannot download"
                                .to_owned(),
                        );
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

/// Put the programs of an unpacked release in place of the ones beside `exe`.
///
/// A release carries `acpilot`, `acpilotd` and `acpilot-gui` in one directory, and one
/// marker beside them describes all three (ADR 0010). Replacing only the program this
/// process happens to be would leave a client and a daemon of two versions in one
/// directory — the mismatch this command exists to end. A program that is not beside
/// `exe` is not installed on this machine, so it is left absent rather than added: an
/// update does not change which programs arrived.
///
/// # Errors
///
/// The first program that cannot be written, named, with the copies already replaced
/// left as they are (each was replaced atomically, so each is a whole program).
pub fn replace_programs(exe: &Path, unpacked: &[PathBuf]) -> Result<Vec<String>, String> {
    let directory = exe
        .parent()
        .ok_or_else(|| format!("{} has no directory to replace into", exe.display()))?;
    let mut replaced = Vec::new();
    for program in PROGRAMS {
        let Some(source) = unpacked
            .iter()
            .find(|path| path.file_stem().and_then(std::ffi::OsStr::to_str) == Some(program))
        else {
            continue;
        };
        let destination = if program == "acpilot" {
            exe.to_path_buf()
        } else {
            let name = source
                .file_name()
                .ok_or_else(|| format!("{} has no name", source.display()))?;
            directory.join(name)
        };
        if program != "acpilot" && !destination.exists() {
            continue;
        }
        replace(&destination, source)?;
        replaced.push(program.to_owned());
    }
    if !replaced.iter().any(|program| program == "acpilot") {
        return Err(format!(
            "the archive held no `acpilot` to write over {}",
            exe.display()
        ));
    }
    Ok(replaced)
}

/// Record the version an update installed, keeping the method that installed it.
///
/// The method does not change because the bytes did: a copy the shell installer made
/// is still a `script` install after `acpilot update` replaced the program, and
/// rewriting the marker to `tarball` (the archive's own word) would lose that. The
/// version does change, and the marker is what a later command asks.
fn record(plan: &Plan, version: &str) -> Result<(), String> {
    let (Some(path), Some(install)) = (&plan.marker, &plan.install) else {
        return Ok(());
    };
    let updated = Install {
        method: install.method.clone(),
        version: Some(version.to_owned()),
    };
    let body = serde_json::to_vec_pretty(&updated)
        .map_err(|error| format!("the marker cannot be written: {error}"))?;
    std::fs::write(path, body).map_err(|error| format!("{}: {error}", path.display()))
}

/// Download and verify one release, then put its programs in place.
fn from_release(plan: &Plan, requested: Option<&str>) -> u8 {
    let Some(target) = release::triple() else {
        eprintln!(
            "acpilot update: no release is built for {} {}; pass --from with a binary built here",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        return acpilot_core::error::exit::FAILURE;
    };
    let tag = match requested {
        Some(version) => release::tag_of(version),
        None => match release::latest_tag() {
            Ok(tag) => tag,
            Err(message) => {
                eprintln!("acpilot update: {message}");
                return acpilot_core::error::exit::FAILURE;
            }
        },
    };
    let version = release::version_of(&tag).to_owned();
    if version == acpilot_core::VERSION {
        println!(
            "acpilot {version} is already installed at {}",
            plan.exe.display()
        );
        return acpilot_core::error::exit::SUCCESS;
    }
    let stage = std::env::temp_dir().join(format!("acpilot-update-{}", std::process::id()));
    let outcome = install_release(plan, &tag, &version, target, &stage);
    // Whatever happened, the unpacked copies go: they are a second installation of
    // programs that are either already in place or untouched.
    let _ = std::fs::remove_dir_all(&stage);
    match outcome {
        Ok(replaced) => {
            println!(
                "installed acpilot {version} from {tag}: replaced {} at {}",
                replaced.join(", "),
                plan.exe.display()
            );
            println!(
                "a daemon that is already running keeps the older binary until it is restarted"
            );
            acpilot_core::error::exit::SUCCESS
        }
        Err(message) => {
            eprintln!("acpilot update: {message}");
            acpilot_core::error::exit::FAILURE
        }
    }
}

/// The checked part of an update: download, verify, unpack, replace, record.
fn install_release(
    plan: &Plan,
    tag: &str,
    version: &str,
    target: &str,
    stage: &Path,
) -> Result<Vec<String>, String> {
    // One archive per program, so an update makes three round trips. Each is signed
    // and checked on its own: a release that published one good archive and one
    // substituted one must not install half a version.
    let mut programs = Vec::new();
    for asset in release::assets(target) {
        let url = release::asset_url(tag, &asset)?;
        println!("downloading {url}");
        let bytes = release::get(&url)?;
        let checksum_url = release::asset_url(tag, &format!("{asset}.sha256"))?;
        let published = release::get(&checksum_url)?;
        let signature = release::get(&release::asset_url(
            tag,
            &format!("{asset}.sha256{}", release::SIGNATURE_SUFFIX),
        )?)?;
        release::verify_signature(&published, &signature)?;
        let published = std::str::from_utf8(&published)
            .map_err(|error| format!("the checksum file is not text: {error}"))?;
        let expected = release::parse_checksum(published)?;
        release::verify(&bytes, &expected)?;
        programs.extend(release::unpack(&bytes, stage)?.programs);
    }
    release::require_client(&programs)?;
    let replaced = replace_programs(&plan.exe, &programs)?;
    record(plan, version)?;
    Ok(replaced)
}

/// `acpilot update [--from PATH] [--version TAG] [--check]`, as an exit status.
pub fn run(from: Option<PathBuf>, version: Option<String>, check: bool) -> u8 {
    let plan = match Plan::here() {
        Ok(plan) => plan,
        Err(message) => {
            eprintln!("{message}");
            return acpilot_core::error::exit::FAILURE;
        }
    };
    for line in plan.report() {
        println!("{line}");
    }
    let replaces = plan.replaces();
    if check {
        // A copy nobody may replace is not a healthy check: it is a machine that has
        // to be fixed by hand, and the exit status says so to a script.
        return match replaces {
            Replaces::Nobody => acpilot_core::error::exit::FAILURE,
            Replaces::PackageManager | Replaces::SelfOwned => acpilot_core::error::exit::SUCCESS,
        };
    }
    match replaces {
        Replaces::SelfOwned => {}
        Replaces::PackageManager => {
            let method = plan
                .install
                .as_ref()
                .map_or("a package manager", |install| install.method.as_str());
            eprintln!(
                "acpilot update: {method} owns this copy, so nothing was replaced; upgrading it here would be undone by its next upgrade"
            );
            return acpilot_core::error::exit::FAILURE;
        }
        Replaces::Nobody => {
            eprintln!(
                "acpilot update: no installation marker says this copy may be replaced, so {} was not touched",
                plan.exe.display()
            );
            return acpilot_core::error::exit::FAILURE;
        }
    }
    match from {
        Some(from) => match replace(&plan.exe, &from) {
            Ok(()) => {
                println!("replaced {} with {}", plan.exe.display(), from.display());
                acpilot_core::error::exit::SUCCESS
            }
            Err(message) => {
                eprintln!("acpilot update: {message}");
                acpilot_core::error::exit::FAILURE
            }
        },
        None => from_release(&plan, version.as_deref()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use acpilot_host::install::Replaces;

    use super::{Plan, record, replace, replace_programs};

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
        assert!(!report.contains("acpilot update --from"), "{report}");
        assert!(
            !report.contains("  acpilot update\n"),
            "a package-managed copy is not offered the verb: {report}"
        );
    }

    #[test]
    fn a_tarball_is_offered_the_update_verb() {
        let directory = tempfile::tempdir().expect("temp dir");
        marker(directory.path(), r#"{ "method": "tarball" }"#);
        let plan = Plan::at(directory.path().join("acpilot"));
        let report = plan.report().join("\n");
        assert!(report.contains("  acpilot update\n"), "{report}");
        assert!(report.contains("acpilot update --from"), "{report}");
        assert_eq!(plan.replaces(), Replaces::SelfOwned);
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

    #[test]
    fn an_installation_is_replaced_where_it_is_not_added() {
        let directory = tempfile::tempdir().expect("temp dir");
        let exe = directory.path().join("acpilot");
        std::fs::write(&exe, b"old acpilot").expect("the old client");
        let daemon = directory.path().join("acpilotd");
        std::fs::write(&daemon, b"old daemon").expect("the old daemon");
        let unpacked = tempfile::tempdir().expect("temp dir");
        for (name, body) in [
            ("acpilot", &b"new acpilot"[..]),
            ("acpilotd", &b"new daemon"[..]),
            ("acpilot-gui", &b"new shell"[..]),
        ] {
            std::fs::write(unpacked.path().join(name), body).expect("an unpacked program");
        }
        let programs: Vec<PathBuf> = ["acpilotd", "acpilot-gui", "acpilot"]
            .iter()
            .map(|name| unpacked.path().join(name))
            .collect();
        let replaced =
            replace_programs(&exe, &programs).expect("the installation is replaced in place");
        assert_eq!(replaced, ["acpilotd", "acpilot"]);
        assert_eq!(std::fs::read(&exe).expect("the client"), b"new acpilot");
        assert_eq!(std::fs::read(&daemon).expect("the daemon"), b"new daemon");
        assert!(
            !directory.path().join("acpilot-gui").exists(),
            "a program this machine did not have is not added by an update"
        );
    }

    #[test]
    fn the_marker_keeps_its_method_and_takes_the_new_version() {
        let directory = tempfile::tempdir().expect("temp dir");
        marker(
            directory.path(),
            r#"{ "method": "script", "version": "0.1.0" }"#,
        );
        let plan = Plan::at(directory.path().join("acpilot"));
        record(&plan, "0.2.0").expect("the marker is written");
        let written = std::fs::read_to_string(directory.path().join("install.json"))
            .expect("the marker is readable");
        assert!(written.contains("\"script\""), "{written}");
        assert!(written.contains("\"0.2.0\""), "{written}");
        assert!(!written.contains("\"0.1.0\""), "{written}");
        assert_eq!(
            plan.replaces(),
            Replaces::SelfOwned,
            "the method still decides"
        );
    }

    #[test]
    fn a_copy_with_no_marker_records_nothing() {
        let directory = tempfile::tempdir().expect("temp dir");
        let plan = Plan::at(directory.path().join("acpilot"));
        record(&plan, "0.2.0").expect("nothing to write is not a failure");
        assert!(!directory.path().join("install.json").exists());
    }
}
