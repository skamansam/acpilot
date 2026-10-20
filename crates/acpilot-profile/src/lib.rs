//! Profiles: one identity bundle per context (`work`, `personal`, `client-acme`).
//!
//! Empty in P1. P4 makes a profile a directory, a keyring namespace and a store,
//! with one function resolving every mutable path so nothing else in the
//! workspace has to know where config lives. Resolution is explicit and loud: a
//! name that does not resolve is an error, because silently falling back is how a
//! client's code ends up sent with a personal key.
