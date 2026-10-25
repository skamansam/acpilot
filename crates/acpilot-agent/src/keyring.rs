//! The OS keyring backend, and the one place the namespace is decided.
//!
//! A manifest says `keyring:anthropic`; the keyring on the machine has to have an
//! account that means the same thing. This module owns that translation, so no
//! other code needs to know whether it is a service, an account prefix, or both:
//!
//! ```text
//! service  acpilot
//! account  <profile>/<name>          for keyring:<name>
//! account  <profile>/<name>          for keyring:<profile>/<name>
//! ```
//!
//! Both reference forms land on the same `<profile>/<name>` account, which is why
//! the docs can say copying a profile directory does not copy credentials and
//! that the same manifest line means different secrets in two profiles. The
//! cross-profile form is accepted here without a second decision: the manifest
//! already refused it unless the agent wrote `allow_cross_profile = true`.
//!
//! The store is behind [`CredentialStore`] rather than called directly. That is
//! not ceremony: it lets the namespace and every error path be tested on a
//! machine with no Secret Service, no Keychain, and no logged-in desktop, which
//! includes every CI runner the project builds on.

use std::fmt;

use ::keyring::Entry;

use crate::secret::{Secret, SecretError, SecretRef, SecretResolver};

/// Service name every Acpilot entry is filed under.
pub const SERVICE: &str = "acpilot";

/// Reads and writes credentials in a platform store.
///
/// `Ok(None)` is "this account has no entry", which is a different fact from
/// "the store could not be read": the first is a missing secret the user can add,
/// the second is a keyring that is locked or absent. Both fail the manifest load,
/// but only one of them is fixed by setting a secret.
pub trait CredentialStore: Send + Sync + fmt::Debug {
    /// Read `account` from `service`, if it is there.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String>;

    /// Write `value` to `account`, replacing any previous entry.
    fn set(&self, service: &str, account: &str, value: &str) -> Result<(), String>;

    /// Remove `account`. `Ok(false)` means there was nothing to remove, which is
    /// not a failure for a verb that asks the entry not to exist.
    fn delete(&self, service: &str, account: &str) -> Result<bool, String>;
}

/// The platform store: Keychain on macOS, Credential Manager on Windows, Secret
/// Service on unix.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeyring;

impl CredentialStore for OsKeyring {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        let entry = Entry::new(service, account)
            .map_err(|error| format!("cannot open the OS keyring entry: {error}"))?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(::keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(format!("cannot read the OS keyring: {error}")),
        }
    }

    fn set(&self, service: &str, account: &str, value: &str) -> Result<(), String> {
        let entry = Entry::new(service, account)
            .map_err(|error| format!("cannot open the OS keyring entry: {error}"))?;
        entry
            .set_password(value)
            .map_err(|error| format!("cannot write the OS keyring: {error}"))
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, String> {
        let entry = Entry::new(service, account)
            .map_err(|error| format!("cannot open the OS keyring entry: {error}"))?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(::keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(format!("cannot remove from the OS keyring: {error}")),
        }
    }
}

/// A [`SecretResolver`] backed by the OS keyring of one profile.
///
/// The profile is the directory the manifest lives in, and it is the namespace:
/// `work` and `personal` can both hold `anthropic` and get different values from
/// the same reference.
pub struct KeyringResolver {
    profile: String,
    store: Box<dyn CredentialStore>,
}

impl fmt::Debug for KeyringResolver {
    /// The profile is safe to print; the store is not asked to describe its
    /// contents.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyringResolver")
            .field("profile", &self.profile)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl KeyringResolver {
    /// Resolve against the platform store for `profile`.
    pub fn new(profile: impl Into<String>) -> Self {
        Self::with_store(profile, OsKeyring)
    }

    /// Resolve against an injected store. Tests use this; a caller that selects a
    /// different backend (a headless Linux box with a file store, say) can too.
    pub fn with_store(profile: impl Into<String>, store: impl CredentialStore + 'static) -> Self {
        Self {
            profile: profile.into(),
            store: Box::new(store),
        }
    }

    /// The profile this resolver speaks for.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The keyring account a reference names.
    ///
    /// Public so a status line or an error can say which entry to add without
    /// duplicating the rule.
    pub fn account(&self, reference: &SecretRef) -> String {
        match reference.profile() {
            Some(profile) => format!("{profile}/{}", reference.name()),
            None => format!("{}/{}", self.profile, reference.name()),
        }
    }

    /// Write `value` for `reference`, returning the account it was stored under.
    ///
    /// This is the write side of [`SecretResolver::resolve`], and it uses the
    /// same namespace rule: the CLI cannot store a secret in an account the
    /// resolver would not read.
    pub fn set(&self, reference: &SecretRef, value: &str) -> Result<String, SecretError> {
        let account = self.account(reference);
        self.store
            .set(SERVICE, &account, value)
            .map_err(|message| store_error(reference, &account, message))?;
        Ok(account)
    }

    /// Whether `reference` has a value in this store.
    ///
    /// Only presence is reported: the CLI's `list` can say "set" or "missing"
    /// without ever reading a value into its own output.
    pub fn has(&self, reference: &SecretRef) -> Result<bool, SecretError> {
        let account = self.account(reference);
        self.store
            .get(SERVICE, &account)
            .map(|found| found.is_some())
            .map_err(|message| store_error(reference, &account, message))
    }

    /// Remove `reference`; `Ok(false)` means it was already absent.
    pub fn remove(&self, reference: &SecretRef) -> Result<bool, SecretError> {
        let account = self.account(reference);
        self.store
            .delete(SERVICE, &account)
            .map_err(|message| store_error(reference, &account, message))
    }
}

/// Wrap a store failure with the account that was being written, so the user
/// knows which entry to inspect when the keyring is locked.
fn store_error(reference: &SecretRef, account: &str, message: String) -> SecretError {
    SecretError {
        reference: reference.reference().to_owned(),
        message: format!("{account}: {message}"),
    }
}

impl SecretResolver for KeyringResolver {
    fn resolve(&self, reference: &SecretRef) -> Result<Secret, SecretError> {
        let account = self.account(reference);
        let found = self
            .store
            .get(SERVICE, &account)
            .map_err(|message| SecretError {
                reference: reference.reference().to_owned(),
                message,
            })?;
        found.map(Secret::new).ok_or_else(|| SecretError {
            reference: reference.reference().to_owned(),
            message: format!("no entry for account {account:?} in the {SERVICE} keyring service"),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A store that answers from a map and remembers what it was asked for.
    #[derive(Debug, Default, Clone)]
    struct MemoryStore {
        entries: Arc<Mutex<BTreeMap<(String, String), String>>>,
        asked: Arc<Mutex<Vec<(String, String)>>>,
        broken: Option<String>,
    }

    impl MemoryStore {
        fn with(service: &str, account: &str, value: &str) -> Self {
            let store = Self::default();
            store
                .entries
                .lock()
                .expect("entries")
                .insert((service.to_owned(), account.to_owned()), value.to_owned());
            store
        }

        fn broken(message: &str) -> Self {
            Self {
                broken: Some(message.to_owned()),
                ..Self::default()
            }
        }

        fn asked(&self) -> Vec<(String, String)> {
            self.asked.lock().expect("asked").clone()
        }

        fn entry(&self, service: &str, account: &str) -> Option<String> {
            self.entries
                .lock()
                .expect("entries")
                .get(&(service.to_owned(), account.to_owned()))
                .cloned()
        }
    }

    impl CredentialStore for MemoryStore {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
            self.asked
                .lock()
                .expect("asked")
                .push((service.to_owned(), account.to_owned()));
            if let Some(message) = &self.broken {
                return Err(message.clone());
            }
            Ok(self.entry(service, account))
        }

        fn set(&self, service: &str, account: &str, value: &str) -> Result<(), String> {
            if let Some(message) = &self.broken {
                return Err(message.clone());
            }
            self.entries
                .lock()
                .expect("entries")
                .insert((service.to_owned(), account.to_owned()), value.to_owned());
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<bool, String> {
            if let Some(message) = &self.broken {
                return Err(message.clone());
            }
            Ok(self
                .entries
                .lock()
                .expect("entries")
                .remove(&(service.to_owned(), account.to_owned()))
                .is_some())
        }
    }

    fn reference(raw: &str) -> SecretRef {
        SecretRef::parse(raw).expect("valid reference")
    }

    #[test]
    fn a_local_reference_lands_in_the_profiles_namespace() {
        let store = MemoryStore::with(SERVICE, "work/anthropic", "sk-work");
        let resolver = KeyringResolver::with_store("work", store.clone());

        let secret = resolver
            .resolve(&reference("keyring:anthropic"))
            .expect("resolved");
        assert_eq!(secret.expose(), "sk-work");
        assert_eq!(
            store.asked(),
            [(SERVICE.to_owned(), "work/anthropic".to_owned())]
        );
    }

    #[test]
    fn the_same_line_means_different_secrets_in_two_profiles() {
        let store = MemoryStore::with(SERVICE, "work/anthropic", "sk-work");
        store
            .set(SERVICE, "personal/anthropic", "sk-personal")
            .expect("set personal");

        let work = KeyringResolver::with_store("work", store.clone());
        let personal = KeyringResolver::with_store("personal", store);
        assert_eq!(
            work.resolve(&reference("keyring:anthropic"))
                .expect("work")
                .expose(),
            "sk-work"
        );
        assert_eq!(
            personal
                .resolve(&reference("keyring:anthropic"))
                .expect("personal")
                .expose(),
            "sk-personal"
        );
    }

    #[test]
    fn a_cross_profile_reference_names_its_own_profile() {
        let store = MemoryStore::with(SERVICE, "personal/anthropic", "sk-personal");
        let resolver = KeyringResolver::with_store("work", store.clone());

        let secret = resolver
            .resolve(&reference("keyring:personal/anthropic"))
            .expect("resolved");
        assert_eq!(secret.expose(), "sk-personal");
        assert_eq!(
            store.asked(),
            [(SERVICE.to_owned(), "personal/anthropic".to_owned())]
        );
        assert_eq!(
            resolver.account(&reference("keyring:personal/anthropic")),
            "personal/anthropic"
        );
    }

    #[test]
    fn a_missing_entry_says_which_account_to_add() {
        let resolver = KeyringResolver::with_store("work", MemoryStore::default());
        let error = resolver
            .resolve(&reference("keyring:anthropic"))
            .expect_err("nothing stored");

        assert_eq!(error.reference, "keyring:anthropic");
        assert!(error.message.contains("work/anthropic"), "{error}");
        assert!(error.message.contains("acpilot"), "{error}");
    }

    #[test]
    fn a_store_failure_is_reported_as_a_store_failure() {
        let resolver =
            KeyringResolver::with_store("work", MemoryStore::broken("keyring is locked"));
        let error = resolver
            .resolve(&reference("keyring:anthropic"))
            .expect_err("store failed");

        assert!(error.message.contains("keyring is locked"), "{error}");
        assert!(!error.to_string().contains("sk-"), "{error}");
    }

    #[test]
    fn setting_and_removing_use_the_same_account_as_reading() {
        let store = MemoryStore::default();
        let resolver = KeyringResolver::with_store("work", store.clone());
        let reference = reference("keyring:anthropic");

        let account = resolver.set(&reference, "sk-work").expect("set");
        assert_eq!(account, "work/anthropic");
        assert_eq!(
            store.entry(SERVICE, "work/anthropic").as_deref(),
            Some("sk-work")
        );
        assert!(resolver.has(&reference).expect("has"));
        assert_eq!(
            resolver.resolve(&reference).expect("resolved").expose(),
            "sk-work",
            "a value written by the CLI is the value the manifest load reads"
        );

        assert!(resolver.remove(&reference).expect("remove"));
        assert!(!resolver.has(&reference).expect("has"));
    }

    #[test]
    fn removing_an_absent_secret_reports_it_instead_of_failing() {
        let resolver = KeyringResolver::with_store("work", MemoryStore::default());
        assert!(
            !resolver
                .remove(&reference("keyring:anthropic"))
                .expect("remove")
        );
    }

    #[test]
    fn a_write_failure_names_the_account_it_tried() {
        let resolver =
            KeyringResolver::with_store("work", MemoryStore::broken("keyring is locked"));
        let error = resolver
            .set(&reference("keyring:anthropic"), "sk-work")
            .expect_err("store failed");
        assert!(error.message.contains("work/anthropic"), "{error}");
        assert!(error.message.contains("keyring is locked"), "{error}");
        assert!(!error.to_string().contains("sk-work"), "{error}");
    }

    #[test]
    fn the_resolver_travels_between_threads_like_the_registry_needs() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<KeyringResolver>();
    }
}
