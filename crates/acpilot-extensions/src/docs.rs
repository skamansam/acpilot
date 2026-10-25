//! The theme data the documentation site renders.
//!
//! The site's theme gallery is generated from the same files the binary embeds,
//! so the pictures cannot drift from the product. The contrast ratios in the
//! gallery are the ones CI enforces — not recomputed in JavaScript, where a
//! second implementation could disagree with the gate.
//!
//! Regenerate with `cargo xtask docs-data`. A test fails when the committed file
//! is stale, so the drift is caught in review rather than by a reader.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::contrast;
use crate::registry::Registry;

/// Render `site/src/data/themes.json`.
#[must_use]
pub fn themes_json(registry: &Registry) -> String {
    let themes: Vec<Value> = registry
        .themes()
        .iter()
        .map(|theme| {
            let audit = contrast::audit(theme);
            let ui: BTreeMap<&str, String> = theme
                .palette
                .ui
                .iter()
                .map(|(token, color)| (token.as_str(), color.to_string()))
                .collect();
            let syntax: BTreeMap<&str, String> = theme
                .palette
                .syntax
                .iter()
                .map(|(token, color)| (token.as_str(), color.to_string()))
                .collect();
            let pairs: Vec<Value> = audit
                .pairs
                .iter()
                .map(|pair| {
                    json!({
                        "foreground": pair.foreground.as_str(),
                        "surface": pair.surface.as_str(),
                        "ratio": round(pair.ratio),
                    })
                })
                .collect();

            json!({
                "id": theme.id,
                "name": theme.name,
                "appearance": theme.appearance,
                "built_in": theme.built_in,
                "extension_id": theme.extension_id,
                "ui": ui,
                "syntax": syntax,
                "pairs": pairs,
                "minimum": audit.minimum().map(round),
            })
        })
        .collect();

    let document = json!({
        "generated_by": "cargo xtask docs-data",
        "schema_version": 1,
        "aa_threshold": contrast::AA_NORMAL_TEXT,
        "themes": themes,
    });

    let mut text =
        serde_json::to_string_pretty(&document).expect("the theme document is always serialisable");
    text.push('\n');
    text
}

/// Two decimals: enough to compare, not so many that a diff is noise.
fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
