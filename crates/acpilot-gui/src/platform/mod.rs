//! Where the shell meets the operating system, one file per platform.
//!
//! The same rule `acpilot-host` enforces across the workspace applies here: a
//! `cfg` that names an operating system may appear only under `src/platform/`, so
//! a branch cannot be written in a hurry somewhere that never compiles it.
//!
//! Today the seam answers one question — is another Acpilot already running, and
//! if so, can this launch hand it the request? — because that is what makes first
//! paint cheap. The design phase's task P0-3's phase trace put 320–411 ms of a ~620 ms start inside
//! gpui's Direct3D device creation, before a window exists, and nothing at the pin
//! can defer or overlap it. A launch that reuses a live process never reaches
//! that phase: on the machine where it was measured, a cold launch mapped its
//! window in 69–70 ms and a repeat launch in 7–8 ms (`docs/notes/gui-bench.md`,
//! §"What the app does about it").

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{Claim, Instance, claim};
#[cfg(windows)]
pub use windows::{Claim, Instance, claim};
