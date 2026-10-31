//! The colour maths every surface shares.
//!
//! Three things live here and nothing else: the token set ([`theme`]), the rules a
//! palette is held to ([`contrast`]), and the solver that builds a palette out of a
//! terminal's own background and foreground ([`follow`]). They are one crate
//! because they are one question — "what does this token look like?" — and because
//! two very different callers need the answer from the same code:
//!
//! * every surface in the product, through [`acpilot_extensions`]'s re-export, on
//!   a machine with a terminal or a window;
//! * the site's showcase, compiled to WebAssembly, where a reader's own terminal
//!   colours are solved into a palette in their browser by these same lines.
//!
//! What is deliberately absent is the file format. Parsing a `theme.toml`, walking
//! an `extends` chain and refusing a hole are the loader's job and live with it in
//! `acpilot-extensions`; a page does not load extensions, and a leaf that imported
//! the loader would drag the daemon's whole dependency tree into a browser tab.

pub mod contrast;
pub mod follow;
pub mod theme;

pub use theme::{Color, ColorError, Palette, ResolvedTheme, SyntaxToken, UiToken};
