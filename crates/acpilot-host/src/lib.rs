//! Where files and processes actually live.
//!
//! Empty in P1. Every filesystem and process operation in Acpilot is meant to go
//! through this trait, so that "on the remote box" and "in the WSL distro" are
//! the same code path as "on this laptop" — including language servers and
//! agents, not just editing. Windows→WSL deliberately goes through the helper
//! binary rather than `\\wsl.localhost` paths, which are slow enough to feel
//! broken.
