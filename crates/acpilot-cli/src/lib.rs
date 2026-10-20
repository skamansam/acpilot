//! CLI behaviour that is worth testing without spawning a process.
//!
//! The CLI is a client of the daemon API — it holds no domain logic. The one
//! thing it owns outright is the process exit code, because that is the interface
//! scripts see, so it lives here with tests rather than in `main`.

use acpilot_core::AcpilotError;

/// Exit code for a failed command.
///
/// Argument-parsing failures never reach this function: `clap` exits with
/// [`acpilot_core::error::exit::USAGE`] itself.
#[must_use]
pub fn exit_code(error: &AcpilotError) -> u8 {
    error.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_failures_are_distinguishable() {
        assert_eq!(exit_code(&AcpilotError::not_found("session s1")), 3);
        assert_eq!(exit_code(&AcpilotError::conflict("stale etag")), 4);
        assert_eq!(exit_code(&AcpilotError::denied("outside workspace")), 5);
    }

    #[test]
    fn everything_else_is_a_plain_failure() {
        assert_eq!(
            exit_code(&AcpilotError::HostUnavailable {
                host: "dev@box".into()
            }),
            1
        );
    }
}
