//! The agent engine: registry, supervisor, sessions, and the ACP client.
//!
//! [`registry`] reads a profile's `agents.toml` into validated [`AgentSpec`]s,
//! with secrets resolved at load rather than mid-turn and a snapshot replaced
//! whole when the file changes. [`supervisor`] starts an `acp-stdio` spec, keeps
//! its stderr in a bounded ring, and restarts it on a crash when the manifest says
//! so. [`permission`] decides what an agent may do — by rule, by default, or by
//! asking a person — and writes every decision down. [`fs`] serves the agent's own
//! file requests from the workspace, through the same [`acpilot_host::Host`] the
//! daemon uses, so a remote workspace reads and writes without this crate knowing
//! it is remote. Everything after this — sessions, the PTY adapter — consumes a
//! spec and never reads the file itself, so there is one place where a manifest
//! can be wrong.
//!
//! The crate is headless on purpose: no daemon, no UI, and tests against a mock
//! agent rather than a real model.

pub mod error;
pub mod fs;
pub mod keyring;
pub mod manifest;
pub mod permission;
// Operating-system branches live here and nowhere else in the workspace; the
// rule is enforced by `acpilot-host/tests/platform_boundary.rs`.
mod platform;
pub mod registry;
pub mod secret;
pub mod session;
pub mod supervisor;
pub mod terminal;

pub use error::AgentError;
pub use fs::{Fs, FsError};
pub use keyring::{CredentialStore, KeyringResolver, OsKeyring, SERVICE as KEYRING_SERVICE};
pub use manifest::{
    AgentDefinition, AgentSpec, CommandSpec, DEFAULT_IDLE_TIMEOUT, DEFAULT_SUBMIT_DELAY, EnvValue,
    MANIFEST_FILE, Parser, PtyProfile, Restart, SCHEMA_PATH, SCHEMA_URL, SCHEMA_VERSION, Transport,
    TransportDefinition, initial_manifest, json_schema, parse_definitions, schema_json_text,
};
pub use permission::{
    ASK_TIMEOUT, Answer, AuditEntry, Broker, Case, DecidedBy, Decision, Pattern, Policy,
    PolicyError, Rule, Ruling, Subject,
};
// The two wire enums the permission API is written in. They live in
// `acpilot-proto` because a client has to spell them, and are re-exported here
// because a caller of `Subject` or `Broker::answer` needs them to call it.
pub use acpilot_proto::permission::{PermissionKind, PermissionOption};
pub use registry::{AgentWatcher, Registry};
pub use secret::{MapResolver, SCHEME, Secret, SecretError, SecretRef, SecretResolver};
pub use session::{EVENT_CAPACITY, Session, SessionError, SessionOptions};
pub use supervisor::{
    AgentProcess, MAX_RESTARTS, Outcome, STDERR_CAPACITY, StderrRing, Supervisor, availability,
};
pub use terminal::{IDLE_TIMEOUT, MAX_OUTPUT, TerminalError, Terminals};
