//! The colour types every surface shares, and nothing that reads a file.
//!
//! A leaf crate on purpose: the tokens, the palette and the resolved theme are what
//! the terminal, the window and the browser all draw with, and the site's showcase
//! resolves a palette in WebAssembly from this same code. What is *not* here is the
//! theme file format — parsing, `extends` chains and the registry live in
//! `acpilot-extensions`, because they belong to the loader, and a page does not
//! load extensions.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use acpilot_proto::extension::ThemeAppearance;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

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
