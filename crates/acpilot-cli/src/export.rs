//! `acpilot profile export` — one profile's configuration in one file.
//!
//! The files keep keyring references exactly as they were written, so the default
//! document contains no credential and can be read, diffed or committed. The
//! optional `secrets` map is separate on purpose: resolving a reference into a
//! manifest would turn a reference into a plaintext credential, which is the one
//! shape this product refuses everywhere else.
//!
//! A cross-profile reference is refused rather than exported. The flag that
//! permits the agent to read it is an opt-in for use, not permission to copy
//! another profile's credential into a file.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use acpilot_agent::SecretResolver;
use acpilot_agent::keyring::KeyringResolver;
use acpilot_host::private_file;
use acpilot_profile::Profile;
use serde_json::Value;

use crate::secret;

/// The export document's schema version.
const SCHEMA_VERSION: u32 = 1;

/// Build the export document.
///
/// With `with_secrets`, every local keyring reference named by the profile's
/// agents is resolved into a `secrets` map keyed by its account. A cross-profile
/// reference fails the export; it belongs to the other profile.
pub fn document(
    profile: &Profile,
    with_secrets: bool,
    resolver: &KeyringResolver,
) -> Result<String, String> {
    let id = profile.id().ok_or_else(|| {
        format!(
            "{} is not a named profile; an export needs a keyring namespace",
            profile.root().display()
        )
    })?;

    let files = acpilot_profile::export::files(profile)?;
    let mut document = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "profile": id.as_str(),
        "files": files,
    });

    if with_secrets {
        let secrets = secret_map(profile, resolver)?;
        if let Value::Object(object) = &mut document {
            object.insert("secrets".to_owned(), serde_json::json!(secrets));
        }
    }

    let mut text = serde_json::to_string_pretty(&document)
        .map_err(|error| format!("cannot encode the export: {error}"))?;
    text.push('\n');
    Ok(text)
}

/// Resolve the profile's own references into account-to-value pairs.
fn secret_map(
    profile: &Profile,
    resolver: &KeyringResolver,
) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    for reference in secret::references(profile.root())? {
        if reference.cross_profile() {
            return Err(format!(
                "{} belongs to profile `{}`; a profile export cannot include another profile's \
                 secret",
                reference.reference(),
                reference.profile().unwrap_or("unknown")
            ));
        }
        let value = resolver
            .resolve(&reference)
            .map_err(|error| error.to_string())?;
        values.insert(resolver.account(&reference), value.expose().to_owned());
    }
    Ok(values)
}

/// Write `document` to `path`.
///
/// A secret-bearing export refuses an existing path and is made owner-only before
/// any bytes are written. A redacted export may replace an existing file, because
/// it contains exactly what the committed configuration already contains.
pub fn write(document: &str, path: &Path, with_secrets: bool) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true);
    if with_secrets {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if with_secrets && let Err(error) = private_file(path) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(format!("cannot make {} private: {error}", path.display()));
    }
    file.write_all(document.as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use acpilot_agent::MANIFEST_FILE;
    use acpilot_agent::keyring::{CredentialStore, SERVICE};
    use acpilot_profile::CONFIG_FILE;
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug, Default, Clone)]
    struct MemoryStore {
        entries: Arc<Mutex<BTreeMap<(String, String), String>>>,
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

    fn profile(manifest: &str) -> (TempDir, Profile) {
        let root = tempfile::tempdir().expect("temp dir");
        let directory = root.path().join("work");
        fs::create_dir_all(&directory).expect("profile directory");
        fs::write(directory.join(CONFIG_FILE), "schema_version = 1\n").expect("config");
        fs::write(directory.join(MANIFEST_FILE), manifest).expect("manifest");
        let profile = Profile::open(&directory).expect("profile");
        (root, profile)
    }

    fn manifest(reference: &str) -> String {
        format!(
            "schema_version = 1\n\n[[agents]]\nid = \"claude\"\ntransport = \"acp-stdio\"\n\
             command = \"npx\"\nenv = {{ ANTHROPIC_API_KEY = \"{reference}\" }}\n"
        )
    }

    fn resolver(store: MemoryStore) -> KeyringResolver {
        KeyringResolver::with_store("work", store)
    }

    #[test]
    fn the_default_export_keeps_a_secret_as_a_reference() {
        let (_root, profile) = profile(&manifest("keyring:anthropic"));
        let store = MemoryStore::default();
        store
            .set(SERVICE, "work/anthropic", "sk-work")
            .expect("store");
        let document = document(&profile, false, &resolver(store)).expect("export");

        assert!(document.contains("keyring:anthropic"), "{document}");
        assert!(!document.contains("sk-work"), "{document}");
        assert!(!document.contains("\"secrets\""), "{document}");
    }

    #[test]
    fn with_secrets_puts_the_value_in_a_separate_map() {
        let (_root, profile) = profile(&manifest("keyring:anthropic"));
        let store = MemoryStore::default();
        store
            .set(SERVICE, "work/anthropic", "sk-work")
            .expect("store");
        let document = document(&profile, true, &resolver(store)).expect("export");

        assert!(document.contains("keyring:anthropic"), "{document}");
        assert!(document.contains("work/anthropic"), "{document}");
        assert!(document.contains("sk-work"), "{document}");
    }

    #[test]
    fn another_profiles_secret_is_not_exported() {
        let (_root, profile) = profile(
            "schema_version = 1\n\n[[agents]]\nid = \"claude\"\ntransport = \"acp-stdio\"\n\
             command = \"npx\"\nallow_cross_profile = true\n\
             env = { ANTHROPIC_API_KEY = \"keyring:personal/anthropic\" }\n",
        );
        let message = document(&profile, true, &resolver(MemoryStore::default()))
            .expect_err("a cross-profile reference is refused");

        assert!(message.contains("personal/anthropic"), "{message}");
        assert!(message.contains("another profile"), "{message}");
    }

    #[test]
    fn a_secret_export_never_overwrites_a_file() {
        let (_root, profile) = profile(&manifest("keyring:anthropic"));
        let store = MemoryStore::default();
        store
            .set(SERVICE, "work/anthropic", "sk-work")
            .expect("store");
        let document = document(&profile, true, &resolver(store)).expect("export");
        let output = tempfile::tempdir().expect("temp dir");
        let path = output.path().join("work.json");

        write(&document, &path, true).expect("first write");
        assert!(
            write(&document, &path, true).is_err(),
            "the second write must refuse"
        );
        assert_eq!(fs::read_to_string(&path).expect("written"), document);
    }
}
