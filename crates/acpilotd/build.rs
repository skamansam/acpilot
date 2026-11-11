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

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=ACPILOT_HELPER_BIN");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("acpilotd-helper");
    let bytes = match std::env::var_os("ACPILOT_HELPER_BIN") {
        None => Vec::new(),
        Some(path) => {
            println!("cargo:rerun-if-changed={}", path.to_string_lossy());
            std::fs::read(&path).unwrap_or_else(|error| {
                panic!(
                    "ACPILOT_HELPER_BIN={} could not be read: {error}",
                    path.to_string_lossy()
                )
            })
        }
    };
    std::fs::write(&out, &bytes).expect("the embedded helper is written to OUT_DIR");
}
