//! Domain model for Acpilot.
//!
//! This crate holds what the product *means* — workspaces, sessions, agents,
//! permissions — with no HTTP, no protocol types, and no UI. It must stay
//! dependency-light: if a type here needs a runtime to be useful, it belongs in a
//! layer above.
//!
//! P2 fills in the domain types. P1 establishes the error contract every layer
//! shares, because exit codes and HTTP statuses are a promise to users and
//! scripts, not an implementation detail.

pub mod error;
pub mod stats;

pub use error::AcpilotError;
