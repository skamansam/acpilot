//! A pack's key overrides, parsed but not applied.
//!
//! The key table itself is the client's: `Action` and `Chord` live in
//! `acpilot-tui`, and this crate cannot depend on it without a cycle. So the
//! loader checks the file's shape here — a `[keys]` table of action names to
//! lists of key names — and the client refuses a verb or a chord it does not
//! know. That keeps the format in one place (`keymap.toml`'s) while the set of
//! verbs stays a property of the program that answers them.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::ExtensionError;

/// One extension's key overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapLayer {
    /// The extension that contributed the layer, so an error can name it.
    pub extension: String,
    /// Action name to the keys that should mean it.
    pub keys: BTreeMap<String, Vec<String>>,
}

/// Parse a `keymap` contribution.
///
/// The shape is the one `keymap.toml` uses, minus `preset`: a layer moves keys,
/// it does not choose the editing model. A pack that could set `preset` would
/// silently turn a Vim user's modal editor modeless.
///
/// # Errors
///
/// The file is not TOML, is not a `[keys]` table of string lists, names a field
/// a layer may not set, or binds nothing at all.
pub fn parse_keymap_layer(
    path: &str,
    text: &str,
    extension: &str,
) -> Result<KeymapLayer, ExtensionError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct File {
        /// Defaulted so a file with no `[keys]` gets the sentence below rather
        /// than serde's "missing field", which does not say what to do.
        #[serde(default)]
        keys: BTreeMap<String, Vec<String>>,
    }

    let file: File = toml::from_str(text).map_err(|error| ExtensionError::Toml {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    if file.keys.is_empty() {
        return Err(ExtensionError::Toml {
            path: path.to_owned(),
            message: "a keymap layer with no keys; remove the contribution or name one".to_owned(),
        });
    }
    Ok(KeymapLayer {
        extension: extension.to_owned(),
        keys: file.keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layer_reads_the_keys_table() {
        let layer = parse_keymap_layer(
            "keys.toml",
            "[keys]\npalette = [\"f2\"]\nquit = []\n",
            "acpilot.vim-keys",
        )
        .expect("a layer");
        assert_eq!(layer.extension, "acpilot.vim-keys");
        assert_eq!(layer.keys["palette"], vec!["f2".to_owned()]);
        assert!(
            layer.keys["quit"].is_empty(),
            "an empty list is how a layer unbinds a key"
        );
    }

    #[test]
    fn a_layer_cannot_choose_the_preset() {
        let error = parse_keymap_layer(
            "keys.toml",
            "preset = \"vscode\"\n[keys]\nquit = [\"q\"]\n",
            "acpilot.vim-keys",
        )
        .expect_err("a layer is not the file");
        assert!(error.to_string().contains("preset"), "{error}");
    }

    #[test]
    fn an_empty_layer_is_refused() {
        let error = parse_keymap_layer("keys.toml", "", "acpilot.vim-keys")
            .expect_err("an empty contribution is a mistake");
        assert!(error.to_string().contains("no keys"), "{error}");
    }
}
