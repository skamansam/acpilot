//! Windows local PTY — deliberately the last platform to be finished.
//!
//! ConPTY is not `forkpty` with a different name: it is `CreatePseudoConsole`
//! plus a pair of pipes and a resize call on the pseudo-console handle, and the
//! process is started with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`. P0-5 measured
//! the one behaviour that bites first — under ConPTY a program that asks for the
//! cursor position (`CSI 6 n`) waits for an answer — which is why the answer here
//! is a typed refusal rather than a silent fall back to pipes.
//!
//! Exec already works on Windows (`std::process`), so only this function is
//! outstanding. P2-5 owns turning it into ConPTY; when that lands it is the only
//! file that changes.

use crate::pty::Pty;
use crate::{ExecCommand, HostError, Size};

pub(crate) fn spawn_pty(command: &ExecCommand, size: Size) -> Result<Pty, HostError> {
    let _ = (command, size);
    Err(HostError::Unsupported {
        feature: "a local PTY",
        platform: "Windows",
        owner: "P2-5 adds ConPTY; exec and the WSL helper already work",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windows_pty_refusal_names_its_owner() {
        let error = spawn_pty(&ExecCommand::new("cmd.exe"), Size::DEFAULT).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Windows"), "{message}");
        assert!(message.contains("P2-5"), "{message}");
    }
}
