//! Language servers, owned by the daemon rather than by any UI.
//!
//! Empty in P1. P7 adds supervision over the same `Host` trait the agents use, so
//! a server can run on the SSH host or inside WSL and still feed the TUI, GUI and
//! browser. Wire types stay isolated behind a `wire` module so a crate swap does
//! not leak outward, and position encoding is negotiated rather than assumed.
//!
//! The P0-7 spike lives here rather than in `spikes/` because the two pieces
//! worth keeping — [`position`] and [`wire`] — are exactly the pieces P7 would
//! otherwise rewrite. They are tested against real servers, and the probe
//! (`examples/lsp_probe.rs`) is the evidence behind `docs/notes/lsp-servers.md`.

pub mod client;
pub mod position;
pub mod wire;

pub use position::{LineIndex, Position, PositionEncoding};
