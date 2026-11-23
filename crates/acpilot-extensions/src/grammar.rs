//! A prebuilt tree-sitter grammar and its queries.
//!
//! The manifest points at a directory. Inside it, `manifest.toml` names the
//! language, its file extensions and the assets. The wasm is read as bytes and
//! kept for the editor's lazy loader. Nothing compiles here.

use serde::Deserialize;

use crate::error::ExtensionError;
use crate::manifest::validate_segment;
use crate::source::AssetSource;

/// The grammar manifest inside a contribution directory.
pub const GRAMMAR_MANIFEST: &str = "manifest.toml";
const DEFAULT_WASM: &str = "grammar.wasm";
const DEFAULT_HIGHLIGHTS: &str = "highlights.scm";
const DEFAULT_INDENTS: &str = "indents.scm";

/// One grammar contribution, ready for the editor's registry.
#[derive(Clone, PartialEq, Eq)]
pub struct GrammarAsset {
    /// The language name, which is also the wasm export's name.
    pub name: String,
    /// The extension that contributed it, for deterministic conflict reports.
    pub extension: String,
    /// File extensions this grammar handles, without the leading dot.
    pub extensions: Vec<String>,
    /// The prebuilt tree-sitter wasm module.
    pub wasm: Vec<u8>,
    /// The `highlights.scm` query.
    pub highlights: String,
    /// The optional `indents.scm` query.
    pub indents: Option<String>,
}

impl std::fmt::Debug for GrammarAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrammarAsset")
            .field("name", &self.name)
            .field("extension", &self.extension)
            .field("extensions", &self.extensions)
            .field("wasm_bytes", &self.wasm.len())
            .field("highlights_bytes", &self.highlights.len())
            .field("indents", &self.indents.as_ref().map(String::len))
            .finish()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrammarManifest {
    name: String,
    #[serde(default)]
    wasm: Option<String>,
    #[serde(default)]
    highlights: Option<String>,
    #[serde(default)]
    indents: Option<String>,
    extensions: Vec<String>,
}

/// Read a grammar directory.
///
/// `manifest.toml` is required. `wasm` defaults to `grammar.wasm` and
/// `highlights` to `highlights.scm`. A missing `indents.scm` is allowed.
///
/// # Errors
///
/// The manifest does not parse, a required asset is missing or empty, or an
/// extension names the same suffix twice.
pub fn parse_grammar(
    source: &dyn AssetSource,
    directory: &str,
    extension: &str,
) -> Result<GrammarAsset, ExtensionError> {
    let manifest_path = format!("{directory}/{GRAMMAR_MANIFEST}");
    let text = source.read_text(&manifest_path)?;
    let manifest: GrammarManifest =
        toml::from_str(&text).map_err(|error| ExtensionError::Toml {
            path: manifest_path,
            message: error.to_string(),
        })?;

    validate_segment("grammar name", &manifest.name)?;
    if manifest.extensions.is_empty() {
        return Err(ExtensionError::EmptyField {
            path: format!("{directory}/{GRAMMAR_MANIFEST}"),
            field: "extensions",
        });
    }

    let mut extensions = Vec::with_capacity(manifest.extensions.len());
    for raw in manifest.extensions {
        let suffix = raw.trim_start_matches('.').to_ascii_lowercase();
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(ExtensionError::InvalidId {
                what: "grammar extension",
                id: suffix,
                why: "must be letters and digits",
            });
        }
        if extensions.contains(&suffix) {
            return Err(ExtensionError::Duplicate {
                what: "grammar extension",
                id: suffix,
                owner: extension.to_owned(),
            });
        }
        extensions.push(suffix);
    }

    let wasm_name = manifest.wasm.unwrap_or_else(|| DEFAULT_WASM.to_owned());
    let wasm_path = format!("{directory}/{wasm_name}");
    let wasm = source.read(&wasm_path)?;
    if wasm.is_empty() {
        return Err(ExtensionError::EmptyField {
            path: wasm_path,
            field: "wasm",
        });
    }

    let highlights_name = manifest
        .highlights
        .unwrap_or_else(|| DEFAULT_HIGHLIGHTS.to_owned());
    let highlights_path = format!("{directory}/{highlights_name}");
    let highlights = source.read_text(&highlights_path)?;
    if highlights.trim().is_empty() {
        return Err(ExtensionError::EmptyField {
            path: highlights_path,
            field: "highlights",
        });
    }

    let indents = match manifest.indents {
        Some(name) => Some(source.read_text(&format!("{directory}/{name}"))?),
        None => {
            let path = format!("{directory}/{DEFAULT_INDENTS}");
            if source.exists(&path)? {
                Some(source.read_text(&path)?)
            } else {
                None
            }
        }
    };

    Ok(GrammarAsset {
        name: manifest.name,
        extension: extension.to_owned(),
        extensions,
        wasm,
        highlights,
        indents,
    })
}
