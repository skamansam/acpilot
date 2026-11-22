//! Per-language snippets from an extension.
//!
//! The file is JSON. The loader checks names and prefixes but does not expand a
//! body. The editor owns insertion and tab stops.

use serde::Deserialize;

use crate::error::ExtensionError;
use crate::manifest::validate_segment;
use crate::source::AssetSource;

/// One language's snippets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetSet {
    /// The language id the snippets apply to.
    pub language: String,
    /// The extension that contributed them.
    pub extension: String,
    /// Snippets in file order.
    pub snippets: Vec<Snippet>,
}

/// One snippet template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// The word a person types to trigger it.
    pub prefix: String,
    /// The body lines, joined by newlines when inserted.
    pub body: Vec<String>,
    /// An optional sentence shown by a picker.
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnippetFile {
    language: String,
    snippets: Vec<SnippetEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnippetEntry {
    prefix: String,
    body: Vec<String>,
    #[serde(default)]
    description: Option<String>,
}

/// Read a snippet contribution.
///
/// # Errors
///
/// The JSON does not parse, a required field is empty, a prefix repeats, or a
/// value is not the shape the registry accepts.
pub fn parse_snippets(
    source: &dyn AssetSource,
    path: &str,
    extension: &str,
) -> Result<SnippetSet, ExtensionError> {
    let text = source.read_text(path)?;
    let file: SnippetFile = serde_json::from_str(&text).map_err(|error| ExtensionError::Json {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    validate_segment("snippet language", &file.language)?;
    if file.snippets.is_empty() {
        return Err(ExtensionError::EmptyField {
            path: path.to_owned(),
            field: "snippets",
        });
    }

    let mut snippets = Vec::with_capacity(file.snippets.len());
    for entry in file.snippets {
        if entry.prefix.is_empty() || entry.prefix.chars().any(char::is_whitespace) {
            return Err(ExtensionError::InvalidId {
                what: "snippet prefix",
                id: entry.prefix,
                why: "must be non-empty and contain no spaces",
            });
        }
        if entry.body.is_empty() || entry.body.iter().all(|line| line.is_empty()) {
            return Err(ExtensionError::EmptyField {
                path: path.to_owned(),
                field: "snippet body",
            });
        }
        if snippets
            .iter()
            .any(|snippet: &Snippet| snippet.prefix == entry.prefix)
        {
            return Err(ExtensionError::Duplicate {
                what: "snippet prefix",
                id: entry.prefix,
                owner: extension.to_owned(),
            });
        }
        snippets.push(Snippet {
            prefix: entry.prefix,
            body: entry.body,
            description: entry.description,
        });
    }

    Ok(SnippetSet {
        language: file.language,
        extension: extension.to_owned(),
        snippets,
    })
}
