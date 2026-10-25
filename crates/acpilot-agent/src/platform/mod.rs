//! The only place in this crate that names an operating system.
//!
//! The rule is the host crate's, and it is enforced for every crate by
//! `acpilot-host/tests/platform_boundary.rs`: a `cfg` naming a platform is allowed
//! only under `src/platform/`. A `cfg(windows)` in the middle of the supervisor
//! would compile on Windows and silently never run anywhere else. Three functions
//! carry every difference, and both platforms provide them:
//!
//! - [`configure`] shapes the child before it starts: its own process group on
//!   unix, no console window on Windows.
//! - [`kill_tree`] ends the child *and* what it started. Agents are commonly
//!   launched through wrappers (`npx …`, `uvx …`); killing only the immediate
//!   child orphans the real agent, which then keeps the workspace's file handles
//!   and stderr pipe open.
//! - [`find_program`] answers whether a command is on `PATH`, which is the whole
//!   of `AgentStatus::Available` until a handshake exists.
//!
//! A target that is neither unix nor Windows is a compile error rather than a
//! build that starts an agent it cannot stop.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(not(any(unix, windows)))]
compile_error!(
    "acpilot-agent supervises processes on unix and windows only: add \
     src/platform/<target>.rs with configure, kill_tree and find_program"
);

#[cfg(unix)]
pub(crate) use unix::{configure, find_program, kill_tree};

#[cfg(windows)]
pub(crate) use windows::{configure, find_program, kill_tree};
