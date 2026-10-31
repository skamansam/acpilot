//! The theme data the documentation site renders.
//!
//! The site's theme gallery is generated from the same files the binary embeds,
//! so the pictures cannot drift from the product. The contrast ratios in the
//! gallery are the ones CI enforces — not recomputed in JavaScript, where a
//! second implementation could disagree with the gate.
//!
//! It carries two lists: the themes the binary ships, and palettes *solved* from
//! two representative terminals. The second is the [`crate::follow`] solver run at
//! generation time, so `theme_follow_terminal` has a picture in the manual instead
//! of only a paragraph — and the picture is a palette that setting would actually
//! produce, ratios included.
//!
//! Regenerate with `cargo xtask docs-data`. A test fails when the committed file
//! is stale, so the drift is caught in review rather than by a reader.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::contrast;
use crate::follow;
use crate::registry::Registry;
use crate::theme::{Color, ResolvedTheme};

/// Render `site/src/data/themes.json`.
#[must_use]
pub fn themes_json(registry: &Registry) -> String {
    let themes: Vec<Value> = registry.themes().iter().map(theme_json).collect();
    let followed = followed_json();

    let document = json!({
        "generated_by": "cargo xtask docs-data",
        "schema_version": 1,
        "aa_threshold": contrast::AA_NORMAL_TEXT,
        "themes": themes,
        "followed": followed,
    });

    let mut text =
        serde_json::to_string_pretty(&document).expect("the theme document is always serialisable");
    text.push('\n');
    text
}

/// One resolved theme, with its palette and every measured pair.
fn theme_json(theme: &ResolvedTheme) -> Value {
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
        "panel": audit.panel().map(round),
    })
}

/// The palettes [`follow`] solves for the terminals the gallery shows.
///
/// Two, and deliberately the two ends of the ramp: a black terminal with white ink
/// is what most people are looking at, and a cream one with dark ink is the case
/// that proves the solver reads polarity from the pair instead of assuming a dark
/// theme. Both are solved here, when the data is generated, by the function the TUI
/// calls at launch — a picture of a palette the setting would not produce is exactly
/// what generating this file is meant to prevent.
fn followed_json() -> Vec<Value> {
    sample_terminals()
        .into_iter()
        .filter_map(|(label, background, foreground)| {
            // A pair that cannot carry a UI gets no card, rather than a card whose
            // ratios contradict the gate: `solve` answers `None` for those.
            let theme = follow::solve(background, foreground)?;
            let mut value = theme_json(&theme);
            value["terminal"] = json!({
                "label": label,
                "background": background.to_string(),
                "foreground": foreground.to_string(),
            });
            Some(value)
        })
        .collect()
}

/// The terminal pairs the gallery solves a palette for: the label a reader sees,
/// and the pair a terminal would have reported.
fn sample_terminals() -> [(&'static str, Color, Color); 2] {
    [
        (
            "Black terminal, white ink",
            Color::from_rgb(0x00, 0x00, 0x00),
            Color::from_rgb(0xff, 0xff, 0xff),
        ),
        (
            "Cream terminal, dark ink",
            Color::from_rgb(0xfd, 0xf6, 0xe3),
            Color::from_rgb(0x59, 0x64, 0x6a),
        ),
    ]
}

/// Two decimals: enough to compare, not so many that a diff is noise.
fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
