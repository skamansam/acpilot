//! The only place in the workspace that branches on the operating system.
//!
//! The rule is enforced by `tests/platform_boundary.rs`: a `cfg(windows)` or
//! `cfg(unix)` anywhere else in a crate's sources fails the build. The point is
//! not tidiness — every branch outside this directory is a place where one
//! platform's behaviour can drift without the other side noticing, and the
//! Windows side is written last, so nothing would notice for a phase.
//!
//! Both platforms implement [`spawn_pty`] with the same signature. On Windows it
//! returns [`Unsupported`](crate::HostError::Unsupported) until P2-5 lands
//! ConPTY; the fact that the signature exists is what makes that a decision
//! rather than an omission.

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
