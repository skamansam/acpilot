//! The window's palette: the shared one, in the words gpui draws with.
//!
//! The tokens and the conversion are [`acpilot_extensions::theme::AppliedTheme`]'s,
//! not this crate's: the terminal, the window and the browser read one
//! `theme.toml`, and they apply it through one type. A token added to
//! [`acpilot_extensions::UiToken::ALL`] reaches every surface with no edit here,
//! and a palette with a hole is refused by name — at load, for every surface, for
//! the same reasons.
//!
//! What is left in this module is the projection gpui needs:
//! [`AppliedTheme::ui`] is `0xRRGGBB`, which is what `gpui::rgb` takes. The
//! browser projects the same value into a CSS variable. Neither surface owns a
//! token list, so neither can fall behind the other by forgetting one.

pub use acpilot_extensions::theme::{AppliedTheme as GuiTheme, ThemeError};
