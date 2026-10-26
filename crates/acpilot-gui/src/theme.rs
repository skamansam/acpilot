//! Theme tokens, as the values gpui draws with.
//!
//! The tokens are [`acpilot_extensions::theme`]'s, not this crate's: the TUI and
//! the GUI read one `theme.toml`, so "muted text" cannot mean two greys. What
//! lives here is only the conversion — a resolved [`Palette`] checked for holes
//! and turned into the `0xRRGGBB` words `gpui::rgb` takes.
//!
//! The check matters more than the conversion. A theme that resolves to nine of
//! ten tokens would draw a transparent panel in one place and a black one in
//! another, and the bug would look like a layout mistake. Refusing the palette by
//! name, at load, turns that into one line of error text.

use std::collections::BTreeMap;

use acpilot_extensions::ThemeAppearance;
use acpilot_extensions::loader::LoadedExtension;
use acpilot_extensions::theme::{Palette, ThemeRegistry, UiToken};

/// The shared [`acpilot_extensions::theme::ThemeError`], so a surface that already
/// depends on this crate does not need a second import to say what went wrong.
pub use acpilot_extensions::theme::ThemeError;

/// A complete palette in the form gpui wants.
///
/// Every UI token is present or the theme was refused, so [`GuiTheme::ui`] has no
/// "missing" branch and no caller has to invent a fallback colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiTheme {
    id: String,
    name: String,
    dark: bool,
    ui: BTreeMap<UiToken, u32>,
}

impl GuiTheme {
    /// Wrap a resolved palette, refusing one with holes.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        appearance: ThemeAppearance,
        palette: &Palette,
    ) -> Result<Self, ThemeError> {
        let id = id.into();
        let missing: Vec<&'static str> = UiToken::ALL
            .iter()
            .filter(|token| !palette.ui.contains_key(*token))
            .map(|token| token.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(ThemeError::MissingTokens {
                id,
                tokens: missing,
            });
        }

        let ui = UiToken::ALL
            .iter()
            .map(|token| {
                let color = palette
                    .ui(*token)
                    .expect("the missing-token check above covers every token");
                (*token, hex(color))
            })
            .collect();

        Ok(Self {
            id,
            name: name.into(),
            dark: matches!(appearance, ThemeAppearance::Dark),
            ui,
        })
    }

    /// Find a theme by id among loaded extensions and resolve its inheritance.
    ///
    /// The walk and the hole check are the shared ones
    /// ([`ThemeRegistry::complete`]): a pack can ship a three-line theme over one
    /// of ours without copying twelve tokens, and the terminal, the window and the
    /// browser all accept or refuse it for the same reasons.
    pub fn from_extensions(extensions: &[LoadedExtension], id: &str) -> Result<Self, ThemeError> {
        let resolved = ThemeRegistry::new(extensions).complete(id)?;
        Self::new(
            resolved.id,
            resolved.name,
            resolved.appearance,
            &resolved.palette,
        )
    }

    /// The theme's namespaced id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The theme's display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this is a dark theme, for anything that has to pick a half.
    #[must_use]
    pub fn is_dark(&self) -> bool {
        self.dark
    }

    /// A UI token as `0xRRGGBB`, which is what `gpui::rgb` takes.
    ///
    /// # Panics
    ///
    /// Never in practice: [`GuiTheme::new`] refuses a palette with a hole, which
    /// is the only way a token could be absent. The `expect` is the proof of that
    /// invariant rather than a runtime gamble.
    #[must_use]
    pub fn ui(&self, token: UiToken) -> u32 {
        *self
            .ui
            .get(&token)
            .expect("a theme is only built with every UI token present")
    }
}

/// `#rrggbb` as a word, without the `#`.
fn hex(color: acpilot_extensions::theme::Color) -> u32 {
    let (r, g, b) = color.channels();
    (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

#[cfg(test)]
mod tests {
    use acpilot_extensions::builtin;
    use acpilot_extensions::theme::{Color, UiToken};

    use super::*;

    fn built_ins() -> Vec<LoadedExtension> {
        builtin::load_all().expect("the built-in extensions load")
    }

    #[test]
    fn the_shipped_dark_theme_maps_to_the_tokens_the_tui_reads() {
        let theme = GuiTheme::from_extensions(&built_ins(), "acpilot.night-flight")
            .expect("night-flight resolves");

        assert_eq!(theme.ui(UiToken::Bg), 0x0007_0d18);
        assert_eq!(theme.ui(UiToken::Text), 0x00e8_edf6);
        assert_eq!(theme.ui(UiToken::BgPanel), 0x0014_1e33);
        assert!(
            theme.is_dark(),
            "the theme says so and the window trusts it"
        );
        assert_eq!(theme.name(), "Night flight");
    }

    #[test]
    fn the_light_theme_is_a_different_palette_not_a_filtered_one() {
        let dark = GuiTheme::from_extensions(&built_ins(), "acpilot.night-flight")
            .expect("night-flight resolves");
        let light =
            GuiTheme::from_extensions(&built_ins(), "acpilot.paper").expect("paper resolves");

        assert!(!light.is_dark());
        assert_ne!(dark.ui(UiToken::Bg), light.ui(UiToken::Bg));
        assert_ne!(dark.ui(UiToken::Text), light.ui(UiToken::Text));
    }

    #[test]
    fn a_theme_nobody_provides_is_named_in_the_error() {
        let error = GuiTheme::from_extensions(&built_ins(), "acpilot.no-such-theme")
            .expect_err("an unknown theme is refused");
        assert_eq!(
            error,
            ThemeError::Unknown {
                id: "acpilot.no-such-theme".to_owned()
            }
        );
        assert!(error.to_string().contains("acpilot.no-such-theme"));
    }

    #[test]
    fn a_palette_with_a_hole_names_every_missing_token() {
        let extensions = built_ins();
        let theme = extensions
            .iter()
            .flat_map(|extension| &extension.contributions)
            .find_map(|contribution| match contribution {
                acpilot_extensions::loader::Contribution::Theme(theme)
                    if theme.id == "acpilot.night-flight" =>
                {
                    Some(theme.as_ref())
                }
                _ => None,
            })
            .expect("the built-in theme is there");

        // Build the full palette, then remove the two tokens a buggy theme is
        // most likely to forget.
        let mut palette = theme.resolve(None, "acpilot.night-flight", true).palette;
        palette.ui.remove(&UiToken::Danger);
        palette.ui.remove(&UiToken::AccentHigh);

        let error = GuiTheme::new(&theme.id, &theme.name, theme.appearance, &palette)
            .expect_err("a partial palette is refused");
        match &error {
            ThemeError::MissingTokens { tokens, .. } => {
                assert!(tokens.contains(&"danger"), "{tokens:?}");
                assert!(tokens.contains(&"accent_high"), "{tokens:?}");
                assert_eq!(tokens.len(), 2, "{tokens:?}");
            }
            other => panic!("expected missing tokens, got {other}"),
        }
        assert!(error.to_string().contains("danger"));
    }

    #[test]
    fn an_inheriting_theme_takes_the_base_and_overrides_one_token() {
        let base = built_ins()
            .into_iter()
            .find(|extension| {
                extension
                    .contributions
                    .iter()
                    .any(|contribution| matches!(contribution, acpilot_extensions::loader::Contribution::Theme(theme) if theme.id == "acpilot.night-flight"))
            })
            .expect("night-flight ships in a built-in");

        let child_extension = acpilot_extensions::load_embedded(&[
            (
                "extension.toml",
                "schema_version = 1\nid = \"acpilot.child-pack\"\nname = \"Child pack\"\nversion = \"0.1.0\"\npublisher = \"acpilot\"\ndescription = \"a theme over night-flight\"\n\n[[contributes]]\nkind = \"theme\"\npath = \"child.toml\"\n",
            ),
            (
                "child.toml",
                "schema_version = 1\nid = \"acpilot.child\"\nname = \"Child\"\nappearance = \"dark\"\nextends = \"acpilot.night-flight\"\n\n[ui]\naccent = \"#ff0000\"\n",
            ),
        ])
        .expect("the child extension loads");

        let extensions = vec![base, child_extension];
        let theme = GuiTheme::from_extensions(&extensions, "acpilot.child")
            .expect("the child theme resolves over its base");

        assert_eq!(
            theme.ui(UiToken::Accent),
            0x00ff_0000,
            "the child's token wins"
        );
        assert_eq!(
            theme.ui(UiToken::Bg),
            0x0007_0d18,
            "and everything it did not set comes from the base"
        );
    }

    #[test]
    fn a_theme_that_extends_something_absent_is_an_error_not_a_blank_window() {
        let extension = acpilot_extensions::load_embedded(&[
            (
                "extension.toml",
                "schema_version = 1\nid = \"acpilot.orphan-pack\"\nname = \"Orphan\"\nversion = \"0.1.0\"\ndescription = \"extends a theme nobody ships\"\npublisher = \"acpilot\"\n\n[[contributes]]\nkind = \"theme\"\npath = \"orphan.toml\"\n",
            ),
            (
                "orphan.toml",
                "schema_version = 1\nid = \"acpilot.orphan\"\nname = \"Orphan\"\nappearance = \"dark\"\nextends = \"acpilot.does-not-exist\"\n\n[ui]\naccent = \"#ff0000\"\n",
            ),
        ])
        .expect("the orphan extension loads");

        let error = GuiTheme::from_extensions(&[extension], "acpilot.orphan")
            .expect_err("an absent base is refused");
        assert_eq!(
            error,
            ThemeError::MissingBase {
                id: "acpilot.does-not-exist".to_owned(),
                required_by: "acpilot.orphan".to_owned(),
            }
        );
    }

    #[test]
    fn hex_is_the_three_channels_in_order() {
        assert_eq!(hex(Color::from_rgb(0x07, 0x0d, 0x18)), 0x0007_0d18);
        assert_eq!(hex(Color::from_rgb(0xff, 0xff, 0xff)), 0x00ff_ffff);
        assert_eq!(hex(Color::from_rgb(0, 0, 0)), 0);
    }
}
