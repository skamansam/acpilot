//! The only place in the workspace that branches on the operating system.
//!
//! The rule is enforced by `tests/platform_boundary.rs`: a `cfg(windows)` or
//! `cfg(unix)` anywhere else in a crate's sources fails the build. The point is
//! not tidiness — every branch outside this directory is a place where one
//! platform's behaviour can drift without the other side noticing, and the
//! Windows side is written last, so nothing would notice for a phase.
//!
//! Both platforms implement `spawn_pty` with the same signature and share the
//! construction in `pty.rs`: `portable-pty` already owns the difference
//! (`forkpty` here, ConPTY there). What each file holds is the difference that
//! is *not* in the dependency — the echo, line-ending and query-answering
//! behaviour of a Windows pseudo-console, and the process-group question on
//! unix — so that when one becomes code there is exactly one place to put it.
//!
//! [`CASE_INSENSITIVE_PATHS`] is the other fact that had to live here: whether a
//! path is compared case-insensitively is a property of the platform, and the
//! permission broker's globs need the answer without branching on it.

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

/// Whether this platform compares paths case-insensitively.
///
/// A policy glob has to fold case on Windows to be usable there, and must not
/// fold it on POSIX: folding both would let a rule about `/work/secret` match
/// `/Work/Secret` on Linux, which is a rule the author never wrote. It is a
/// constant rather than a function because the answer is a property of the
/// target, not of the call.
pub const CASE_INSENSITIVE_PATHS: bool = cfg!(windows);

#[cfg(not(any(unix, windows)))]
compile_error!(
    "acpilot-host supports unix and windows only: add src/platform/<target>.rs and a \
     `spawn_pty` there, or make the daemon refuse to build for this target"
);

#[cfg(unix)]
pub(crate) use unix::{
    default_endpoint, default_shell, detach, private_dir, private_file, spawn_pty,
};

#[cfg(windows)]
pub(crate) use windows::{
    default_endpoint, default_shell, detach, private_dir, private_file, spawn_pty,
};
