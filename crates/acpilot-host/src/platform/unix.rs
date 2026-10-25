//! unix and macOS: `forkpty` through `portable-pty`.
//!
//! Nothing in the construction is unix-specific today — `portable-pty` picks
//! `forkpty` here and ConPTY on Windows — so the body is shared in `pty.rs` and
//! this file is the seam where a unix-only difference would go.
//!
//! Two candidates are known and neither is needed yet: a process group of its
//! own for tree-kill (`Pty::kill` reaches one process), and `TIOCSWINSZ`
//! behaviour on macOS when the window changes while a program is reading. Both
//! would be written blind today for the platform that is developed first, which
//! is the shape of change this seam exists to prevent rather than encourage.

use crate::pty::Pty;
use crate::{ExecCommand, HostError, Size};

pub(crate) fn spawn_pty(command: &ExecCommand, size: Size) -> Result<Pty, HostError> {
    crate::pty::spawn(command, size)
}

/// Start `command` in a process group of its own.
///
/// A daemon spawned by a TUI has to outlive the terminal that started it. The
/// terminal's hang-up signal and its Ctrl-C both go to the foreground process
/// group, and a child in the caller's group receives them: the daemon would die
/// with the window that had just closed. `process_group(0)` puts it in a new
/// group whose leader is the child, which is what `setsid` buys without `unsafe`
/// and without re-executing through a shell.
pub(crate) fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

/// Make `path` readable only by its owner (`0700`).
///
/// The daemon's runtime directory holds a socket, a token and a log. On POSIX the
/// filesystem is the access control for all three, so the directory's mode is
/// part of the security story rather than tidiness (ADR 0004 §3).
pub(crate) fn private_dir(path: &std::path::Path) -> std::io::Result<()> {
    set_mode(path, 0o700)
}

/// Make `path` readable and writable only by its owner (`0600`).
pub(crate) fn private_file(path: &std::path::Path) -> std::io::Result<()> {
    set_mode(path, 0o600)
}

/// The unix socket a daemon binds and a client probes when nobody said.
///
/// In the runtime directory, so it lives as long as the login session does. A
/// filesystem path rather than a URL because on this platform the filesystem is
/// the access control: another user cannot open the socket at all (ADR 0004 §3).
pub(crate) fn default_endpoint() -> String {
    crate::runtime_dir()
        .join("acpilot.sock")
        .display()
        .to_string()
}

/// The user's shell, or `sh` when `$SHELL` says nothing useful.
///
/// `$SHELL` is what a login session would have given; falling back to `sh`
/// rather than to a hard-coded `/bin/bash` keeps working on systems whose shell
/// lives somewhere else. An empty value counts as unset — `env` can export one.
pub(crate) fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| "sh".to_owned())
}

fn set_mode(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    std::fs::set_permissions(path, permissions)
}
