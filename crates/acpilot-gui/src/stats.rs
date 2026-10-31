//! What the window does with the shared stats rows.
//!
//! `acpilot_stats::Row` is the measurement: the fill ratio, the sentence beside it
//! and the source that produced it. This module is the window's half of the bar —
//! which glyph names a row, which theme token a source is drawn in, and how large a
//! track is. It is plain data, so the answers are tested without a display; the
//! view in `crate::app` only arranges them.
//!
//! Two of these are the same decisions the terminal's bar makes, and they are made
//! the same way on purpose: a source is coloured by where the figure came from
//! rather than by how full the track is, so an estimate is never painted as a
//! measurement, and a figure with no denominator gets a mark that is neither full
//! nor empty.

use acpilot_extensions::UiToken;
use acpilot_proto::stats::{IntegrationKind, StatSource};
use acpilot_stats::Track;

/// How wide a track is drawn, in pixels.
///
/// Wide enough that a half is visibly not a third, narrow enough that two
/// integrations and a credit track fit side by side in the default window.
pub const TRACK_WIDTH: f32 = 72.0;

/// How tall a measured track is, in pixels.
pub const TRACK_HEIGHT: f32 = 5.0;

/// The height of the mark drawn for a figure with no denominator.
///
/// A hairline, not a full or an empty track: it reads as a placeholder, and it is
/// the one shape that cannot be mistaken for a measurement.
pub const INDETERMINATE_HEIGHT: f32 = 1.0;

/// The glyph that names a row's kind.
///
/// The same pair the terminal uses, so a screenshot of either surface is read the
/// same way: a hollow diamond is a provider, a filled one an agent.
#[must_use]
pub const fn marker(kind: IntegrationKind) -> &'static str {
    match kind {
        IntegrationKind::Provider => "◇",
        IntegrationKind::Agent => "◆",
    }
}

/// The token a track is filled with.
///
/// The source, not the level: green is "the server said so", the accent is "a plan
/// stated it", amber is "this client counted", and a figure nobody reported has no
/// fill to colour at all.
#[must_use]
pub const fn source_token(source: StatSource) -> UiToken {
    match source {
        StatSource::Server => UiToken::Success,
        StatSource::Configured => UiToken::Accent,
        StatSource::Estimated => UiToken::Warning,
        StatSource::Unknown => UiToken::TextMuted,
    }
}

/// The token the figures beside a track are written in.
///
/// A number nobody could report is dim, like a hint; every other reading is body
/// text, because it is a fact the window is asserting.
#[must_use]
pub const fn figure_token(source: StatSource) -> UiToken {
    match source {
        StatSource::Unknown => UiToken::TextMuted,
        _ => UiToken::Text,
    }
}

/// How many pixels of a [`TRACK_WIDTH`] track are filled.
///
/// [`None`] is a figure with no denominator — no window reported, no ceiling
/// configured — and it is the caller's business to draw that as neither full nor
/// empty. A non-zero ratio always shows at least a pixel: a track that says
/// "nothing" while a turn is running is a track nobody believes.
#[must_use]
pub fn fill_width(track: &Track) -> Option<f32> {
    let fill = track.fill?;
    let width = fill.clamp(0.0, 1.0) * TRACK_WIDTH;
    Some(if fill > 0.0 { width.max(1.0) } else { 0.0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(fill: Option<f32>) -> Track {
        Track {
            fill,
            text: "18.4k / 32.8k".to_owned(),
            source: StatSource::Server,
        }
    }

    #[test]
    fn the_two_kinds_are_told_apart() {
        assert_eq!(marker(IntegrationKind::Provider), "◇");
        assert_eq!(marker(IntegrationKind::Agent), "◆");
        assert_ne!(
            marker(IntegrationKind::Provider),
            marker(IntegrationKind::Agent),
            "a provider and an agent must not draw the same glyph"
        );
    }

    #[test]
    fn a_track_is_coloured_by_where_the_figure_came_from() {
        assert_eq!(source_token(StatSource::Server), UiToken::Success);
        assert_eq!(source_token(StatSource::Configured), UiToken::Accent);
        assert_eq!(source_token(StatSource::Estimated), UiToken::Warning);
        assert_eq!(source_token(StatSource::Unknown), UiToken::TextMuted);
        assert_ne!(
            source_token(StatSource::Estimated),
            source_token(StatSource::Server),
            "an estimate must not be painted as a measurement"
        );
    }

    #[test]
    fn only_an_unreported_figure_is_dimmed() {
        assert_eq!(figure_token(StatSource::Server), UiToken::Text);
        assert_eq!(figure_token(StatSource::Configured), UiToken::Text);
        assert_eq!(figure_token(StatSource::Estimated), UiToken::Text);
        assert_eq!(figure_token(StatSource::Unknown), UiToken::TextMuted);
    }

    #[test]
    fn a_ratio_is_a_length_and_no_ratio_is_not_one() {
        assert_eq!(fill_width(&track(Some(0.5))), Some(TRACK_WIDTH / 2.0));
        assert_eq!(fill_width(&track(Some(0.0))), Some(0.0));
        assert_eq!(fill_width(&track(Some(1.0))), Some(TRACK_WIDTH));
        assert_eq!(fill_width(&track(None)), None);
    }

    #[test]
    fn a_speck_of_fill_is_visible_and_an_overfull_track_is_clamped() {
        let speck = fill_width(&track(Some(0.001))).expect("a fill");
        assert!(speck >= 1.0, "a running turn shows something: {speck}");
        assert!(speck < TRACK_WIDTH / 4.0, "and not a quarter of the bar");
        assert_eq!(
            fill_width(&track(Some(4.0))),
            Some(TRACK_WIDTH),
            "a denominator that moved under the reading does not overdraw the bar"
        );
        assert_eq!(
            fill_width(&track(Some(-1.0))),
            Some(0.0),
            "and a negative one draws nothing rather than backwards"
        );
    }
}
