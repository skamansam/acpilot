//! A symlink out of the workspace is resolved before it is trusted.
//!
//! This case is an integration test rather than a unit test beside `Fs` because
//! it is a platform fact: `std::os::unix::fs::symlink` does not exist on Windows,
//! and the workspace's rule (`acpilot-host/tests/platform_boundary.rs`) keeps an
//! operating-system `cfg` out of `src/` — one file per platform under
//! `src/platform/`, so a branch cannot be silently Windows-only. `crates/acpilotd/
//! tests/files.rs` carries the same case for the HTTP route, gated the same way.
//!
//! What it protects is the one path check a string prefix cannot make: the path
//! stays inside the workspace, and the file it names does not.

#![cfg(unix)]

use std::sync::Arc;

use acpilot_agent::{Fs, FsError};
use acpilot_host::LocalHost;

#[tokio::test]
async fn a_symlink_out_of_the_workspace_does_not_launder_a_read() {
    let inside = tempfile::tempdir().expect("temp dir");
    let outside = tempfile::tempdir().expect("temp dir");
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "not for the agent").expect("write");
    let link = inside.path().join("link.txt");
    std::os::unix::fs::symlink(&secret, &link).expect("symlink");

    let fs = Fs::new(Arc::new(LocalHost), inside.path()).expect("the workspace resolves");

    let error = fs
        .read("session-1", &link, None, None)
        .await
        .expect_err("the link is resolved before it is trusted");
    assert!(
        matches!(error, FsError::OutsideWorkspace { .. }),
        "{error:?}"
    );
}
