//! Preferences: the typed values a launch reads and every surface applies.
//!
//! The type itself lives in `acpilot-proto`, because it is also the `prefs` frame
//! and the response body the profiles API will serve. This module is the
//! profile-side door to it: the shipped defaults, and the paths a watcher has to
//! read for a profile and the parents it extends.

pub use acpilot_proto::profile::{Preferences, ThemeSurface};

use crate::settings::{Settings, SettingsError};

/// The preferences the shipped defaults produce.
///
/// A caller that needs a value before a launch has resolved one (a demo, a test,
/// a window with no daemon) gets the same answer every other surface would.
///
/// # Errors
///
/// The checked-in `defaults.toml` does not parse. That is a build defect, not a
/// user configuration error, which is why it is returned rather than hidden.
pub fn defaults() -> Result<Preferences, SettingsError> {
    Ok(Settings::load(None, None)?.preferences)
}
