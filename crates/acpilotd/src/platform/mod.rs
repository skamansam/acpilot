//! Where the daemon listens, per platform.
//!
//! ADR 0004 §3 settles this: a unix socket by default, TCP only when asked. The
//! filesystem is the access control on the socket — the directory is `0700` and
//! the socket `0600`, so another local user cannot open it at all — and a TCP
//! listener does not inherit that property. That is the whole argument for the
//! default, and it is why TCP is an explicit flag rather than a fallback.
//!
//! The branch lives here rather than in `main.rs` for the reason the host crate
//! states: a `cfg(windows)` in the middle of the daemon compiles on the platform
//! it was written for and silently never runs on the other.

use std::path::PathBuf;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::default_target;
#[cfg(unix)]
pub use unix::{serve, shutdown};
#[cfg(windows)]
pub(crate) use windows::default_target;
#[cfg(windows)]
pub use windows::{serve, shutdown};

/// What the daemon binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A unix socket, made private to the user who started the daemon.
    Socket(PathBuf),
    /// A TCP address. Explicit, and unauthenticated until the token lands —
    /// which is why it is never the default.
    Tcp(String),
}

impl std::fmt::Display for Target {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Socket(path) => write!(formatter, "{}", path.display()),
            Self::Tcp(addr) => write!(formatter, "http://{addr}"),
        }
    }
}

/// The target the flags name, or the platform's default.
///
/// Both flags is a mistake rather than a precedence rule: a daemon that quietly
/// chose one would leave a client probing the other.
///
/// # Errors
///
/// The two flags together. The string is the message, because the caller is a
/// process about to exit and there is nothing to recover from.
pub fn resolve_target(addr: Option<String>, socket: Option<PathBuf>) -> Result<Target, String> {
    match (addr, socket) {
        (Some(_), Some(_)) => Err(
            "--addr and --socket name two endpoints; pick one — the socket is the default, \
             and --addr is the opt-in TCP listener"
                .to_owned(),
        ),
        (Some(addr), None) => Ok(Target::Tcp(addr)),
        (None, Some(path)) => Ok(Target::Socket(path)),
        (None, None) => Ok(default_target()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_flag_decides_or_the_platform_does() {
        assert_eq!(
            resolve_target(None, Some(PathBuf::from("/tmp/x.sock"))).expect("socket"),
            Target::Socket(PathBuf::from("/tmp/x.sock"))
        );
        assert_eq!(
            resolve_target(Some("127.0.0.1:7777".to_owned()), None).expect("tcp"),
            Target::Tcp("127.0.0.1:7777".to_owned())
        );
        assert!(resolve_target(Some("127.0.0.1:7777".to_owned()), Some("/tmp/x".into())).is_err());
        assert_eq!(
            resolve_target(None, None).expect("default"),
            default_target()
        );
    }
}
