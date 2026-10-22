//! Secrets: a manifest names a keyring entry, and only this module turns that
//! name into a value.
//!
//! Two rules from `site/.../safety/secrets.md` are enforced by the types here.
//!
//! **A secret cannot be printed.** [`Secret`]'s `Debug` and `Display` are
//! `<redacted>`, so an [`AgentSpec`](crate::AgentSpec) can be logged, debugged
//! or quoted in an error message without leaking one. `expose` is the one way
//! out, and it is called at exactly one place: the spawn that puts the value in
//! a child's environment.
//!
//! **A plaintext credential is refused.** Where a field *is* a credential
//! (`auth` on an HTTP agent), it must parse as a `keyring:` reference. `env` is
//! deliberately not strict — it also carries ordinary variables such as
//! `NO_COLOR=1` — but a value that starts with `keyring:` is always a reference
//! and never a literal, so a secret cannot become a literal by accident.
//!
//! The namespace (`acpilot/<profile>/<name>`) is not decided here. A
//! [`SecretResolver`] owns that, which is what lets P4-5 gain a keyring backend
//! without this crate learning where a profile lives.

use std::collections::BTreeMap;
use std::fmt;

use thiserror::Error;

/// Prefix that marks an env or credential value as a reference.
pub const SCHEME: &str = "keyring:";

/// The value of a secret once it has been resolved.
///
/// Deliberately opaque: the only way to read it is [`Secret::expose`], and it
/// prints as `<redacted>` so it cannot reach a log by being interpolated into
/// one.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap an already-resolved value.
    ///
    /// For resolvers, and for the literal env values a manifest may carry. It
    /// does not check anything: by the time a value is a `Secret`, the decision
    /// to trust its source has already been made.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value itself. The only path from a keyring to a child process.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A parsed `keyring:` reference.
///
/// Two forms, and the second one is not free:
///
/// - `keyring:<name>` resolves in the profile the manifest belongs to.
/// - `keyring:<profile>/<name>` reaches into another profile, and a manifest
///   must say `allow_cross_profile = true` before the registry will accept it
///   (P4-5). The point of the flag is that mixing identities is the failure this
///   product exists to prevent, so it has to be a sentence somebody wrote down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    /// The reference exactly as written, `keyring:` included.
    raw: String,
    /// The profile part of a cross-profile reference.
    profile: Option<String>,
    /// The entry name inside the profile.
    name: String,
}

impl SecretRef {
    /// Whether a value is a reference rather than a literal.
    pub fn is_reference(raw: &str) -> bool {
        raw.starts_with(SCHEME)
    }

    /// Parse `keyring:<name>` or `keyring:<profile>/<name>`.
    pub fn parse(raw: &str) -> Result<Self, SecretError> {
        let Some(account) = raw.strip_prefix(SCHEME) else {
            return Err(SecretError {
                reference: raw.to_owned(),
                message: format!("not a reference: it must start with {SCHEME:?}"),
            });
        };

        let (profile, name) = match account.split_once('/') {
            Some((profile, name)) => {
                validate_account_part(raw, "profile", profile)?;
                (Some(profile.to_owned()), name)
            }
            None => (None, account),
        };
        validate_account_part(raw, "name", name)?;

        Ok(Self {
            raw: raw.to_owned(),
            profile,
            name: name.to_owned(),
        })
    }

    /// The reference as written, for messages and for storing it back.
    pub fn reference(&self) -> &str {
        &self.raw
    }

    /// The account string after the scheme: `<profile>/<name>` or `<name>`.
    ///
    /// This is what a resolver looks up, so a resolver decides the namespace
    /// (`acpilot` service, `<profile>/<name>` account) in one place.
    pub fn account(&self) -> &str {
        &self.raw[SCHEME.len()..]
    }

    /// The profile this reference names, when it names one explicitly.
    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    /// The entry name inside the profile.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this reference leaves the manifest's own profile.
    pub fn cross_profile(&self) -> bool {
        self.profile.is_some()
    }
}

/// One account part: a profile name or an entry name.
///
/// Loose on purpose — keyring account names are opaque strings, and a provider
/// key may be spelled `gpt-5.key` — but narrow enough to survive every backend
/// (no whitespace, no separator that would make `<profile>/<name>` ambiguous).
fn validate_account_part(raw: &str, what: &'static str, part: &str) -> Result<(), SecretError> {
    let invalid = |why: &str| SecretError {
        reference: raw.to_owned(),
        message: format!("{what} {part:?} {why}"),
    };

    if part.is_empty() {
        return Err(invalid("is empty"));
    }
    if part.len() > 64 {
        return Err(invalid("is longer than 64 characters"));
    }
    if !part.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(invalid("must start with a letter or digit"));
    }
    if !part
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(invalid(
            "may only contain letters, digits, `-`, `_`, and `.`",
        ));
    }
    Ok(())
}

/// Why a reference could not be read.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{reference}: {message}")]
pub struct SecretError {
    /// The reference as written, so the message can be pasted into a bug report.
    pub reference: String,
    /// What is wrong, in one sentence.
    pub message: String,
}

/// Turns a reference into a value.
///
/// The registry calls this once per reference while it loads a manifest, which
/// is what makes a missing credential a load error instead of a failure in the
/// middle of a turn. The backend is injected: P4-5 supplies the OS keyring, and
/// tests supply [`MapResolver`].
pub trait SecretResolver: Send + Sync {
    /// Resolve one reference, or say why not.
    fn resolve(&self, reference: &SecretRef) -> Result<Secret, SecretError>;
}

/// A resolver over an in-memory map, keyed by [`SecretRef::account`].
///
/// Used by tests, and by any caller that already holds the value. It is not the
/// keyring: nothing here is persisted, and it exists so that the registry can be
/// tested without a secret service on the machine.
#[derive(Debug, Default, Clone)]
pub struct MapResolver {
    entries: BTreeMap<String, String>,
}

impl MapResolver {
    /// An empty resolver.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one entry, keyed by the account string (`name` or `profile/name`).
    #[must_use]
    pub fn with(mut self, account: &str, value: &str) -> Self {
        self.entries.insert(account.to_owned(), value.to_owned());
        self
    }
}

impl SecretResolver for MapResolver {
    fn resolve(&self, reference: &SecretRef) -> Result<Secret, SecretError> {
        self.entries
            .get(reference.account())
            .map(Secret::new)
            .ok_or_else(|| SecretError {
                reference: reference.reference().to_owned(),
                message: "no entry for this account".to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_prints_its_value() {
        let secret = Secret::new("sk-live-1234");
        assert_eq!(format!("{secret:?}"), "<redacted>");
        assert_eq!(format!("{secret}"), "<redacted>");
        assert!(!format!("{secret:?}").contains("sk-live"));
        assert_eq!(secret.expose(), "sk-live-1234");
    }

    #[test]
    fn parses_both_reference_forms() {
        let local = SecretRef::parse("keyring:anthropic").expect("local");
        assert_eq!(local.account(), "anthropic");
        assert_eq!(local.name(), "anthropic");
        assert_eq!(local.profile(), None);
        assert!(!local.cross_profile());

        let remote = SecretRef::parse("keyring:work/anthropic").expect("cross-profile");
        assert_eq!(remote.account(), "work/anthropic");
        assert_eq!(remote.name(), "anthropic");
        assert_eq!(remote.profile(), Some("work"));
        assert!(remote.cross_profile());
        assert_eq!(remote.reference(), "keyring:work/anthropic");
    }

    #[test]
    fn refuses_references_that_cannot_be_an_account() {
        for raw in [
            "anthropic",          // no scheme
            "keyring:",           // nothing to look up
            "keyring:/anthropic", // empty profile
            "keyring:work/",      // empty name
            "keyring:work/an/thropic",
            "keyring:has space",
            "keyring:-leading",
            "keyring:../escape",
        ] {
            assert!(SecretRef::parse(raw).is_err(), "{raw} should be refused");
        }
    }

    #[test]
    fn marks_the_scheme_and_nothing_else() {
        assert!(SecretRef::is_reference("keyring:anthropic"));
        assert!(!SecretRef::is_reference("anthropic"));
        assert!(!SecretRef::is_reference("sk-live-1234"));
    }

    #[test]
    fn map_resolver_reports_a_missing_account() {
        let resolver = MapResolver::new().with("work/anthropic", "sk-work");
        let found = SecretRef::parse("keyring:work/anthropic").expect("parse");
        let missing = SecretRef::parse("keyring:personal").expect("parse");

        assert_eq!(resolver.resolve(&found).expect("found").expose(), "sk-work");
        let error = resolver.resolve(&missing).expect_err("missing");
        assert_eq!(error.reference, "keyring:personal");
    }
}
