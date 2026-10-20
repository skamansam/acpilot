//! Language servers, owned by the daemon rather than by any UI.
//!
//! Empty in P1. P7 adds supervision over the same `Host` trait the agents use, so
//! a server can run on the SSH host or inside WSL and still feed the TUI, GUI and
//! browser. Wire types stay isolated behind a `wire` module so a crate swap does
//! not leak outward, and position encoding is negotiated rather than assumed.
