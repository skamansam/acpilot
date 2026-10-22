//! The agent engine: registry, supervisor, sessions, and the ACP client.
//!
//! P2-1 lands the [`registry`]: a profile's `agents.toml` parsed into validated
//! [`AgentSpec`]s, secrets resolved at load rather than mid-turn, and a snapshot
//! that is replaced whole when the file changes. Everything after this — the
//! supervisor, sessions, the PTY adapter — consumes a spec and never reads the
//! file itself, so there is one place where a manifest can be wrong.
//!
//! The crate is headless on purpose: no daemon, no UI, and tests against a mock
//! agent rather than a real model.

pub mod error;
pub mod manifest;
pub mod registry;
pub mod secret;

pub use error::AgentError;
pub use manifest::{
    AgentSpec, CommandSpec, DEFAULT_IDLE_TIMEOUT, DEFAULT_SUBMIT_DELAY, MANIFEST_FILE, Parser,
    PtyProfile, Restart, SCHEMA_VERSION, Transport,
};
pub use registry::{AgentWatcher, Registry};
pub use secret::{MapResolver, SCHEME, Secret, SecretError, SecretRef, SecretResolver};
