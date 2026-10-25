//! Domain model for Acpilot.
//!
//! This crate holds what the product *means* — workspaces, sessions, agents,
//! permissions — with no HTTP, no protocol types, and no UI. It must stay
//! dependency-light: if a type here needs a runtime to be useful, it belongs in a
//! layer above.
//!
//! The engine phase (P2) fills in the domain types. The foundation phase (P1) establishes the error contract every layer
//! shares, because exit codes and HTTP statuses are a promise to users and
//! scripts, not an implementation detail.

pub mod commit;
pub mod error;
pub mod stats;

pub use error::AcpilotError;

/// The version this build reports, everywhere it reports one.
///
/// A release is a tag, and the tag is decided by the labels on the pull request
/// (`.github/scripts/next_version.py`) — not by a field in `Cargo.toml` that
/// somebody has to remember to bump. The release workflow sets
/// `ACPILOT_RELEASE_VERSION` when it builds, so a binary says the version it was
/// published as; a local `cargo build` has no such variable and falls back to the
/// manifest, which is the truth about that binary.
///
/// One constant rather than `env!("CARGO_PKG_VERSION")` at each call site, so
/// `acpilot --version`, `acpilot version`, `acpilotd --version` and the About
/// window cannot disagree about what they are.
pub const VERSION: &str = match option_env!("ACPILOT_RELEASE_VERSION") {
    Some(release) => release,
    None => env!("CARGO_PKG_VERSION"),
};

#[cfg(test)]
mod tests {
    #[test]
    fn the_version_is_a_plain_semver_core() {
        // Not a full SemVer parse: the point is that a release tag (`0.4.2`) and a
        // local manifest (`0.1.0`) both answer this way, so nothing downstream has
        // to handle a `v` prefix or a build suffix.
        assert!(!super::VERSION.is_empty());
        assert!(
            super::VERSION
                .split('.')
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())),
            "{} is not X.Y.Z",
            super::VERSION
        );
    }
}
