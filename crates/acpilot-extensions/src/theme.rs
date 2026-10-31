//! Theme tokens: the one schema every surface shares.
//!
//! A theme is a token file, not a recolouring function. The TUI, the desktop
//! app, and the web UI read the same names, so "muted text" cannot mean three
//! different greys. A theme may be partial — it inherits whatever it omits from
//! the theme named by `extends` — but a theme with no base must supply every
//! token, because there is nothing left to inherit from.

use std::collections::BTreeMap;

use acpilot_proto::extension::ThemeAppearance;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::error::ExtensionError;
use crate::manifest::{SCHEMA_VERSION, validate_namespaced_id};

// The types are `acpilot-palette`'s, and this module is where a reader of a theme
// file expects to find them: a surface imports `acpilot_extensions::theme::Color`,
// and the site's WebAssembly showcase imports the same type from the leaf crate.
// One definition, two doors.
pub use acpilot_palette::theme::{Color, ColorError, Palette, ResolvedTheme, SyntaxToken, UiToken};

/// A theme file, exactly as written, before inheritance.
///
/// Token tables are maps keyed by enum, so a typo is `unknown variant` at parse
/// time with the offending key named — the same rule configuration files follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    /// Schema version. `1` today.
    pub schema_version: u32,
    /// Namespaced identifier, `publisher.name`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Light or dark, so a UI can pick the right half of a pair.
    pub appearance: ThemeAppearance,
    /// Theme to inherit unset tokens from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// Non-syntax tokens.
    #[serde(default)]
    pub ui: BTreeMap<UiToken, Color>,
    /// Syntax tokens.
    #[serde(default)]
    pub syntax: BTreeMap<SyntaxToken, Color>,
}

impl Theme {
    /// Parse and validate a theme file.
    ///
    /// The `path` is the file's path relative to the extension root; it appears
    /// in every error so a broken theme is a one-line fix, not a hunt.
    pub fn parse(path: &str, text: &str) -> Result<Self, ExtensionError> {
        let theme: Self = toml::from_str(text).map_err(|error| ExtensionError::Toml {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        theme.validate(path)?;
        Ok(theme)
    }

    /// Check the rules that do not need the rest of the registry.
    pub fn validate(&self, path: &str) -> Result<(), ExtensionError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ExtensionError::SchemaVersion {
                path: path.to_owned(),
                found: u64::from(self.schema_version),
                expected: SCHEMA_VERSION,
            });
        }
        validate_namespaced_id("theme id", &self.id)?;
        if self.name.trim().is_empty() {
            return Err(ExtensionError::EmptyField {
                path: path.to_owned(),
                field: "name",
            });
        }
        if let Some(base) = &self.extends {
            validate_namespaced_id("extended theme id", base)?;
        }

        // A base theme is the floor: it has nothing to inherit from, so a
        // missing token is a hole every surface would fall through.
        if self.extends.is_none() {
            let missing: Vec<&str> = UiToken::ALL
                .iter()
                .filter(|token| !self.ui.contains_key(token))
                .map(|token| token.as_str())
                .chain(
                    SyntaxToken::ALL
                        .iter()
                        .filter(|token| !self.syntax.contains_key(token))
                        .map(|token| token.as_str()),
                )
                .collect();
            if !missing.is_empty() {
                return Err(ExtensionError::Theme {
                    id: self.id.clone(),
                    message: format!(
                        "a theme without `extends` must set every token; missing {}",
                        missing.join(", ")
                    ),
                });
            }
        }
        Ok(())
    }

    /// Merge this theme over its base, producing the palette a UI reads.
    pub fn resolve(
        &self,
        base: Option<&Palette>,
        extension_id: &str,
        built_in: bool,
    ) -> ResolvedTheme {
        let mut palette = base.cloned().unwrap_or_default();
        palette.ui.extend(self.ui.iter().map(|(k, v)| (*k, *v)));
        palette
            .syntax
            .extend(self.syntax.iter().map(|(k, v)| (*k, *v)));
        ResolvedTheme {
            id: self.id.clone(),
            name: self.name.clone(),
            appearance: self.appearance,
            built_in,
            extension_id: extension_id.to_owned(),
            palette,
        }
    }
}

/// Why a theme could not be produced.
///
/// One error type for every surface: the TUI, the desktop app and the web UI ask
/// the same question — "give me this theme, complete" — so they get the same
/// answer, and a broken theme is one message rather than three interpretations of
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ThemeError {
    /// No loaded extension contributes a theme with that id.
    #[error("no loaded extension provides the theme {id:?}")]
    Unknown {
        /// The id that was asked for.
        id: String,
    },
    /// A theme inherits from one that is not loaded.
    #[error("theme {required_by:?} inherits from {id:?}, which is not loaded")]
    MissingBase {
        /// The base id that could not be found.
        id: String,
        /// The theme that named it.
        required_by: String,
    },
    /// `extends` leads back to a theme already in the chain.
    #[error("theme inheritance loops back to {id:?}")]
    Cycle {
        /// The id that closed the loop.
        id: String,
    },
    /// The palette does not set every UI token.
    #[error(
        "theme {id:?} does not set {}; a surface would have nothing to draw with",
        tokens.join(", ")
    )]
    MissingTokens {
        /// The theme as resolved, for the message.
        id: String,
        /// The tokens it left out.
        tokens: Vec<&'static str>,
    },
}

/// Every theme a set of loaded extensions provides.
///
/// Resolution lives here, next to the tokens, rather than in a frontend: "the same
/// theme extension draws all three UIs" is only true if all three follow the same
/// `extends` chain and refuse the same holes. A frontend's job is the conversion
/// to its own colour type, not a second opinion about what a theme means.
pub struct ThemeRegistry<'a> {
    themes: Vec<(&'a Theme, &'a crate::loader::LoadedExtension)>,
}

impl<'a> ThemeRegistry<'a> {
    /// Index the themes the extensions contribute.
    #[must_use]
    pub fn new(extensions: &'a [crate::loader::LoadedExtension]) -> Self {
        let themes = extensions
            .iter()
            .flat_map(|extension| {
                extension
                    .contributions
                    .iter()
                    .filter_map(move |contribution| match contribution {
                        crate::loader::Contribution::Theme(theme) => {
                            Some((theme.as_ref(), extension))
                        }
                        _ => None,
                    })
            })
            .collect();
        Self { themes }
    }

    /// The ids of every theme on offer, in the order they were loaded.
    #[must_use]
    pub fn ids(&self) -> Vec<&'a str> {
        self.themes
            .iter()
            .map(|(theme, _)| theme.id.as_str())
            .collect()
    }

    /// Find a theme by id and resolve its inheritance.
    ///
    /// # Errors
    ///
    /// The id is not there, a base it names is not there, or `extends` loops.
    pub fn resolve(&self, id: &str) -> Result<ResolvedTheme, ThemeError> {
        let (theme, owner) = self
            .themes
            .iter()
            .find(|(theme, _)| theme.id == id)
            .copied()
            .ok_or_else(|| ThemeError::Unknown { id: id.to_owned() })?;

        // Requests come from a file, so a cycle is something a person can write;
        // walking it away with a depth limit would be a hang instead.
        let mut chain = vec![theme];
        let mut seen = vec![theme.id.as_str()];
        let mut cursor = theme;
        while let Some(base_id) = cursor.extends.as_deref() {
            if seen.contains(&base_id) {
                return Err(ThemeError::Cycle {
                    id: base_id.to_owned(),
                });
            }
            seen.push(base_id);
            let (base, _) = self
                .themes
                .iter()
                .find(|(theme, _)| theme.id == base_id)
                .copied()
                .ok_or_else(|| ThemeError::MissingBase {
                    id: base_id.to_owned(),
                    required_by: cursor.id.clone(),
                })?;
            chain.push(base);
            cursor = base;
        }

        // Root first: each layer overrides the one before it.
        let mut palette = Palette::default();
        for layer in chain.iter().rev() {
            palette = layer
                .resolve(Some(&palette), &owner.manifest.id, owner.built_in)
                .palette;
        }

        Ok(ResolvedTheme {
            id: theme.id.clone(),
            name: theme.name.clone(),
            appearance: theme.appearance,
            built_in: owner.built_in,
            extension_id: owner.manifest.id.clone(),
            palette,
        })
    }

    /// A theme with every UI token present, which is what a surface needs.
    ///
    /// # Errors
    ///
    /// [`ThemeRegistry::resolve`]'s errors, or [`ThemeError::MissingTokens`] when
    /// the chain leaves a token unset.
    pub fn complete(&self, id: &str) -> Result<ResolvedTheme, ThemeError> {
        let theme = self.resolve(id)?;
        let missing: Vec<&'static str> = UiToken::ALL
            .iter()
            .filter(|token| !theme.palette.ui.contains_key(*token))
            .map(|token| token.as_str())
            .collect();
        if missing.is_empty() {
            Ok(theme)
        } else {
            Err(ThemeError::MissingTokens {
                id: theme.id,
                tokens: missing,
            })
        }
    }

    /// [`ThemeRegistry::complete`], plus every rule a theme can break and still be
    /// worth drawing.
    ///
    /// Today that is the panel window ([`crate::contrast::PANEL_MIN`]–
    /// [`crate::contrast::PANEL_MAX`]): a theme whose panel is 1.04:1 from the
    /// background is a panel nobody can see, not a file that cannot be read, so it
    /// loads and the caller says what is wrong with it. Refusing it would leave a
    /// person with no theme at all over a shade they can fix in one line.
    ///
    /// # Errors
    ///
    /// [`ThemeRegistry::complete`]'s errors: a theme that is not there, an unknown
    /// base, a cycle, or a token the chain never sets.
    pub fn complete_with_warnings(
        &self,
        id: &str,
    ) -> Result<(ResolvedTheme, Vec<String>), ThemeError> {
        let theme = self.complete(id)?;
        let warnings = crate::contrast::panel_violation(&theme)
            .map(|violation| vec![format!("theme {}: {violation}", theme.id)])
            .unwrap_or_default();
        Ok((theme, warnings))
    }
}

/// A palette a surface can draw with: every token checked, once.
///
/// The terminal, the window and the browser each turn these colours into
/// something of their own — a terminal colour, a packed `0xRRGGBB` word, a CSS
/// variable — but none of them decides *which* tokens exist, or what to do about
/// a gap in the set. That is this type's job, and it is the only place it happens.
///
/// A token added to [`UiToken::ALL`] therefore reaches every surface without a
/// second table to update: the window reads the tokens it draws, and the browser
/// derives its variable names from [`UiToken::as_str`] rather than from a list of
/// its own. There is no per-surface token list to forget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedTheme {
    id: String,
    name: String,
    appearance: ThemeAppearance,
    ui: BTreeMap<UiToken, Color>,
}

impl AppliedTheme {
    /// Take a resolved theme, refusing one with a hole.
    ///
    /// # Errors
    ///
    /// [`ThemeError::MissingTokens`] when the palette left a token unset, naming
    /// every one it left out: a surface that drew with nine of ten tokens would
    /// paint a transparent panel in one place and a black one in another, and the
    /// bug would look like a layout mistake.
    pub fn new(resolved: &ResolvedTheme) -> Result<Self, ThemeError> {
        let missing: Vec<&'static str> = UiToken::ALL
            .iter()
            .filter(|token| !resolved.palette.ui.contains_key(*token))
            .map(|token| token.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(ThemeError::MissingTokens {
                id: resolved.id.clone(),
                tokens: missing,
            });
        }
        Ok(Self {
            id: resolved.id.clone(),
            name: resolved.name.clone(),
            appearance: resolved.appearance,
            ui: resolved.palette.ui.clone(),
        })
    }

    /// Find a theme by id among loaded extensions and resolve its inheritance.
    ///
    /// The walk and the hole check are the registry's
    /// ([`ThemeRegistry::complete`]): a pack can ship a three-line theme over one
    /// of ours without copying ten tokens, and the terminal, the window and the
    /// browser all accept or refuse it for the same reasons.
    ///
    /// # Errors
    ///
    /// [`ThemeRegistry::complete`]'s errors.
    pub fn from_extensions(
        extensions: &[crate::loader::LoadedExtension],
        id: &str,
    ) -> Result<Self, ThemeError> {
        let resolved = ThemeRegistry::new(extensions).complete(id)?;
        Self::new(&resolved)
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

    /// Whether the theme says it is light or dark.
    ///
    /// Use [`AppliedTheme::is_dark`] to decide how something is *painted*: a
    /// theme file can claim one half and be the other, and the colour on screen
    /// is the one a person sees.
    #[must_use]
    pub fn appearance(&self) -> ThemeAppearance {
        self.appearance
    }

    /// Whether the theme paints a dark background.
    ///
    /// Decided from the background's luminance rather than from
    /// [`AppliedTheme::appearance`], because the two can disagree in a theme file
    /// and a browser or window has to match the colour it is actually painted
    /// with.
    #[must_use]
    pub fn is_dark(&self) -> bool {
        self.color(UiToken::Bg).relative_luminance() < 0.5
    }

    /// A UI token's colour.
    ///
    /// # Panics
    ///
    /// Never in practice: a theme is only built with every token present, which
    /// is the only way a token could be absent. The `expect` is the proof of that
    /// invariant rather than a runtime gamble.
    #[must_use]
    pub fn color(&self, token: UiToken) -> Color {
        *self
            .ui
            .get(&token)
            .expect("a theme is only built with every UI token present")
    }

    /// A UI token as `0xRRGGBB`, which is the word a GPU and `gpui` take.
    #[must_use]
    pub fn ui(&self, token: UiToken) -> u32 {
        let (r, g, b) = self.color(token).channels();
        (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
    }
}

#[cfg(test)]
mod tests {
    use crate::builtin;
    use crate::loader::{Contribution, LoadedExtension};

    use super::*;

    fn built_ins() -> Vec<LoadedExtension> {
        builtin::load_all().expect("the built-in extensions load")
    }

    #[test]
    fn the_shipped_dark_theme_maps_to_the_tokens_the_tui_reads() {
        let theme = AppliedTheme::from_extensions(&built_ins(), "acpilot.night-flight")
            .expect("night-flight resolves");

        assert_eq!(theme.ui(UiToken::Bg), 0x0007_0d18);
        assert_eq!(theme.ui(UiToken::Text), 0x00e8_edf6);
        assert_eq!(theme.ui(UiToken::BgPanel), 0x0014_1e33);
        assert_eq!(theme.color(UiToken::Bg).to_string(), "#070d18");
        assert!(
            theme.is_dark(),
            "the theme says so and the surface trusts it"
        );
        assert_eq!(theme.appearance(), ThemeAppearance::Dark);
        assert_eq!(theme.name(), "Night flight");
        assert_eq!(theme.id(), "acpilot.night-flight");
    }

    #[test]
    fn the_light_theme_is_a_different_palette_not_a_filtered_one() {
        let dark = AppliedTheme::from_extensions(&built_ins(), "acpilot.night-flight")
            .expect("night-flight resolves");
        let light =
            AppliedTheme::from_extensions(&built_ins(), "acpilot.paper").expect("paper resolves");

        assert!(!light.is_dark());
        assert_ne!(dark.ui(UiToken::Bg), light.ui(UiToken::Bg));
        assert_ne!(dark.ui(UiToken::Text), light.ui(UiToken::Text));
    }

    #[test]
    fn a_theme_nobody_provides_is_named_in_the_error() {
        let error = AppliedTheme::from_extensions(&built_ins(), "acpilot.no-such-theme")
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
                Contribution::Theme(theme) if theme.id == "acpilot.night-flight" => {
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
        let resolved = ResolvedTheme {
            id: theme.id.clone(),
            name: theme.name.clone(),
            appearance: theme.appearance,
            built_in: true,
            extension_id: "acpilot.night-flight".to_owned(),
            palette,
        };

        let error = AppliedTheme::new(&resolved).expect_err("a partial palette is refused");
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
                extension.contributions.iter().any(|contribution| {
                    matches!(contribution, Contribution::Theme(theme) if theme.id == "acpilot.night-flight")
                })
            })
            .expect("night-flight ships in a built-in");

        let child_extension = crate::load_embedded(&[
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
        let theme = AppliedTheme::from_extensions(&extensions, "acpilot.child")
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
    fn a_theme_that_extends_something_absent_is_an_error_not_a_blank_surface() {
        let extension = crate::load_embedded(&[
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

        let error = AppliedTheme::from_extensions(&[extension], "acpilot.orphan")
            .expect_err("an absent base is refused");
        assert_eq!(
            error,
            ThemeError::MissingBase {
                id: "acpilot.does-not-exist".to_owned(),
                required_by: "acpilot.orphan".to_owned(),
            }
        );
    }
}
