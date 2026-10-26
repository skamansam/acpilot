//! Theme tokens: the one schema every surface shares.
//!
//! A theme is a token file, not a recolouring function. The TUI, the desktop
//! app, and the web UI read the same names, so "muted text" cannot mean three
//! different greys. A theme may be partial — it inherits whatever it omits from
//! the theme named by `extends` — but a theme with no base must supply every
//! token, because there is nothing left to inherit from.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use acpilot_proto::extension::ThemeAppearance;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::error::ExtensionError;
use crate::manifest::{SCHEMA_VERSION, validate_namespaced_id};

/// A colour, stored as the eight bits per channel a terminal actually has.
///
/// The file format is `#rrggbb` in lowercase or uppercase. No named colours, no
/// alpha: a theme that needs translucency is asking the UI to invent a
/// background, and the result is unreadable on somebody's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Color {
    r: u8,
    g: u8,
    b: u8,
}

impl Color {
    /// Build a colour from its channels.
    #[must_use]
    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// The channels, in RGB order.
    #[must_use]
    pub const fn channels(self) -> (u8, u8, u8) {
        (self.r, self.g, self.b)
    }

    /// WCAG relative luminance, on a 0–1 scale.
    ///
    /// This is the sRGB definition, including the linearisation step that people
    /// skip when they compare channel values and conclude a pair "looks fine".
    #[must_use]
    pub fn relative_luminance(self) -> f64 {
        fn linear(channel: u8) -> f64 {
            let value = f64::from(channel) / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }

        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// WCAG contrast ratio between two colours, from 1.0 to 21.0.
    #[must_use]
    pub fn contrast_ratio(self, other: Self) -> f64 {
        let (first, second) = (self.relative_luminance(), other.relative_luminance());
        let (high, low) = if first >= second {
            (first, second)
        } else {
            (second, first)
        };
        (high + 0.05) / (low + 0.05)
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Why a colour string was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("expected a #rrggbb colour, got {0:?}")]
pub struct ColorError(String);

impl FromStr for Color {
    type Err = ColorError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits = text
            .strip_prefix('#')
            .ok_or_else(|| ColorError(text.to_owned()))?;
        if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ColorError(text.to_owned()));
        }
        let channel = |start: usize| {
            u8::from_str_radix(&digits[start..start + 2], 16)
                .map_err(|_| ColorError(text.to_owned()))
        };
        Ok(Self {
            r: channel(0)?,
            g: channel(2)?,
            b: channel(4)?,
        })
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_str(&text).map_err(serde::de::Error::custom)
    }
}

/// The non-syntax tokens. These are the ones a UI reads by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiToken {
    /// The main background.
    Bg,
    /// Panels, cards, and the sidebar.
    BgPanel,
    /// Body text.
    Text,
    /// Hints, line numbers, secondary labels.
    TextMuted,
    /// Selection, focus, the current item.
    Accent,
    /// A brighter accent, for the one thing that should be noticed.
    AccentHigh,
    /// Hairline separators and panel edges.
    Border,
    /// Success status.
    Success,
    /// Warning status.
    Warning,
    /// Error status and destructive actions.
    Danger,
}

impl UiToken {
    /// Every UI token, in display order.
    pub const ALL: [Self; 10] = [
        Self::Bg,
        Self::BgPanel,
        Self::Text,
        Self::TextMuted,
        Self::Accent,
        Self::AccentHigh,
        Self::Border,
        Self::Success,
        Self::Warning,
        Self::Danger,
    ];

    /// The token's name, as written in a theme file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bg => "bg",
            Self::BgPanel => "bg_panel",
            Self::Text => "text",
            Self::TextMuted => "text_muted",
            Self::Accent => "accent",
            Self::AccentHigh => "accent_high",
            Self::Border => "border",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Danger => "danger",
        }
    }
}

impl fmt::Display for UiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The syntax-highlighting tokens, shared by code blocks, diffs, and the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntaxToken {
    /// Comments, including documentation comments.
    Comment,
    /// Keywords and control flow.
    Keyword,
    /// String and character literals.
    String,
    /// Numeric literals.
    Number,
    /// Type names and traits.
    Type,
    /// Function and method names.
    Function,
    /// Variables and parameters.
    Variable,
    /// Constants and enum variants.
    Constant,
    /// Operators and punctuation that carries meaning.
    Operator,
    /// Brackets, commas, and other structure.
    Punctuation,
}

impl SyntaxToken {
    /// Every syntax token, in display order.
    pub const ALL: [Self; 10] = [
        Self::Comment,
        Self::Keyword,
        Self::String,
        Self::Number,
        Self::Type,
        Self::Function,
        Self::Variable,
        Self::Constant,
        Self::Operator,
        Self::Punctuation,
    ];

    /// The token's name, as written in a theme file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::Keyword => "keyword",
            Self::String => "string",
            Self::Number => "number",
            Self::Type => "type",
            Self::Function => "function",
            Self::Variable => "variable",
            Self::Constant => "constant",
            Self::Operator => "operator",
            Self::Punctuation => "punctuation",
        }
    }
}

impl fmt::Display for SyntaxToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

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

/// A complete token set: every token has a value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Palette {
    /// Non-syntax tokens.
    pub ui: BTreeMap<UiToken, Color>,
    /// Syntax tokens.
    pub syntax: BTreeMap<SyntaxToken, Color>,
}

impl Palette {
    /// A UI token, if the theme resolved it.
    #[must_use]
    pub fn ui(&self, token: UiToken) -> Option<Color> {
        self.ui.get(&token).copied()
    }

    /// A syntax token, if the theme resolved it.
    #[must_use]
    pub fn syntax(&self, token: SyntaxToken) -> Option<Color> {
        self.syntax.get(&token).copied()
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

/// A theme after inheritance, ready to hand to a surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTheme {
    /// Namespaced identifier.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Light or dark.
    pub appearance: ThemeAppearance,
    /// Whether this came out of the binary rather than the store.
    pub built_in: bool,
    /// Extension that provided it.
    pub extension_id: String,
    /// The complete token set.
    pub palette: Palette,
}
