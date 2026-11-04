//! What the daemon's tests share: the fixture agent, built on demand.
//!
//! One copy because every suite that starts a real session needs the same thing, and
//! a second transcription would drift the way two copies of a path do.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The mock agent, built on demand.
///
/// `acpilot-mock-agent` is a `[[bin]]` with `test = false`, so `nextest` does not
/// guarantee it is built before the tests that need it; building it here once is what
/// makes a suite work from a clean `cargo nextest run` as well as from
/// `cargo xtask check`, which builds the workspace first.
pub fn mock_agent() -> &'static Path {
    static AGENT: OnceLock<PathBuf> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            let exe = std::env::current_exe().expect("the test's own path");
            let debug = exe.parent().and_then(Path::parent).expect("target/debug");
            let path = debug.join("acpilot-mock-agent");
            if !path.exists() {
                let status = std::process::Command::new(env!("CARGO"))
                    .args([
                        "build",
                        "-p",
                        "acpilot-agent",
                        "--bin",
                        "acpilot-mock-agent",
                    ])
                    .current_dir(env!("CARGO_MANIFEST_DIR"))
                    .status()
                    .expect("cargo is on PATH");
                assert!(status.success(), "the mock agent must build");
            }
            path
        })
        .as_path()
}
