//! The native desktop app, on gpui.
//!
//! Empty in P1. The GUI is the second UI over the same daemon API, so nothing
//! here is allowed to hold domain logic: if the TUI cannot do something, the GUI
//! should not be able to either. P0-3 decides whether gpui meets the text
//! budgets before P10 commits, and `iced` is the documented fallback.
