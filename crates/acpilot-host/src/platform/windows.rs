//! Windows local PTY: ConPTY.
//!
//! `portable-pty`'s `ConPtySystem` is `CreatePseudoConsole` plus a pair of
//! pipes, and the child is started with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`.
//! Creation is therefore the same code as unix (`pty::spawn`), and this file is
//! the seam for everything ConPTY does differently. Three of those differences
//! are observable and none of them is a `cfg` at the call site:
//!
//! * **Echo and line endings.** ConPTY echoes input and reports `\n` as `\r\n`,
//!   so a transcript is not byte-identical to unix before the program even
//!   starts. Anything that compares output against a fixture has to allow it —
//!   that belongs to the terminal pane's renderer, which parses bytes anyway.
//! * **Terminal queries block.** A program that asks for the cursor position
//!   (`CSI 6 n`) waits for an answer. The design phase's task P0-5 measured `wsl.exe` never starting
//!   until DSR was answered, so a query responder is not optional here the way
//!   it is on a real unix pty. That responder lives with the pane (P2-5); this
//!   function only has to give it a terminal to answer on.
//! * **Kill is not hang-up.** ConPTY has no process group: `TerminateProcess`
//!   ends the root process and its children keep running, where on unix the
//!   session leader's death hangs up the rest. Closing the pseudo-console is the
//!   documented way to end what is attached to it, which is what dropping the
//!   `Pty` does. `Pty::kill` is therefore a reliable stop for the process the
//!   caller started, and the tree is the engine phase's P2-5 to bound.
//!
//! The construction is shared with unix rather than copied here: a second copy
//! of the same fifteen lines with a Windows comment on it is how the two sides
//! drift. What is genuinely Windows-only goes below, and there is nothing yet
//! that cannot be said in prose.

use crate::pty::Pty;
use crate::{ExecCommand, HostError, Size};

pub(crate) fn spawn_pty(command: &ExecCommand, size: Size) -> Result<Pty, HostError> {
    crate::pty::spawn(command, size)
}

/// `CREATE_NEW_PROCESS_GROUP`: the daemon is not in the console's Ctrl-C group.
///
/// Windows has no session leader to hand the child to, so this is the whole of
/// what "detached" means here: a Ctrl-C in the terminal that spawned the daemon
/// is not delivered to it. Closing the console still ends what is attached to
/// it, which is why the daemon is spawned with its own (absent) console stream
/// rather than inheriting the TUI's.
pub(crate) fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;

    /// `CREATE_NEW_PROCESS_GROUP`.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

/// Loopback HTTP, because the named pipe ADR 0004 names is not built yet.
///
/// The ADR's Windows endpoint is `\\.\pipe\acpilot-<user>`, whose ACL is the
/// per-user boundary. Until that transport exists (P10-5's named pipe), the daemon
/// listens on loopback and the API says so: a token is what makes a TCP listener
/// safe, and the token is a separate task. Reporting a pipe nobody is serving
/// would be worse than reporting a socket that needs one.
pub(crate) fn default_endpoint() -> String {
    "http://127.0.0.1:7777".to_owned()
}

/// `%COMSPEC%`, or `cmd.exe` when the variable is missing or empty.
///
/// Windows has no `$SHELL`: the command interpreter is what `COMSPEC` names, and
/// `cmd.exe` is the documented fallback. `powershell.exe` is not the default
/// here even though it is the more useful shell to write against — a terminal is
/// expected to behave like the one the operating system would open.
pub(crate) fn default_shell() -> String {
    std::env::var("COMSPEC")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| "cmd.exe".to_owned())
}

/// Nothing to do: the per-user profile directory already excludes other users.
///
/// The named pipe that replaces the socket gets its own ACL from
/// `-<user>` plus SDDL when the socket transport lands (ADR 0004 §3); a mode on a
/// file is not how Windows expresses that, and setting one would be a no-op that
/// reads like a guarantee.
pub(crate) fn private_dir(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// See [`private_dir`].
pub(crate) fn private_file(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}
