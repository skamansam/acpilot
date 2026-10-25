//! `acpilot secret` — the OS keyring, through the profile's namespace.
//!
//! The verbs are filesystem-free but not daemon-backed: a machine's credentials
//! have to be repairable when `acpilotd` is not running. The namespace rule is
//! [`KeyringResolver`]'s, so a value this command writes is exactly the value a
//! manifest load reads: service `acpilot`, account `<profile>/<name>`.
//!
//! Three properties are deliberate:
//!
//! * **The value is never a command-line argument.** It is read from stdin, so it
//!   does not appear in shell history, in `ps`, or in an error message. Nothing
//!   here prints a stored value; `list` prints presence only.
//! * **`list` lists what the profile references**, read from its `agents.toml`
//!   with a resolver that records references instead of fetching them. The
//!   generic keyring API cannot enumerate a store, so the manifest is the honest
//!   source of "which entries should exist"; the keyring answers "do they".
//! * **`rm` of something absent is `not found`, not success.** A script that
//!   removes a credential should be able to tell that there was nothing there.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use acpilot_agent::keyring::{KeyringResolver, SERVICE};
use acpilot_agent::manifest::{self, MANIFEST_FILE};
use acpilot_agent::secret::{SCHEME, Secret, SecretError, SecretRef, SecretResolver};

/// Parse a CLI name as a reference.
///
/// Both `anthropic` (this profile) and `personal/anthropic` (an explicit
/// profile) are accepted, with or without the `keyring:` prefix, because a name
/// copied out of a manifest should not have to be edited to be usable.
pub fn reference(name: &str) -> Result<SecretRef, String> {
    let raw = if name.starts_with(SCHEME) {
        name.to_owned()
    } else {
        format!("{SCHEME}{name}")
    };
    SecretRef::parse(&raw).map_err(|error| error.to_string())
}

/// Read one value from `reader`, stripping the newline a shell adds.
///
/// Empty input is refused: an empty credential is far more likely to be a
/// mistyped pipe than a key that happens to be zero bytes, and the keyring would
/// happily store it and fail later, at a turn.
pub fn read_value(mut reader: impl Read) -> Result<String, String> {
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .map_err(|error| format!("cannot read the secret from stdin: {error}"))?;
    let value = text.strip_suffix('\n').map_or(text.as_str(), |line| {
        line.strip_suffix('\r').unwrap_or(line)
    });
    if value.is_empty() {
        return Err("the secret from stdin is empty".to_owned());
    }
    Ok(value.to_owned())
}

/// Store `value` under `reference`; returns the account it was written to.
pub fn set(
    resolver: &KeyringResolver,
    reference: &SecretRef,
    value: &str,
) -> Result<String, String> {
    if value.is_empty() {
        return Err("the secret may not be empty".to_owned());
    }
    resolver
        .set(reference, value)
        .map_err(|error| error.to_string())
}

/// Remove `reference`; `Ok(false)` means there was no entry.
pub fn remove(resolver: &KeyringResolver, reference: &SecretRef) -> Result<bool, String> {
    resolver
        .remove(reference)
        .map_err(|error| error.to_string())
}

/// List the accounts this profile's `agents.toml` references, and whether each
/// is set.
///
/// The manifest is parsed with a resolver that records references and returns a
/// placeholder, so validation (including the cross-profile rule) still runs; no
/// secret is fetched. A missing manifest is reported as one line rather than an
/// error, because a new profile legitimately has nothing to list.
pub fn list(profile_dir: &Path, resolver: &KeyringResolver) -> Result<Vec<String>, String> {
    let path = profile_dir.join(MANIFEST_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(vec![format!(
                "no {MANIFEST_FILE} for this profile at {}",
                path.display()
            )]);
        }
        Err(error) => {
            return Err(format!("{}: {error}", path.display()));
        }
    };

    let recorder = RecordingResolver::default();
    manifest::parse(&path, &text, &recorder)
        .map_err(|error| format!("{}: {error}", path.display()))?;

    let mut seen = BTreeSet::new();
    let mut lines = Vec::new();
    for reference in recorder.references() {
        let account = resolver.account(&reference);
        if !seen.insert(account.clone()) {
            continue;
        }
        let present = resolver
            .has(&reference)
            .map_err(|error| error.to_string())?;
        let state = if present { "present" } else { "missing" };
        lines.push(format!("{state:<8} {account}"));
    }

    if lines.is_empty() {
        lines.push(format!("{} references no keyring secrets", path.display()));
    }
    Ok(lines)
}

/// A [`SecretResolver`] that records which references the manifest asks for.
#[derive(Debug, Default)]
struct RecordingResolver {
    references: Mutex<Vec<SecretRef>>,
}

impl RecordingResolver {
    fn references(&self) -> Vec<SecretRef> {
        self.references
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl SecretResolver for RecordingResolver {
    fn resolve(&self, reference: &SecretRef) -> Result<Secret, SecretError> {
        self.references
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(reference.clone());
        // Never exposed: `list` asks only whether an entry exists.
        Ok(Secret::new(String::new()))
    }
}

/// The profile directory, without resolving the extension store.
///
/// `Paths::resolve` also finds the store root, which these verbs never touch;
/// failing to find an extension store must not stop someone from storing a
/// credential.
pub fn profile_dir(config: Option<&PathBuf>, profile: &str) -> Result<PathBuf, String> {
    let root = match config {
        Some(path) => path.clone(),
        None => acpilot_extensions::activation::config_root().map_err(|error| error.to_string())?,
    };
    acpilot_extensions::activation::profile_dir_in(&root, profile)
        .map_err(|error| error.to_string())
}

/// The service name, for messages.
pub fn service() -> &'static str {
    SERVICE
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use acpilot_agent::keyring::CredentialStore;

    use super::*;

    #[derive(Debug, Default, Clone)]
    struct MemoryStore {
        entries: Arc<Mutex<BTreeMap<(String, String), String>>>,
    }

    impl MemoryStore {
        fn entry(&self, account: &str) -> Option<String> {
            self.entries
                .lock()
                .expect("entries")
                .get(&(SERVICE.to_owned(), account.to_owned()))
                .cloned()
        }
    }

    impl CredentialStore for MemoryStore {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
            Ok(self
                .entries
                .lock()
                .expect("entries")
                .get(&(service.to_owned(), account.to_owned()))
                .cloned())
        }

        fn set(&self, service: &str, account: &str, value: &str) -> Result<(), String> {
            self.entries
                .lock()
                .expect("entries")
                .insert((service.to_owned(), account.to_owned()), value.to_owned());
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<bool, String> {
            Ok(self
                .entries
                .lock()
                .expect("entries")
                .remove(&(service.to_owned(), account.to_owned()))
                .is_some())
        }
    }

    fn temp_profile(manifest: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(MANIFEST_FILE), manifest).expect("manifest");
        dir
    }

    #[test]
    fn a_bare_name_is_a_reference_in_this_profile() {
        let reference = reference("anthropic").expect("valid");
        assert_eq!(reference.name(), "anthropic");
        assert_eq!(reference.profile(), None);
    }

    #[test]
    fn a_prefixed_or_cross_profile_name_is_the_same_reference() {
        assert_eq!(
            reference("keyring:anthropic").expect("valid").name(),
            "anthropic"
        );
        let cross = reference("personal/anthropic").expect("valid");
        assert_eq!(cross.profile(), Some("personal"));
        assert_eq!(cross.name(), "anthropic");
    }

    #[test]
    fn a_malformed_name_is_refused_before_it_reaches_the_keyring() {
        assert!(reference("work/api key").is_err());
        assert!(reference("").is_err());
    }

    #[test]
    fn set_writes_the_profiles_account_and_remove_takes_it_away() {
        let store = MemoryStore::default();
        let resolver = KeyringResolver::with_store("work", store.clone());
        let reference = reference("anthropic").expect("valid");

        let account = set(&resolver, &reference, "sk-work").expect("set");
        assert_eq!(account, "work/anthropic");
        assert_eq!(store.entry("work/anthropic").as_deref(), Some("sk-work"));

        assert!(remove(&resolver, &reference).expect("remove"));
        assert_eq!(store.entry("work/anthropic"), None);
        assert!(!remove(&resolver, &reference).expect("already gone"));
    }

    #[test]
    fn set_refuses_an_empty_value() {
        let resolver = KeyringResolver::with_store("work", MemoryStore::default());
        let reference = reference("anthropic").expect("valid");
        assert!(set(&resolver, &reference, "").is_err());
    }

    #[test]
    fn a_value_read_from_stdin_loses_only_the_shells_newline() {
        assert_eq!(
            read_value("sk-test\n".as_bytes()).expect("value"),
            "sk-test"
        );
        assert_eq!(
            read_value("sk-with-space \n".as_bytes()).expect("value"),
            "sk-with-space ",
            "spaces inside a secret are part of it"
        );
        assert_eq!(
            read_value("line1\nline2\n".as_bytes()).expect("value"),
            "line1\nline2",
            "a multi-line key is stored as given; only the last newline is a shell artefact"
        );
    }

    #[test]
    fn an_empty_stdin_is_an_error_not_an_empty_secret() {
        assert!(read_value("".as_bytes()).is_err());
        assert!(read_value("\n".as_bytes()).is_err());
    }

    #[test]
    fn list_reports_presence_and_never_a_value() {
        let dir = temp_profile(
            "schema_version = 1\n\
             [[agents]]\nid = \"claude\"\ntransport = \"acp-stdio\"\ncommand = \"claude\"\n\
             env = { ANTHROPIC_API_KEY = \"keyring:anthropic\" }\n\
             [[agents]]\nid = \"opencode\"\ntransport = \"acp-stdio\"\ncommand = \"opencode\"\n\
             env = { OPENAI_API_KEY = \"keyring:openai\" }\n",
        );
        let store = MemoryStore::default();
        store
            .set(SERVICE, "work/anthropic", "sk-secret-value")
            .expect("seed");
        let resolver = KeyringResolver::with_store("work", store);

        let lines = list(dir.path(), &resolver).expect("list");
        assert_eq!(
            lines,
            vec!["present  work/anthropic", "missing  work/openai"]
        );
        let printed = lines.join("\n");
        assert!(!printed.contains("sk-secret-value"));
    }

    #[test]
    fn list_deduplicates_an_entry_two_agents_share() {
        let dir = temp_profile(
            "schema_version = 1\n\
             [[agents]]\nid = \"a\"\ntransport = \"acp-stdio\"\ncommand = \"a\"\n\
             env = { A = \"keyring:anthropic\", B = \"keyring:anthropic\" }\n\
             [[agents]]\nid = \"b\"\ntransport = \"acp-stdio\"\ncommand = \"b\"\n\
             env = { C = \"keyring:anthropic\" }\n",
        );
        let resolver = KeyringResolver::with_store("work", MemoryStore::default());
        let lines = list(dir.path(), &resolver).expect("list");
        assert_eq!(lines, vec!["missing  work/anthropic"]);
    }

    #[test]
    fn list_without_a_manifest_says_so_instead_of_failing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let resolver = KeyringResolver::with_store("work", MemoryStore::default());
        let lines = list(dir.path(), &resolver).expect("list");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(MANIFEST_FILE), "{lines:?}");
    }

    #[test]
    fn list_reports_a_manifest_the_loader_would_refuse() {
        let dir = temp_profile("schema_version = 1\n");
        let resolver = KeyringResolver::with_store("work", MemoryStore::default());
        let error = list(dir.path(), &resolver).expect_err("no agents is invalid");
        assert!(error.contains(MANIFEST_FILE), "{error}");
    }
}
