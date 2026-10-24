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
