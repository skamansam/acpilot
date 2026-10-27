//! A theme solved from the terminal's own two colours.
//!
//! `theme_follow_terminal` names no theme. Instead the terminal is asked what its
//! background and foreground are (`OSC 11` and `OSC 10`, via
//! `terminal-colorsaurus` in the TUI), and this module solves the complete token
//! set around that pair: the panel is pushed out of the background into the window
//! [`crate::contrast`] holds every theme to, the ink is pushed out until it clears
//! WCAG AA on the panel, and the roles the terminal has no colour for — a status
//! red, a keyword amber — start from a hue and take the lightness that clears the
//! same floor.
//!
//! Nothing here is a third palette. The answer is a [`ResolvedTheme`], drawn by the
//! same code that draws night flight, and it is *validated* by the same rules: the
//! audit runs on the result and a palette that fails it is returned as [`None`]
//! rather than shown. That is what makes this a solver rather than a guess.
//!
//! Two consequences are worth knowing before reading the numbers:
//!
//! - A terminal whose background sits near the middle of the ramp cannot carry a
//!   UI. Pure white ink on a panel lifted off `#808080` tops out around 3.3:1, so
//!   the background is deepened towards its own end until the ink has room. The
//!   palette is therefore solved *around* the terminal's pair, not pinned to it.
//! - Polarity is read from the pair, never asked for: whichever way the terminal's
//!   own foreground leans is the way the ink runs, so the same call gives a dark
//!   theme on a black terminal and a light one on a white terminal.

use acpilot_proto::extension::ThemeAppearance;

use crate::contrast::{AA_NORMAL_TEXT, PANEL_MAX, PANEL_MIN};
use crate::theme::{Color, Palette, ResolvedTheme, SyntaxToken, UiToken};

/// The id a followed palette is reported under.
///
/// It is not an extension id and never resolves through a registry: it names the
/// answer so a status line can say "Follow terminal" rather than showing a blank.
pub const ID: &str = "acpilot.follow-terminal";

/// The display name, for the status line.
pub const NAME: &str = "Follow terminal";

const WHITE: Color = Color::from_rgb(255, 255, 255);
const BLACK: Color = Color::from_rgb(0, 0, 0);

/// Where in the window [`crate::contrast::PANEL_MIN`]–[`crate::contrast::PANEL_MAX`]
/// the solved panel lands.
///
/// The middle: far enough from both edges that eight-bit rounding on either side
/// cannot push it out.
const PANEL_TARGET: f64 = 1.18;

/// What text needs on the panel: WCAG AA, plus a margin.
///
/// The margin is not decoration. The palette is solved in floating point and drawn
/// as eight bits per channel, and a token that measures 4.50:1 before rounding can
/// measure 4.48:1 afterwards — which is a theme that fails the gate CI enforces.
const TEXT_FLOOR: f64 = AA_NORMAL_TEXT + 0.15;

/// How far the panel is pushed from the background when the ink needs a hairline.
const BORDER_DISTANCE: f64 = 1.5;

/// A hue, in degrees, and the saturation it is drawn at.
///
/// These are hues, not colours: the lightness is solved, which is what lets the same
/// amber work as a keyword on a black terminal and on a white one. The families are
/// the built-ins' — amber for selection, green for success and strings, red for
/// danger, violet for numbers — so a followed terminal still looks like Acpilot
/// rather than like a different product.
const ACCENT_HUE: f64 = 42.0;
const SUCCESS_HUE: f64 = 145.0;
const WARNING_HUE: f64 = 34.0;
const DANGER_HUE: f64 = 0.0;
const STRING_HUE: f64 = 138.0;
const NUMBER_HUE: f64 = 285.0;
const TYPE_HUE: f64 = 210.0;
const FUNCTION_HUE: f64 = 28.0;
const CONSTANT_HUE: f64 = 340.0;

/// The saturation every solved hue is drawn at.
///
/// A fully saturated hue is a traffic light; this is a hint of one, which is what
/// the built-ins are.
const SATURATION: f64 = 0.62;

/// Solve a complete theme around a terminal's background and foreground.
///
/// [`None`] when the result would not pass the contrast audit — which the tests
/// assert cannot happen for a pair of terminal colours, but a solver that cannot
/// say so is a solver whose guarantees are in a comment.
#[must_use]
pub fn solve(background: Color, foreground: Color) -> Option<ResolvedTheme> {
    // The direction the ink runs: the end of the ramp the terminal's own foreground
    // already leans towards. Light ink on a dark terminal gives a dark theme, dark
    // ink on a light one gives a light theme, and neither had to be declared.
    let dark = foreground.relative_luminance() >= background.relative_luminance();
    let pole = if dark { WHITE } else { BLACK };
    let opposite = if dark { BLACK } else { WHITE };

    // A background too close to the middle of the ramp cannot hold both a panel and
    // readable ink on it, so it is deepened towards its own end first. Two per cent
    // per step and sixty steps, which always reaches a pole: the loop cannot spin.
    let mut background = background;
    let mut panel = into_window(background, pole)?;
    let mut steps = 0;
    while panel.contrast_ratio(pole) < TEXT_FLOOR {
        if steps == 60 {
            return None;
        }
        background = mix(background, opposite, 0.02);
        panel = into_window(background, pole)?;
        steps += 1;
    }

    // The ink: the terminal's own foreground, kept verbatim when it already clears
    // the panel, and pushed towards the pole no further than it has to be.
    let text = lift(foreground, pole, panel, TEXT_FLOOR);
    let muted = mute(text, panel, TEXT_FLOOR);
    let border = blend_to(panel, pole, BORDER_DISTANCE);

    let accent = lifted(ACCENT_HUE, dark, panel, pole, TEXT_FLOOR);
    let accent_high = lift(blend_to(accent, pole, 1.3), pole, panel, TEXT_FLOOR);

    let mut palette = Palette::default();
    for (token, colour) in [
        (UiToken::Bg, background),
        (UiToken::BgPanel, panel),
        (UiToken::Text, text),
        (UiToken::TextMuted, muted),
        (UiToken::Accent, accent),
        (UiToken::AccentHigh, accent_high),
        (UiToken::Border, border),
        (
            UiToken::Success,
            lifted(SUCCESS_HUE, dark, panel, pole, TEXT_FLOOR),
        ),
        (
            UiToken::Warning,
            lifted(WARNING_HUE, dark, panel, pole, TEXT_FLOOR),
        ),
        (
            UiToken::Danger,
            lifted(DANGER_HUE, dark, panel, pole, TEXT_FLOOR),
        ),
    ] {
        palette.ui.insert(token, colour);
    }
    for (token, colour) in [
        (SyntaxToken::Comment, muted),
        (SyntaxToken::Keyword, accent),
        (
            SyntaxToken::String,
            lifted(STRING_HUE, dark, panel, pole, TEXT_FLOOR),
        ),
        (
            SyntaxToken::Number,
            lifted(NUMBER_HUE, dark, panel, pole, TEXT_FLOOR),
        ),
        (
            SyntaxToken::Type,
            lifted(TYPE_HUE, dark, panel, pole, TEXT_FLOOR),
        ),
        (
            SyntaxToken::Function,
            lifted(FUNCTION_HUE, dark, panel, pole, TEXT_FLOOR),
        ),
        (SyntaxToken::Variable, text),
        (
            SyntaxToken::Constant,
            lifted(CONSTANT_HUE, dark, panel, pole, TEXT_FLOOR),
        ),
        (SyntaxToken::Operator, muted),
        (SyntaxToken::Punctuation, muted),
    ] {
        palette.syntax.insert(token, colour);
    }

    let theme = ResolvedTheme {
        id: ID.to_owned(),
        name: NAME.to_owned(),
        appearance: if dark {
            ThemeAppearance::Dark
        } else {
            ThemeAppearance::Light
        },
        built_in: true,
        extension_id: ID.to_owned(),
        palette,
    };

    // The same audit every authored theme goes through, run on the answer. A solved
    // palette that fails it is not shown: the caller keeps the theme it was drawing.
    let audit = crate::contrast::audit(&theme);
    if audit.passes(AA_NORMAL_TEXT) && audit.panel_is_inside_window() {
        Some(theme)
    } else {
        None
    }
}

/// A hue at the lightness the panel leaves room for.
///
/// The seed is vivid and mid-toned, then pushed towards the pole until it clears the
/// floor: on a dark theme that lightens it towards a pastel, on a light one it
/// darkens it, which is the same transformation the built-ins were authored by hand.
fn lifted(hue: f64, dark: bool, panel: Color, pole: Color, floor: f64) -> Color {
    let seed = tint(hue, SATURATION, if dark { 0.66 } else { 0.40 });
    lift(seed, pole, panel, floor)
}

/// One channel-wise step from `from` towards `to`.
fn mix(from: Color, to: Color, amount: f64) -> Color {
    let channel = |a: u8, b: u8| {
        let (a, b) = (f64::from(a), f64::from(b));
        (a + (b - a) * amount).round().clamp(0.0, 255.0) as u8
    };
    let (red, green, blue) = from.channels();
    let (to_red, to_green, to_blue) = to.channels();
    Color::from_rgb(
        channel(red, to_red),
        channel(green, to_green),
        channel(blue, to_blue),
    )
}

/// The first step from `from` towards `to` that measures `target` against `from`.
///
/// The nearer of the two bracketing steps wins: a hundred steps along the whole ramp
/// overshoot a small target by a visible amount, and a border that lands at 1.9:1
/// when it asked for 1.5:1 is a wireframe.
fn blend_to(from: Color, to: Color, target: f64) -> Color {
    let mut previous = from;
    for step in 1..=100 {
        let candidate = mix(from, to, f64::from(step) / 100.0);
        if from.contrast_ratio(candidate) >= target {
            let overshoot = from.contrast_ratio(candidate) - target;
            let undershoot = target - from.contrast_ratio(previous);
            return if overshoot <= undershoot {
                candidate
            } else {
                previous
            };
        }
        previous = candidate;
    }
    previous
}

/// The step from `from` towards `to` whose distance from `from` lands inside the
/// panel window, closest to the target.
///
/// Search rather than arithmetic because the mix is per channel in sRGB, so the
/// distance it produces is not the distance a formula predicts. [`None`] when no
/// step lands inside — which means the two colours are the same colour.
fn into_window(from: Color, to: Color) -> Option<Color> {
    let mut best: Option<(f64, Color)> = None;
    for step in 1..=100 {
        let candidate = mix(from, to, f64::from(step) / 100.0);
        let ratio = from.contrast_ratio(candidate);
        if !(PANEL_MIN..=PANEL_MAX).contains(&ratio) {
            continue;
        }
        let distance = (ratio - PANEL_TARGET).abs();
        if best.is_none_or(|(closest, _)| distance < closest) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, colour)| colour)
}

/// `colour` pushed towards `pole` until it clears `floor` against `surface`.
///
/// A colour that already clears the floor is returned untouched: the terminal's own
/// foreground is somebody's choice, and a solver that re-tints it has not followed
/// the terminal.
fn lift(colour: Color, pole: Color, surface: Color, floor: f64) -> Color {
    if colour.contrast_ratio(surface) >= floor {
        return colour;
    }
    for step in 1..=100 {
        let candidate = mix(colour, pole, f64::from(step) / 100.0);
        if candidate.contrast_ratio(surface) >= floor {
            return candidate;
        }
    }
    pole
}

/// `ink` pulled towards `paper` as far as it can go without falling under `floor`.
///
/// This is the only place a colour is allowed to get *closer* to the surface: muted
/// text is ink that has been made as quiet as it can be and still be text.
fn mute(ink: Color, paper: Color, floor: f64) -> Color {
    let mut best = ink;
    for step in 1..=100 {
        let candidate = mix(ink, paper, f64::from(step) / 100.0);
        if candidate.contrast_ratio(paper) < floor {
            break;
        }
        best = candidate;
    }
    best
}

/// HSL to the eight-bit colour a terminal draws.
///
/// Hand-rolled rather than pulled in: one conversion is not a dependency, and this
/// one is a pure function with a test rather than a formula somebody has to trust.
fn tint(hue: f64, saturation: f64, lightness: f64) -> Color {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = (hue.rem_euclid(360.0)) / 60.0;
    let second = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let (red, green, blue) = match sector.floor() as u32 % 6 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let base = lightness - chroma / 2.0;
    let channel = |value: f64| ((value + base) * 255.0).round().clamp(0.0, 255.0) as u8;
    Color::from_rgb(channel(red), channel(green), channel(blue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contrast;

    /// Every pair the tests below try: dark, light, and the awkward middle.
    fn pairs() -> Vec<(Color, Color, &'static str)> {
        vec![
            (BLACK, WHITE, "a black terminal, white ink"),
            (
                Color::from_rgb(0x0b, 0x0b, 0x0b),
                Color::from_rgb(0xdc, 0xdc, 0xdc),
                "a dark terminal, off-white ink",
            ),
            (
                Color::from_rgb(0x0d, 0x2b, 0x36),
                Color::from_rgb(0x93, 0xa1, 0xa1),
                "solarized dark",
            ),
            (
                Color::from_rgb(0xfd, 0xf6, 0xe3),
                Color::from_rgb(0x65, 0x7b, 0x83),
                "solarized light",
            ),
            (WHITE, BLACK, "a white terminal, black ink"),
            (
                Color::from_rgb(0x80, 0x80, 0x80),
                Color::from_rgb(0xf0, 0xf0, 0xf0),
                "the middle of the ramp, light ink",
            ),
            (
                Color::from_rgb(0x9a, 0x9a, 0x9a),
                Color::from_rgb(0x20, 0x20, 0x20),
                "the middle of the ramp, dark ink",
            ),
        ]
    }

    #[test]
    fn every_terminal_pair_solves_a_theme_that_passes_the_audit() {
        for (background, foreground, label) in pairs() {
            let theme = solve(background, foreground)
                .unwrap_or_else(|| panic!("{label} did not solve a theme"));
            let audit = contrast::audit(&theme);
            assert!(
                audit.passes(AA_NORMAL_TEXT),
                "{label}:\n{}",
                audit.report(&theme.id, AA_NORMAL_TEXT)
            );
            assert!(
                audit.panel_is_inside_window(),
                "{label}:\n{}",
                audit.report(&theme.id, AA_NORMAL_TEXT)
            );
            assert_eq!(theme.id, ID);
            assert_eq!(theme.name, NAME);
            for token in UiToken::ALL {
                assert!(
                    theme.palette.ui(token).is_some(),
                    "{label}: {token} is unset"
                );
            }
            for token in SyntaxToken::ALL {
                assert!(
                    theme.palette.syntax(token).is_some(),
                    "{label}: {token} is unset"
                );
            }
        }
    }

    #[test]
    fn the_polarity_is_read_from_the_terminal_not_asked_for() {
        let dark = solve(BLACK, WHITE).expect("a dark terminal");
        assert_eq!(dark.appearance, ThemeAppearance::Dark);

        let light = solve(WHITE, BLACK).expect("a light terminal");
        assert_eq!(light.appearance, ThemeAppearance::Light);
    }

    #[test]
    fn a_foreground_that_already_clears_the_panel_is_kept_verbatim() {
        let foreground = Color::from_rgb(0xff, 0xcc, 0x00);
        let theme = solve(BLACK, foreground).expect("an amber terminal");
        assert_eq!(
            theme.palette.ui(UiToken::Text),
            Some(foreground),
            "the terminal's own ink, not a re-tint of it"
        );
    }

    #[test]
    fn a_background_in_the_middle_is_deepened_until_the_ink_has_room() {
        let background = Color::from_rgb(0x80, 0x80, 0x80);
        let theme = solve(background, Color::from_rgb(0xf0, 0xf0, 0xf0)).expect("solvable");
        let solved = theme.palette.ui(UiToken::Bg).expect("bg");
        assert!(
            solved.relative_luminance() < background.relative_luminance(),
            "the solved background is further from the middle than the terminal's: {} vs {}",
            solved,
            background
        );
        assert!(
            contrast::audit(&theme).passes(AA_NORMAL_TEXT),
            "and the ink it left itself room for clears AA"
        );
    }

    #[test]
    fn the_panel_and_the_ink_move_in_opposite_directions() {
        // The ladder: the panel lifts off the background on the side the ink runs,
        // so muted text is ink between the two rather than a second surface.
        let theme = solve(BLACK, WHITE).expect("a dark terminal");
        let background = theme.palette.ui(UiToken::Bg).expect("bg");
        let panel = theme.palette.ui(UiToken::BgPanel).expect("panel");
        let text = theme.palette.ui(UiToken::Text).expect("text");
        let muted = theme.palette.ui(UiToken::TextMuted).expect("muted");
        assert!(panel.relative_luminance() > background.relative_luminance());
        assert!(text.relative_luminance() > panel.relative_luminance());
        assert!(muted.relative_luminance() < text.relative_luminance());
        assert!(muted.relative_luminance() > panel.relative_luminance());
    }

    #[test]
    fn the_solver_is_a_function_of_the_pair() {
        let first = solve(Color::from_rgb(0x12, 0x12, 0x12), WHITE).expect("solvable");
        let again = solve(Color::from_rgb(0x12, 0x12, 0x12), WHITE).expect("solvable");
        assert_eq!(first, again, "no clock, no randomness, no second guess");
    }

    #[test]
    fn a_hue_is_a_hue_at_any_lightness() {
        // Every sector of the wheel stays a colour rather than collapsing to grey,
        // which is the one failure that would make a solved palette monochrome.
        for hue in [0.0, 42.0, 120.0, 200.0, 285.0, 340.0, 359.0] {
            let colour = tint(hue, 0.6, 0.5);
            let (red, green, blue) = colour.channels();
            assert!(
                red != green || green != blue,
                "hue {hue} came out grey: {colour}"
            );
        }
    }
}
