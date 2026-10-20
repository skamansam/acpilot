//! UI-agnostic text model: buffer, selections, edits, highlighting, diff.
//!
//! Deliberately empty in P1. The point of the crate is that TUI, GUI and web all
//! share one implementation of editing semantics; it is filled in during P6 and
//! must never depend on a UI toolkit. `ropey`, `tree-sitter` and `imara-diff`
//! arrive with the first real code.
