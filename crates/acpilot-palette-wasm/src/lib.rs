//! The palette solver, on the web.
//!
//! `theme_follow_terminal = true` asks the terminal what its own background and
//! foreground are and solves a complete palette around that pair. The showcase on
//! the documentation site offers the same thing to a reader: pick two colours and
//! every screen on the page recolours with the palette the product would draw — the
//! one [`acpilot_palette::follow::solve`] produces, running in the browser rather
//! than described in a paragraph.
//!
//! # Why this is a separate crate
//!
//! The solver is a leaf (`acpilot-palette`), the loader is not: `acpilot-extensions`
//! pulls the daemon's manifest parsing, which pulls a serial port, and neither has
//! any business in a browser tab. So the wasm build depends on the leaf and the
//! product keeps importing the same functions through its re-export — one
//! implementation with two front doors, rather than a JavaScript port that drifts
//! the first time the solver changes.
//!
//! # The ABI
//!
//! `cdylib` with three exported C functions and no `wasm-bindgen`: the caller is a
//! page that already knows how to read a `Uint8Array` out of `WebAssembly.Memory`,
//! and a generated JS shim would be one more artefact to build, commit and keep in
//! step. The result of the last call is a JSON document left in a static buffer,
//! read with [`out_ptr`] and the length the call returned.
//!
//! The `unsafe` here is the export attribute itself. Rust mangles symbol names; a
//! WebAssembly import needs the name a script asks for, and `#[unsafe(no_mangle)]`
//! is the only way to say so. There is no unsafe block, no pointer arithmetic and
//! no unchecked access in this file, which is why the crate stays under the
//! workspace's `unsafe_code` lint with the one attribute allowed where it is the
//! point.

use std::collections::BTreeMap;
use std::sync::Mutex;

use acpilot_palette::theme::{Color, ResolvedTheme};
use acpilot_palette::{contrast, follow};
use serde_json::{Value, json};

/// The last answer, waiting to be read out of linear memory.
///
/// A static rather than a return value because a `String` cannot cross a C ABI:
/// the length comes back as the return value and the bytes are read from here at
/// the pointer [`out_ptr`] names. Single-threaded by construction — a wasm module
/// without shared memory has one thread — but a `Mutex` states that instead of
/// assuming it.
static LAST: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// A `0xRRGGBB` word as a colour.
fn rgb(word: u32) -> Color {
    Color::from_rgb(
        ((word >> 16) & 0xff) as u8,
        ((word >> 8) & 0xff) as u8,
        (word & 0xff) as u8,
    )
}

/// The theme as the site's `themes.json` describes one, so a page reads both from
/// the same shape.
fn palette_json(theme: &ResolvedTheme) -> Value {
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
    json!({
        "id": theme.id,
        "name": theme.name,
        "appearance": theme.appearance,
        "ui": ui,
        "syntax": syntax,
    })
}

/// Solve a palette around `background` and `foreground`, as `0xRRGGBB` words.
///
/// Returns the length of the JSON document now in the buffer, or `0` when the pair
/// cannot be solved into a readable palette — which the page reports as "that pair
/// has no answer" rather than drawing something nobody can read.
#[unsafe(no_mangle)]
pub extern "C" fn solve(background: u32, foreground: u32) -> usize {
    let answer = match follow::solve(rgb(background), rgb(foreground)) {
        Some(theme) => palette_json(&theme).to_string(),
        None => return 0,
    };
    store(answer.as_bytes())
}

/// WCAG contrast between two `0xRRGGBB` words, from 1.0 to 21.0.
///
/// Exported because it is the number the solver is aiming at, and a page that shows
/// the palette without the ratio is asking a reader to take the claim on trust.
#[unsafe(no_mangle)]
pub extern "C" fn ratio(first: u32, second: u32) -> f64 {
    rgb(first).contrast_ratio(rgb(second))
}

/// The AA threshold the product's themes are held to.
#[unsafe(no_mangle)]
pub extern "C" fn aa_threshold() -> f64 {
    contrast::AA_NORMAL_TEXT
}

/// Where the last answer starts.
#[unsafe(no_mangle)]
pub extern "C" fn out_ptr() -> *const u8 {
    LAST.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ptr()
}

/// Replace the buffer with `bytes` and report its length.
fn store(bytes: &[u8]) -> usize {
    let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    last.clear();
    last.extend_from_slice(bytes);
    last.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_palette::theme::{SyntaxToken, UiToken};

    /// Call [`solve`] the way a page does: `0` is "no answer", anything else is the
    /// length of the document in the buffer.
    fn solved(background: u32, foreground: u32) -> Option<Value> {
        if solve(background, foreground) == 0 {
            return None;
        }
        Some(serde_json::from_slice(&last()).expect("the answer is JSON"))
    }

    /// The buffer's contents.
    ///
    /// A copy rather than a pointer: a page reads `out_ptr` and a length, which is
    /// the only reason that pair exists, and a test does not need to re-enact the
    /// reading of linear memory to check what was written to it.
    fn last() -> Vec<u8> {
        LAST.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    #[test]
    fn a_dark_pair_solves_into_every_token_the_page_draws() {
        let theme = solved(0x000000, 0xffffff).expect("a black terminal is solvable");
        assert_eq!(theme["id"], follow::ID);
        assert_eq!(theme["appearance"], "dark");
        for token in UiToken::ALL {
            let value = theme["ui"][token.as_str()]
                .as_str()
                .unwrap_or_else(|| panic!("{} is set", token));
            assert!(value.starts_with('#'), "{token} is a #rrggbb colour");
        }
        for token in SyntaxToken::ALL {
            assert!(
                theme["syntax"][token.as_str()].as_str().is_some(),
                "{token} is set too: the editor and a diff read them"
            );
        }
    }

    #[test]
    fn a_light_terminal_solves_the_other_half() {
        let light = solved(0xfdf6e3, 0x073642).expect("light is solvable");
        assert_eq!(light["appearance"], "light");
        let dark = solved(0x1e1e1e, 0xd8d8d8).expect("dark is solvable");
        assert_ne!(
            light["ui"]["bg"], dark["ui"]["bg"],
            "the terminal's own background is what the palette is built on"
        );
    }

    #[test]
    fn the_ratio_is_the_products_own_measurement() {
        assert!((ratio(0x000000, 0xffffff) - 21.0).abs() < 0.001);
        assert!((ratio(0xffffff, 0xffffff) - 1.0).abs() < 0.001);
        assert!(aa_threshold() > 4.0 && aa_threshold() < 5.0);
    }
}
