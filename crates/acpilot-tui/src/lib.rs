//! The TUI: an editor and an agent cockpit in one terminal window.
//!
//! Empty in the foundation phase (P1). The editor phase (P6) builds it on `ratatui` with the immediate-mode loop owning
//! state, `ropey` + `tree-sitter` for text, and `tui-term` for agent terminals.
//! It is a client of the daemon, exactly like the GUI and the browser, so it
//! never reaches into the engine directly.
