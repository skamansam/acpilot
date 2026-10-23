//! One error type for every way `agents.toml` can be wrong.
//!
//! A manifest is validated before it is used, never at spawn time, so every
//! variant here names the agent and the field a person has to edit. That is the
//! whole point of the type: `acpilot agents ls` can print the reason a
//! configured agent is not running instead of logging "spawn failed" later.

use std::io;

use thiserror::Error;

/// What went wrong while loading the agent registry.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The manifest could not be read.
    #[error("{path}: {source}")]
    Io {
        /// Path as a human should see it.
        path: String,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },

    /// The manifest did not parse, or carried a key the schema does not know.
    #[error("{path}: {message}")]
    Toml {
        /// Path as a human should see it.
        path: String,
        /// The parser's message, which already names the key and the line.
        message: String,
    },

    /// The file was written for a schema this build does not speak.
    #[error("{path}: schema_version {found} is not supported (this build speaks {expected})")]
    SchemaVersion {
        /// Path as a human should see it.
        path: String,
        /// Version found in the file.
        found: u64,
        /// Version this build understands.
        expected: u32,
    },

    /// The file parsed but declared no agents.
    #[error("{path}: agents may not be empty; delete the file instead of emptying it")]
    NoAgents {
        /// Path as a human should see it.
        path: String,
    },

    /// An identifier is not the shape the registry requires.
    #[error("{what} {id:?} {why}")]
    InvalidId {
        /// Which namespace: `agent id`.
        what: &'static str,
        /// The offending value.
        id: String,
        /// What the rule is, in a few words.
        why: &'static str,
    },

    /// Two agents claim the same identifier. Never resolved by ordering.
    #[error("duplicate agent id {id:?}")]
    Duplicate {
        /// The duplicated identifier.
        id: String,
    },

    /// A transport was configured without a field it cannot run without.
    #[error("agent {id:?}: transport {transport:?} requires {field:?}")]
    MissingField {
        /// Agent identifier.
        id: String,
        /// Transport as written in the manifest.
        transport: &'static str,
        /// Field name as written in the manifest.
        field: &'static str,
    },

    /// A field is present but empty, which is never what was meant.
    #[error("agent {id:?}: {field:?} may not be empty")]
    EmptyField {
        /// Agent identifier.
        id: String,
        /// Field name as written in the manifest.
        field: &'static str,
    },

    /// A field has no meaning for this transport, so it is refused rather than
    /// silently ignored: a `sentinel` on an ACP agent is a misunderstanding
    /// worth reporting, not a typo worth ignoring.
    #[error("agent {id:?}: {field:?} has no meaning for transport {transport:?}")]
    UnexpectedField {
        /// Agent identifier.
        id: String,
        /// Transport as written in the manifest.
        transport: &'static str,
        /// Field name as written in the manifest.
        field: &'static str,
    },

    /// A field holds something the schema cannot mean.
    #[error("agent {id:?}: {field:?}: {why}")]
    InvalidField {
        /// Agent identifier.
        id: String,
        /// Field name as written in the manifest.
        field: &'static str,
        /// What is wrong with the value, in one sentence.
        why: String,
    },

    /// A `keyring:` reference did not resolve, which is reported when the
    /// profile loads rather than in the middle of a turn.
    #[error("agent {id:?}: {variable}: {message}")]
    Secret {
        /// Agent identifier.
        id: String,
        /// Where the reference appeared: an environment variable name, or `auth`.
        variable: String,
        /// The resolver's message, which starts with the reference itself.
        message: String,
    },

    /// A reference reached into another profile without the agent saying so.
    #[error(
        "agent {id:?}: {variable}: {reference} names another profile; \
         cross-profile secrets require allow_cross_profile = true"
    )]
    CrossProfileSecret {
        /// Agent identifier.
        id: String,
        /// Where the reference appeared.
        variable: String,
        /// The reference as written.
        reference: String,
    },

    /// The file watcher could not be started.
    #[error("{path}: cannot watch for changes: {message}")]
    Watch {
        /// Path as a human should see it.
        path: String,
        /// The underlying failure, rendered.
        message: String,
    },

    /// A transport was asked to be spawned by code that does not drive it yet.
    ///
    /// Reported rather than guessed at: a `pty` agent started as if it spoke ACP
    /// would fail on the first protocol line, and the manifest was not the thing
    /// that was wrong.
    #[error("agent {id:?}: transport {transport:?} is not supervised by this build")]
    NotSupervisable {
        /// Agent identifier.
        id: String,
        /// Transport as written in the manifest.
        transport: &'static str,
    },

    /// The agent's process could not be started.
    ///
    /// `NotFound` is the common case: the command is not on `PATH` on the host
    /// the workspace lives on, which is a fact about the host rather than a
    /// malformed manifest.
    #[error("agent {id:?}: cannot spawn {command:?}: {source}")]
    Spawn {
        /// Agent identifier.
        id: String,
        /// Command that was attempted, rendered for a human.
        command: String,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },

    /// The process started but its stderr could not be pumped.
    #[error("agent {id:?}: cannot read stderr: {source}")]
    Stderr {
        /// Agent identifier.
        id: String,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },

    /// Waiting for the process failed.
    #[error("agent {id:?}: cannot wait for the process: {source}")]
    Wait {
        /// Agent identifier.
        id: String,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
}
