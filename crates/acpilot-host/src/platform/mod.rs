//! The only place in the workspace that branches on the operating system.
//!
//! The rule is enforced by `tests/platform_boundary.rs`: a `cfg(windows)` or
//! `cfg(unix)` anywhere else in a crate's sources fails the build. The point is
//! not tidiness — every branch outside this directory is a place where one
//! platform's behaviour can drift without the other side noticing, and the
//! Windows side is written last, so nothing would notice for a phase.
//!
//! Both platforms implement [`spawn_pty`] with the same signature and share the
//! construction in `pty.rs`: `portable-pty` already owns the difference
//! (`forkpty` here, ConPTY there). What each file holds is the difference that
//! is *not* in the dependency — the echo, line-ending and query-answering
//! behaviour of a Windows pseudo-console, and the process-group question on
//! unix — so that when one becomes code there is exactly one place to put it.

#[cfg(unix)]
mod unix;

#[cfg(windows)]
mod windows;

#[cfg(not(any(unix, windows)))]
compile_error!(
    "acpilot-host supports unix and windows only: add src/platform/<target>.rs and a \
     `spawn_pty` there, or make the daemon refuse to build for this target"
);

#[cfg(unix)]
pub(crate) use unix::spawn_pty;

#[cfg(windows)]
pub(crate) use windows::spawn_pty;
