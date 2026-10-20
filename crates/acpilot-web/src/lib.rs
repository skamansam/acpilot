//! Serves the compiled web UI from the daemon binary.
//!
//! Empty in P1. P11 adds the Svelte + CodeMirror frontend under `web/` and this
//! crate embeds its build output, so `acpilot serve` is one binary with no
//! separate asset directory to lose.
