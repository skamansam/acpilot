//! Package the helper into the daemon, when a build was given one.
//!
//! The daemon is the thing that gets installed, and a remote host has nothing on it but
//! what we put there. So the helper travels inside `acpilotd` rather than beside it:
//! `include_bytes!` needs a path that exists at every build, so this script always writes
//! `OUT_DIR/acpilotd-helper` and writes **nothing** when no artifact was named. An empty
//! file is how "this build carries no helper" is spelled, and [`acpilotd::helper::bundled`]
//! turns that into `None`.
//!
//! `ACPILOT_HELPER_BIN` is the artifact. A release build points it at the static musl
//! binary; a developer build leaves it unset and the daemon behaves exactly as before —
//! asking the host for `acpilotd-helper` on its `PATH`. A variable that names a file which
//! is not there is a packaging mistake, so it fails the build instead of producing a daemon
//! that silently cannot install what it promised.
//!
//! A relative value is read from the workspace root, the directory a person is in when they
//! type `cargo build` and the one a workflow writes `$GITHUB_WORKSPACE`-relative paths
//! against. Cargo runs a build script in the script's own crate directory instead, so the
//! plain `target/…` form resolved to `crates/acpilotd/target/…` — a directory no build
//! writes — and every Linux release failed here with "could not be read".

use std::path::{Path, PathBuf};

/// The nearest ancestor of `start` whose `Cargo.toml` declares `[workspace]`.
///
/// Looking for the table rather than counting `..` keeps working if this crate moves; a
/// crate built outside a workspace gets `None` and the relative value means what it did
/// before, which is the reading cargo itself gave it.
fn workspace_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| {
            std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|manifest| {
                manifest
                    .lines()
                    .any(|line| line.trim().trim_start_matches("\u{feff}") == "[workspace]")
            })
        })
        .map(Path::to_path_buf)
}

/// The file `ACPILOT_HELPER_BIN` names, with a relative value taken from the workspace root.
fn helper_path(named: &Path) -> PathBuf {
    if named.is_absolute() {
        return named.to_path_buf();
    }
    workspace_root(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
        .join(named)
}

fn main() {
    println!("cargo:rerun-if-env-changed=ACPILOT_HELPER_BIN");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("acpilotd-helper");
    let bytes = match std::env::var_os("ACPILOT_HELPER_BIN") {
        None => Vec::new(),
        Some(named) => {
            let path = helper_path(Path::new(&named));
            println!("cargo:rerun-if-changed={}", path.to_string_lossy());
            std::fs::read(&path).unwrap_or_else(|error| {
                panic!(
                    "ACPILOT_HELPER_BIN={} was read as {} and could not be read: {error}",
                    named.to_string_lossy(),
                    path.to_string_lossy()
                )
            })
        }
    };
    std::fs::write(&out, &bytes).expect("the embedded helper is written to OUT_DIR");
}
