//! One error type for every way a manifest, an asset, or a theme can be wrong.
//!
//! Variants are typed rather than stringly on purpose: the CLI renders these,
//! and "this extension is blocked" has to be a decision in code, not a pattern
//! match on a message. Every variant names the file it came from.

use std::io;

use thiserror::Error;

/// What went wrong while reading an extension.
#[derive(Debug, Error)]
pub enum ExtensionError {
    /// A file could not be read.
    #[error("{path}: {source}")]
    Io {
        /// Path, as a human should see it.
        path: String,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },

    /// A TOML file did not parse, or carried a key the schema does not know.
    #[error("{path}: {message}")]
    Toml {
        /// Path, relative to the extension root.
        path: String,
        /// The parser's message, which already names the key and the line.
        message: String,
    },

    /// The file was written for a schema this build does not speak.
    #[error("{path}: schema_version {found} is not supported (this build speaks {expected})")]
    SchemaVersion {
        /// Path, relative to the extension root.
        path: String,
        /// Version found in the file.
        found: u64,
        /// Version this build understands.
        expected: u32,
    },

    /// The extension needs a newer acpilot than the one running it.
    #[error("{path}: requires acpilot {required} or newer; this build is {current}")]
    NeedsNewerAcpilot {
        /// Path, relative to the extension root.
        path: String,
        /// The manifest's `min_acpilot`.
        required: String,
        /// The running version.
        current: &'static str,
    },

    /// An identifier is not the shape this registry requires.
    #[error("{what} {id:?} {why}")]
    InvalidId {
        /// Which namespace: `extension id`, `theme id`, `preset id`.
        what: &'static str,
        /// The offending value.
        id: String,
        /// What the rule is, in a few words.
        why: &'static str,
    },

    /// A required field is present but empty, which is never what was meant.
    #[error("{path}: {field} may not be empty")]
    EmptyField {
        /// Path, relative to the extension root.
        path: String,
        /// Field name as written in the file.
        field: &'static str,
    },

    /// Two things claim the same identifier. Never resolved by ordering.
    #[error("duplicate {what} {id:?}: already provided by {owner}")]
    Duplicate {
        /// Which registry.
        what: &'static str,
        /// The duplicated identifier.
        id: String,
        /// Who had it first, in terms a human can act on.
        owner: String,
    },

    /// The same contribution is listed twice in one manifest.
    #[error("{path}: {kind} contribution {contribution:?} is listed twice")]
    DuplicateContribution {
        /// Path, relative to the extension root.
        path: String,
        /// Contribution kind, as written in the manifest.
        kind: &'static str,
        /// Contribution path.
        contribution: String,
    },

    /// A contribution path points outside the extension directory.
    #[error("contribution path {path:?} {why}")]
    UnsafePath {
        /// The offending path.
        path: String,
        /// Why it is refused.
        why: &'static str,
    },

    /// A manifest pointed at a file that is not there.
    #[error("{path}: no {kind} contribution at this path")]
    MissingContribution {
        /// The path the manifest named.
        path: String,
        /// Contribution kind, as written in the manifest.
        kind: &'static str,
    },

    /// A theme is incomplete, or its `extends` chain is.
    #[error("theme {id}: {message}")]
    Theme {
        /// The theme's identifier, or the file path when the id is unreadable.
        id: String,
        /// What is wrong, with the missing token or the cycle named.
        message: String,
    },
}
