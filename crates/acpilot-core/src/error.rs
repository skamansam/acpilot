//! The one error type that crosses layers.
//!
//! Each layer maps it once, at its own boundary: the daemon to HTTP statuses, the
//! CLI to exit codes. Anything that leaks a raw `io::Error` to a user is a bug.

use std::io;

/// Exit codes are a public interface: scripts depend on them, so they are fixed
/// here rather than derived from whatever a layer happens to return.
pub mod exit {
    /// Failure that has no more specific class.
    pub const FAILURE: u8 = 1;
    /// Bad invocation: unknown flag, malformed argument. Owned by the CLI parser.
    pub const USAGE: u8 = 2;
    /// The requested object does not exist.
    pub const NOT_FOUND: u8 = 3;
    /// The request conflicts with current state (stale ETag, duplicate turn).
    pub const CONFLICT: u8 = 4;
    /// The policy refused, or the user denied.
    pub const PERMISSION_DENIED: u8 = 5;
}

/// Everything that can go wrong in a way a user needs to see.
#[derive(Debug, thiserror::Error)]
pub enum AcpilotError {
    /// A workspace, session, agent, or file was asked for and does not exist.
    #[error("not found: {what}")]
    NotFound {
        /// What was being looked for, phrased for a human.
        what: String,
    },

    /// The operation is valid but the current state disagrees (stale ETag,
    /// duplicate idempotency key with different content, session already closed).
    #[error("conflict: {what}")]
    Conflict {
        /// Description of the conflict.
        what: String,
    },

    /// Policy or user refused the operation. Distinct from a failure: nothing is
    /// broken, and retrying the same way will not help.
    #[error("permission denied: {what}")]
    PermissionDenied {
        /// What was refused, and by which rule when known.
        what: String,
    },

    /// The host (SSH, WSL, local helper) could not be reached or did not answer.
    #[error("host unavailable: {host}")]
    HostUnavailable {
        /// Host identifier or address.
        host: String,
    },

    /// A model provider could not be reached, refused the request, or answered
    /// with something that is not a completion. The daemon is fine; the
    /// inference server behind it is not.
    #[error("model unavailable ({provider}): {message}")]
    ModelUnavailable {
        /// Provider identifier, as configured by the user.
        provider: String,
        /// What went wrong, in a sentence a human can act on.
        message: String,
    },

    /// An agent process failed: it crashed, timed out, or refused the protocol.
    #[error("agent failed: {message}")]
    AgentFailed {
        /// What the agent did.
        message: String,
    },

    /// The peer violated the protocol (bad framing, unknown method, bad version).
    #[error("protocol error: {message}")]
    Protocol {
        /// What was malformed.
        message: String,
    },

    /// Filesystem or process I/O.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

impl AcpilotError {
    /// Build a `NotFound` with a formatted description.
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound { what: what.into() }
    }

    /// Build a `Conflict` with a formatted description.
    pub fn conflict(what: impl Into<String>) -> Self {
        Self::Conflict { what: what.into() }
    }

    /// Build a `PermissionDenied` with a formatted description.
    pub fn denied(what: impl Into<String>) -> Self {
        Self::PermissionDenied { what: what.into() }
    }

    /// Stable machine-readable code, matching `acpilot_proto::ErrorBody::code`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::Conflict { .. } => "conflict",
            Self::PermissionDenied { .. } => "permission_denied",
            Self::HostUnavailable { .. } => "host_unavailable",
            Self::ModelUnavailable { .. } => "model_unavailable",
            Self::AgentFailed { .. } => "agent_failed",
            Self::Protocol { .. } => "protocol",
            Self::Io(_) => "io",
        }
    }

    /// Process exit code for a CLI that hit this error.
    ///
    /// `USAGE` is never returned here: argument parsing failures are the parser's
    /// business and never reach an `AcpilotError`.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::NotFound { .. } => exit::NOT_FOUND,
            Self::Conflict { .. } => exit::CONFLICT,
            Self::PermissionDenied { .. } => exit::PERMISSION_DENIED,
            Self::HostUnavailable { .. }
            | Self::ModelUnavailable { .. }
            | Self::AgentFailed { .. }
            | Self::Protocol { .. }
            | Self::Io(_) => exit::FAILURE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_strings() {
        assert_eq!(AcpilotError::not_found("session s1").code(), "not_found");
        assert_eq!(AcpilotError::conflict("stale etag").code(), "conflict");
        assert_eq!(
            AcpilotError::denied("outside workspace").code(),
            "permission_denied"
        );
        assert_eq!(
            AcpilotError::HostUnavailable { host: "dev".into() }.code(),
            "host_unavailable"
        );
        assert_eq!(
            AcpilotError::ModelUnavailable {
                provider: "lmstudio".into(),
                message: "connection refused".into(),
            }
            .code(),
            "model_unavailable"
        );
        assert_eq!(
            AcpilotError::AgentFailed {
                message: "crashed".into()
            }
            .code(),
            "agent_failed"
        );
        assert_eq!(
            AcpilotError::Protocol {
                message: "bad frame".into()
            }
            .code(),
            "protocol"
        );
        assert_eq!(AcpilotError::Io(io::Error::other("disk")).code(), "io");
    }

    #[test]
    fn exit_codes_are_distinct_where_scripts_depend_on_them() {
        assert_eq!(AcpilotError::not_found("x").exit_code(), 3);
        assert_eq!(AcpilotError::conflict("x").exit_code(), 4);
        assert_eq!(AcpilotError::denied("x").exit_code(), 5);
        for generic in [
            AcpilotError::HostUnavailable { host: "h".into() },
            AcpilotError::ModelUnavailable {
                provider: "lmstudio".into(),
                message: "no server".into(),
            },
            AcpilotError::AgentFailed {
                message: "m".into(),
            },
            AcpilotError::Protocol {
                message: "m".into(),
            },
            AcpilotError::Io(io::Error::other("x")),
        ] {
            assert_eq!(
                generic.exit_code(),
                1,
                "{generic} should be a plain failure"
            );
        }
    }

    #[test]
    fn io_errors_convert_and_keep_their_message() {
        let err: AcpilotError = io::Error::new(io::ErrorKind::PermissionDenied, "locked").into();
        assert_eq!(err.code(), "io");
        assert!(err.to_string().contains("locked"));
    }
}
