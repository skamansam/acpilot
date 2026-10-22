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
