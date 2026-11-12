//! The configuration files a profile export may contain.
//!
//! The allow-list lives here so the daemon's API and the CLI's `profile export`
//! cannot disagree about which files belong in the document. Resolving keyring
//! references is deliberately not part of this module: that is the CLI's opt-in
//! path, and the HTTP answer must never contain a credential.

use std::collections::BTreeMap;
use std::fs;

use acpilot_agent::MANIFEST_FILE;

use crate::settings::PROFILE_SETTINGS_FILE;
use crate::{CONFIG_FILE, Profile};

/// Files a profile owns and an export includes when present.
///
/// This is an allow-list rather than a directory walk: a profile also holds its
/// database, locks and files a newer binary wrote, and "export everything" is how
/// an unexpected file becomes somebody else's copy.
pub const FILES: [&str; 8] = [
    CONFIG_FILE,
    PROFILE_SETTINGS_FILE,
    MANIFEST_FILE,
    // `acpilot-extensions` owns the loader and the constant; this crate does not
    // depend on it just to spell a file name that is part of the profile format.
    "activation.toml",
    "lsp.toml",
    "policy.toml",
    "keymap.toml",
    "theme.toml",
];

/// Read the present files in [`FILES`], keyed by their file name.
///
/// # Errors
///
/// A file exists and cannot be read.
pub fn files(profile: &Profile) -> Result<BTreeMap<String, String>, String> {
    let mut files = BTreeMap::new();
    for name in FILES {
        let path = profile.root().join(name);
        match fs::read_to_string(&path) {
            Ok(text) => {
                files.insert(name.to_owned(), text);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    Ok(files)
}
