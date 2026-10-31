//! Theme tokens, as the values ratatui draws with.
//!
//! The tokens are [`acpilot_extensions::theme`]'s, not this crate's: the terminal,
//! the desktop app and the browser read one theme extension, so "muted text" cannot
//! mean three greys. What lives here is only the conversion — a resolved palette
//! turned into the [`Color`] a terminal draw call takes — plus a fallback that is
//! not a second palette.
//!
//! [`Theme::default`] is *no theme*: every token is [`Color::Reset`], which is the
//! terminal's own foreground and background. That is what a person gets when the
//! theme they named is not installed, and it is deliberately not a copy of night
//! flight — a private palette here would be a fourth place a colour is written
//! down, and the first to drift.

use std::collections::BTreeMap;

use acpilot_extensions::ThemeAppearance;
use acpilot_extensions::theme::{Palette, ResolvedTheme, SyntaxToken, UiToken};
use ratatui::style::Color;

/// A resolved theme, in terminal colours.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Theme {
    name: String,
    dark: bool,
    ui: BTreeMap<UiToken, Color>,
    syntax: BTreeMap<SyntaxToken, Color>,
}

impl Theme {
    /// Convert a resolved palette to terminal colours.
    ///
    /// Truecolour, because that is what the theme's `#rrggbb` values are and a
    /// quantised approximation of a 10-token palette is a different theme. Every
    /// terminal a person would run this in answers 24-bit colour; one that does
    /// not gets the nearest the emulator can make of it.
    #[must_use]
    pub fn from_resolved(resolved: &ResolvedTheme) -> Self {
        let colour = |color: acpilot_extensions::Color| {
            let (red, green, blue) = color.channels();
            Color::Rgb(red, green, blue)
        };
        Self {
            name: resolved.name.clone(),
            dark: matches!(resolved.appearance, ThemeAppearance::Dark),
            ui: resolved
                .palette
                .ui
                .iter()
                .map(|(token, color)| (*token, colour(*color)))
                .collect(),
            syntax: resolved
                .palette
                .syntax
                .iter()
                .map(|(token, color)| (*token, colour(*color)))
                .collect(),
        }
    }

    /// The theme's display name, for the About card and the status line.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the theme expects a dark terminal, so anything choosing a half can.
    #[must_use]
    pub fn is_dark(&self) -> bool {
        self.dark
    }

    /// A UI token, or the terminal's own colour when there is no theme.
    #[must_use]
    pub fn ui(&self, token: UiToken) -> Color {
        self.ui.get(&token).copied().unwrap_or(Color::Reset)
    }

    /// A syntax token, or the terminal's own colour when there is no theme.
    #[must_use]
    pub fn syntax(&self, token: SyntaxToken) -> Color {
        self.syntax.get(&token).copied().unwrap_or(Color::Reset)
    }

    /// The main background.
    #[must_use]
    pub fn bg(&self) -> Color {
        self.ui(UiToken::Bg)
    }

    /// Panels, cards, and the sidebar.
    #[must_use]
    pub fn bg_panel(&self) -> Color {
        self.ui(UiToken::BgPanel)
    }

    /// Body text.
    #[must_use]
    pub fn text(&self) -> Color {
        self.ui(UiToken::Text)
    }

    /// Hints, line numbers, secondary labels.
    #[must_use]
    pub fn text_muted(&self) -> Color {
        self.ui(UiToken::TextMuted)
    }

    /// Selection, focus, the current item.
    #[must_use]
    pub fn accent(&self) -> Color {
        self.ui(UiToken::Accent)
    }

    /// A brighter accent, for the one thing that should be noticed.
    #[must_use]
    pub fn accent_high(&self) -> Color {
        self.ui(UiToken::AccentHigh)
    }

    /// Hairline separators and panel edges.
    #[must_use]
    pub fn border(&self) -> Color {
        self.ui(UiToken::Border)
    }

    /// Success status.
    #[must_use]
    pub fn success(&self) -> Color {
        self.ui(UiToken::Success)
    }

    /// Warning status.
    #[must_use]
    pub fn warning(&self) -> Color {
        self.ui(UiToken::Warning)
    }

    /// Error status and destructive actions.
    #[must_use]
    pub fn danger(&self) -> Color {
        self.ui(UiToken::Danger)
    }
}

/// A theme from a palette, or [`Theme::default`] when the palette has no tokens.
///
/// The fallback is the point: a surface told to draw with a theme it could not
/// resolve draws in the terminal's colours rather than refusing to open.
#[must_use]
pub fn from_palette(palette: &Palette) -> Theme {
    Theme {
        name: String::new(),
        dark: true,
        ui: palette
            .ui
            .iter()
            .map(|(token, color)| {
                let (red, green, blue) = color.channels();
                (*token, Color::Rgb(red, green, blue))
            })
            .collect(),
        syntax: palette
            .syntax
            .iter()
            .map(|(token, color)| {
                let (red, green, blue) = color.channels();
                (*token, Color::Rgb(red, green, blue))
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_extensions::builtin;

    fn night_flight() -> Theme {
        let extensions = builtin::load_all().expect("the built-in extensions load");
        let resolved = acpilot_extensions::ThemeRegistry::new(&extensions)
            .complete("acpilot.night-flight")
            .expect("night flight resolves");
        Theme::from_resolved(&resolved)
    }

    #[test]
    fn the_theme_is_the_token_file_not_a_second_palette() {
        let theme = night_flight();
        assert_eq!(theme.name(), "Night flight");
        assert!(theme.is_dark());
        // The values in `extensions/acpilot.night-flight/night-flight.toml`.
        assert_eq!(theme.bg(), Color::Rgb(0x07, 0x0d, 0x18));
        assert_eq!(theme.accent(), Color::Rgb(0xf7, 0xc6, 0x5c));
        assert_eq!(theme.text_muted(), Color::Rgb(0x97, 0xa5, 0xb8));
        assert_eq!(theme.danger(), Color::Rgb(0xff, 0x7a, 0x7a));
        assert_eq!(
            theme.syntax(SyntaxToken::Keyword),
            Color::Rgb(0xf7, 0xc6, 0x5c)
        );
    }

    #[test]
    fn a_surface_with_no_theme_draws_in_the_terminals_own_colours() {
        let theme = Theme::default();
        assert_eq!(theme.name(), "");
        for token in UiToken::ALL {
            assert_eq!(theme.ui(token), Color::Reset, "{token} is the terminal's");
        }
    }

    #[test]
    fn the_other_built_in_theme_resolves_through_the_same_rule() {
        let extensions = builtin::load_all().expect("the built-in extensions load");
        let resolved = acpilot_extensions::ThemeRegistry::new(&extensions)
            .complete("acpilot.paper")
            .expect("paper resolves");
        let theme = Theme::from_resolved(&resolved);
        assert!(!theme.is_dark(), "paper is the light half of the pair");
        assert_ne!(theme.bg(), night_flight().bg());
    }
}
