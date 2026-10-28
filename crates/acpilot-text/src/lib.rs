//! The text model every surface edits through: one rope, one undo history, one
//! answer to "what is a word".
//!
//! The TUI, the desktop app and the browser each own their buffers locally — a
//! keystroke never waits for the daemon (ADR 0004) — and this is the crate they
//! all edit through, so "backspace" cannot mean three things in three frontends.
//! It is deliberately a leaf: no HTTP client, no `ratatui`, no `gpui`, no LSP.
//! Anything that only a terminal can decide (colours, key chords, whether a
//! cursor blinks) belongs to the pane, not here.
//!
//! Three decisions shape the API, and each one is the fix for a bug that is
//! cheap here and expensive later:
//!
//! - **Offsets are counted in characters, not bytes.** A byte offset is what a
//!   rope stores and what the LSP wire speaks, but it is also the thing that
//!   panics when a click lands inside `日` or `😀`. Every public index here is a
//!   char offset, and the byte offsets are handed back through
//!   [`Buffer::rope`] for the code paths that need them (highlighting, LSP
//!   document sync), where the conversion is that layer's business. The
//!   conversions between bytes, UTF-8 columns and UTF-16 columns already live in
//!   `acpilot-lsp::position`, against the same text.
//! - **The rope holds `\n` only.** A file's line ending and byte-order mark are
//!   metadata on the buffer, not content inside it, so line arithmetic has one
//!   case instead of two and saving writes the file's own ending back
//!   ([`LineEnding`]) — an editor that quietly converts a repository to LF is a
//!   diff that touches every line.
//! - **An edit is a set of replacements, and it is its own undo step.** Typing,
//!   backspacing, indenting three lines and a multi-cursor insert all take the
//!   same path ([`Edit`] → [`Change`] → [`History`]), so no operation can be the
//!   one that forgot how to be undone.
//!
//! What is not here yet: syntax highlighting and diffs (language-intelligence
//! phase (P7) and the editor phase's task P6-9), which is why `tree-sitter` and
//! `imara-diff` are not dependencies.

pub mod buffer;
pub mod document;
pub mod edit;
pub mod history;
pub mod selection;

pub use buffer::{Buffer, LineColumn, LineEnding, TextError, is_word};
pub use document::Document;
pub use edit::{Edit, IndentMode, IndentStyle, Replacement};
pub use history::{Change, Flavour, GROUP_WINDOW, History};
pub use selection::{Cursor, Motion, Selection, Selections};
