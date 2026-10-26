//! The native desktop app, on gpui.
//!
//! The GUI is the second UI over the same daemon API, so nothing here is
//! allowed to hold domain logic: if the TUI cannot do something, the GUI should
//! not be able to either. The desktop phase's task P10-2 starts with the shell — a window, the shared
//! theme tokens, and the profiles it was launched with.
//!
//! Two layers, and the split is deliberate:
//!
//! * [`theme`], [`profiles`], [`palette`] and [`keymap`] are plain data and are
//!   tested without a window. They are the part that must be right before a
//!   pixel is drawn, and they compile on a machine with no graphics stack at all.
//! * [`app`] is the gpui view, behind the `gui` feature. It is off by default so
//!   `cargo test --workspace` does not compile a renderer (or need
//!   `libxkbcommon-x11` to link) before there is something worth drawing.
//! * [`platform`] is the operating-system seam: today the single-instance
//!   socket that lets a second launch reuse the running process instead of
//!   paying for a device before the first frame. It is not GPU code, so it is
//!   compiled and tested without the feature.
//!
//! Framework decision and the pin: `docs/adr/0009-gui-framework.md`.

pub mod daemon;
pub mod keymap;
pub mod palette;
pub mod platform;
pub mod profiles;
pub mod theme;

#[cfg(feature = "gui")]
pub mod app;

#[cfg(feature = "gui")]
pub use app::Shell;
