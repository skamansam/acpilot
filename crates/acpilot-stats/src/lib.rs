//! The integration stats bar, as all three surfaces draw it.
//!
//! `IntegrationStats` is what the daemon reports; this is what a person reads. One
//! row per integration, a context track and a credit track, and the words that go
//! beside them — so the terminal, the window and the browser cannot disagree about
//! what `18.4k / 32.8k` means, or whether an unreported window is a full bar.
//!
//! # What is *not* here
//!
//! Drawing. A track is a fill ratio and a sentence; how many cells or pixels it
//! takes, what colour an estimate is, and which border it is drawn inside are the
//! surfaces' answers. The browser is handed these rows as JSON for the same reason
//! the terminal is handed them as Rust: a second implementation of the arithmetic
//! in JavaScript is one that drifts the first time a denominator rule changes.
//!
//! # The three rules
//!
//! 1. **A missing denominator is `None`, never a full bar.** If nobody reported a
//!    context window, the track is drawn as indeterminate — a person can still read
//!    `4.1k / ?` and ask what the window is.
//! 2. **The source travels with the figure.** `estimated` and `configured` are not
//!    worse than `server`, but they are not the same, and a surface that styles them
//!    alike is claiming a measurement it does not have.
//! 3. **A quiet integration keeps its row.** The list is what the daemon reported,
//!    in the daemon's order, so a provider that stopped answering does not make the
//!    bar reflow; its age is what says it stopped.

use std::time::Duration;

use acpilot_core::stats::{context_fill, credit_fill};
use acpilot_proto::stats::{
    ContextStats, CreditStats, IntegrationKind, IntegrationStats, StatSource,
};
use serde::{Deserialize, Serialize};

/// How old a reading may be before its row says so.
///
/// Five minutes is not a data policy, it is a "the number you are reading is from
/// another conversation" policy: the TUI asks every fifteen seconds, and a browser
/// page asks when it is loaded or refreshed, so a row that is minutes old is a row
/// something stopped updating.
pub const STALE_AFTER: Duration = Duration::from_secs(300);

/// One measured quantity, ready to draw.
///
/// The fill and the words are separate because they answer different questions: the
/// track says how full, the text says how much — and when `fill` is `None` the text
/// is the only thing on screen, which is exactly the case where a person needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Fraction in use, clamped to `0.0..=1.0`, or [`None`] when the denominator was
    /// not reported: an indeterminate track is drawn as neither full nor empty.
    pub fill: Option<f32>,
    /// The figures, in the shortest honest spelling: `18.4k / 32.8k`.
    pub text: String,
    /// Where the figures came from, so a surface can style them and a person can
    /// ask for them by name.
    pub source: StatSource,
}

/// One integration's row in the bar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// Provider or agent identifier, as the daemon spells it.
    pub id: String,
    /// Which kind of integration this is.
    pub kind: IntegrationKind,
    /// Display label: a bar is drawn without a second lookup.
    pub label: String,
    /// Context usage, absent when the integration has no conversation attached.
    ///
    /// Absent on the wire as well as in Rust, exactly as `IntegrationStats` says
    /// it: a page that receives `null` and a page that receives no key are the
    /// same page, but only one of them says the field is a track when it exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Track>,
    /// Credit balance, absent for local servers and plans that expose none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<Track>,
    /// RFC 3339 timestamp of the last observation, as it arrived.
    pub updated_at: String,
    /// Whether the reading is old enough that the row should say so.
    pub stale: bool,
}

impl Row {
    /// The sources behind this row's figures, in one line.
    ///
    /// The bar itself has no room for provenance, and provenance is the difference
    /// between a number someone measured and a number this client guessed — so it is
    /// one keystroke away rather than absent. A figure the integration did not report
    /// is named as such instead of being left out, so a missing credit bar reads as
    /// "nobody said" rather than "nothing owed".
    #[must_use]
    pub fn sources(&self) -> String {
        format!(
            "context {} · credits {}",
            source_word(self.context.as_ref().map(|track| track.source)),
            source_word(self.credits.as_ref().map(|track| track.source)),
        )
    }

    /// Whether this row has anything to draw in a track.
    ///
    /// A row with neither is still a row — the bar lists what exists, including what
    /// is quiet — but a surface can say "nothing reported yet" instead of drawing two
    /// hundred empty cells.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.context.is_none() && self.credits.is_none()
    }
}

/// Build the bar's rows from what the daemon reported.
///
/// Order is the daemon's order, one row per integration: a row that arrives or
/// leaves changes the bar's length, and nothing here reorders behind the daemon's
/// back, because a bar that rearranged itself would be one nobody can watch.
#[must_use]
pub fn rows(stats: &[IntegrationStats], now: jiff::Timestamp) -> Vec<Row> {
    stats
        .iter()
        .map(|integration| Row {
            id: integration.id.clone(),
            kind: integration.kind,
            label: integration.label.clone(),
            context: integration.context.as_ref().map(context_track),
            credits: integration.credits.as_ref().map(credit_track),
            stale: is_stale(&integration.updated_at, now),
            updated_at: integration.updated_at.clone(),
        })
        .collect()
}

/// Build the bar's rows as of now.
///
/// The clock-reading shorthand for [`rows`]: a surface that has just received the
/// daemon's answer has no other "now" to pass, and a client that had to depend on a
/// timestamp library to read a bar would be a client with a dependency it does not
/// use.
#[must_use]
pub fn rows_now(stats: &[IntegrationStats]) -> Vec<Row> {
    rows(stats, jiff::Timestamp::now())
}

/// A context reading as a track.
fn context_track(stats: &ContextStats) -> Track {
    Track {
        fill: context_fill(stats.used_tokens, stats.window_tokens),
        text: match stats.window_tokens {
            Some(window) => format!("{} / {}", tokens(stats.used_tokens), tokens(window)),
            // The question mark is the point of this arm: the window is what nobody
            // reported, and a bare "18.4k" would read as a total rather than a
            // numerator with nothing under it.
            None => format!("{} / ?", tokens(stats.used_tokens)),
        },
        source: stats.source,
    }
}

/// A credit reading as a track.
///
/// The denominator follows `acpilot_core::stats::credit_fill`'s order — the plan
/// ceiling first, then spend plus balance — and the sentence follows the same order,
/// so the number beside the bar and the bar itself are the same reading.
fn credit_track(stats: &CreditStats) -> Track {
    let fill = credit_fill(stats.used, stats.remaining, stats.limit);
    let currency = stats.currency.trim();
    let text = match (stats.limit, stats.used, stats.remaining) {
        (Some(limit), _, _) => {
            let spent = stats
                .used
                .or_else(|| stats.remaining.map(|left| limit - left));
            match spent {
                Some(spent) => format!("{} / {} {}", amount(spent), amount(limit), currency),
                None => format!("? / {} {}", amount(limit), currency),
            }
        }
        (None, _, Some(remaining)) if !currency.is_empty() => {
            format!("{} {currency} left", amount(remaining))
        }
        (None, Some(used), _) if !currency.is_empty() => {
            format!("{} {currency} used", amount(used))
        }
        // A balance endpoint that reported only a number, or nothing at all: the
        // source on the row is what says so, and inventing a unit here would be a
        // second way to be wrong.
        _ => "unknown".to_owned(),
    };
    Track {
        fill,
        text: text.trim_end().to_owned(),
        source: stats.source,
    }
}

/// Whether a reading is too old, or too undatable, to look current.
///
/// A timestamp this client cannot read is treated as old: the row's job is to make a
/// frozen number visible, and "I cannot tell how old this is" is not a reason to show
/// it as fresh.
fn is_stale(updated_at: &str, now: jiff::Timestamp) -> bool {
    let Ok(observed) = updated_at.parse::<jiff::Timestamp>() else {
        return true;
    };
    // A clock that moved backwards (an NTP correction, two machines, a container)
    // makes a reading look like it came from the future; that is not staleness.
    let seconds = now.as_second() - observed.as_second();
    seconds > STALE_AFTER.as_secs() as i64
}

/// The word a person reads for where a figure came from.
///
/// `None` is a figure that is absent rather than unreported: there is no credit bar
/// for a local server, and the row says that instead of `unknown`, which would imply
/// somebody looked and failed.
fn source_word(source: Option<StatSource>) -> &'static str {
    match source {
        Some(StatSource::Server) => "server",
        Some(StatSource::Configured) => "configured",
        Some(StatSource::Estimated) => "estimated",
        Some(StatSource::Unknown) => "unknown",
        None => "—",
    }
}

/// Tokens, short enough for a bar and precise enough to act on.
///
/// One decimal place, because the second one changes four times a second during a
/// turn and a number that flickers is one nobody reads. Under a thousand the count
/// is exact: a bar that rounded `940` to `0.9k` would be hiding the end of a window.
#[must_use]
pub fn tokens(count: u32) -> String {
    let count = f64::from(count);
    if count < 1_000.0 {
        return format!("{count:.0}");
    }
    if count < 1_000_000.0 {
        return format!("{:.1}k", count / 1_000.0);
    }
    format!("{:.1}M", count / 1_000_000.0)
}

/// Credits, in the provider's own unit.
///
/// Two decimals always: a balance is money-shaped even when the unit is not money,
/// and a bar whose numbers change width as they cross ten is a bar that jitters.
#[must_use]
pub fn amount(value: f64) -> String {
    format!("{value:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_proto::stats::{ContextStats, CreditStats, IntegrationKind};

    /// A timestamp this many seconds before `now`, as the wire spells it.
    fn ago(now: jiff::Timestamp, seconds: i64) -> String {
        (now - jiff::SignedDuration::from_secs(seconds)).to_string()
    }

    fn provider(id: &str, label: &str, updated_at: String) -> IntegrationStats {
        IntegrationStats {
            id: id.to_owned(),
            kind: IntegrationKind::Provider,
            label: label.to_owned(),
            context: None,
            credits: None,
            updated_at,
        }
    }

    fn now() -> jiff::Timestamp {
        "2026-10-30T12:00:00Z".parse().expect("a timestamp")
    }

    #[test]
    fn a_reported_window_is_a_ratio_and_the_two_numbers_behind_it() {
        let stats = vec![IntegrationStats {
            context: Some(ContextStats {
                used_tokens: 16_384,
                window_tokens: Some(32_768),
                source: StatSource::Server,
            }),
            ..provider("lmstudio", "lmstudio", now().to_string())
        }];
        let rows = rows(&stats, now());
        let track = rows[0].context.as_ref().expect("a context track");
        assert_eq!(track.fill, Some(0.5));
        assert_eq!(track.text, "16.4k / 32.8k");
        assert_eq!(track.source, StatSource::Server);
        assert!(rows[0].credits.is_none(), "a local server owes nothing");
        assert!(!rows[0].stale);
    }

    #[test]
    fn a_window_nobody_reported_is_indeterminate_rather_than_full() {
        let stats = vec![IntegrationStats {
            context: Some(ContextStats {
                used_tokens: 4_096,
                window_tokens: None,
                source: StatSource::Estimated,
            }),
            ..provider("claude-code", "claude-code", now().to_string())
        }];
        let rows = rows(&stats, now());
        let track = rows[0].context.as_ref().expect("a context track");
        assert_eq!(track.fill, None, "no window means no fill to draw");
        assert_eq!(track.text, "4.1k / ?");
        assert_eq!(track.source, StatSource::Estimated);
        assert_eq!(
            rows[0].sources(),
            "context estimated · credits —",
            "a source a person can read, and an absent figure named as absent"
        );
    }

    #[test]
    fn credits_are_measured_against_the_plan_ceiling() {
        let stats = vec![IntegrationStats {
            credits: Some(CreditStats {
                used: Some(8.4),
                remaining: Some(11.6),
                limit: Some(20.0),
                currency: "USD".to_owned(),
                source: StatSource::Configured,
            }),
            ..provider("anthropic", "anthropic", now().to_string())
        }];
        let rows = rows(&stats, now());
        let track = rows[0].credits.as_ref().expect("a credit track");
        assert!((track.fill.expect("a fill") - 0.42).abs() < 1e-6);
        assert_eq!(track.text, "8.40 / 20.00 USD");
        assert_eq!(track.source, StatSource::Configured);
    }

    #[test]
    fn a_balance_with_no_ceiling_shows_the_number_and_no_fill() {
        let stats = vec![IntegrationStats {
            credits: Some(CreditStats {
                used: None,
                remaining: Some(4.5),
                limit: None,
                currency: "credits".to_owned(),
                source: StatSource::Server,
            }),
            ..provider("openrouter", "openrouter", now().to_string())
        }];
        let track = rows(&stats, now())[0]
            .credits
            .clone()
            .expect("a credit track");
        assert_eq!(track.fill, None);
        assert_eq!(
            track.text, "4.50 credits left",
            "a balance is still worth reading when the ceiling is unknown"
        );
    }

    #[test]
    fn a_credit_reading_with_nothing_in_it_says_so() {
        let stats = vec![IntegrationStats {
            credits: Some(CreditStats {
                used: None,
                remaining: None,
                limit: None,
                currency: String::new(),
                source: StatSource::Unknown,
            }),
            ..provider("quiet", "quiet", now().to_string())
        }];
        let track = rows(&stats, now())[0].credits.clone().expect("a row");
        assert_eq!(track.fill, None);
        assert_eq!(track.text, "unknown");
        assert_eq!(
            rows(&stats, now())[0].sources(),
            "context — · credits unknown"
        );
    }

    #[test]
    fn every_integration_keeps_its_row_in_the_daemons_order() {
        let stats = vec![
            provider("lmstudio", "lmstudio", now().to_string()),
            provider("anthropic", "anthropic", now().to_string()),
            IntegrationStats {
                kind: IntegrationKind::Agent,
                ..provider("claude-code", "Claude Code", now().to_string())
            },
        ];
        let rows = rows(&stats, now());
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["lmstudio", "anthropic", "claude-code"],
            "a quiet integration is a row, not an absence"
        );
        assert!(rows.iter().all(Row::is_quiet), "nothing was reported");
        assert_eq!(rows[2].kind, IntegrationKind::Agent);
        assert_eq!(rows[2].label, "Claude Code");
    }

    #[test]
    fn an_old_reading_is_marked_and_a_fresh_one_is_not() {
        let at = now();
        let stats = vec![
            provider("fresh", "fresh", ago(at, 1)),
            provider("edge", "edge", ago(at, STALE_AFTER.as_secs() as i64 - 1)),
            provider("old", "old", ago(at, STALE_AFTER.as_secs() as i64 + 1)),
        ];
        let rows = rows(&stats, at);
        assert!(!rows[0].stale);
        assert!(!rows[1].stale, "one second inside the window is inside it");
        assert!(rows[2].stale);
        assert_eq!(
            rows[2].updated_at,
            ago(at, STALE_AFTER.as_secs() as i64 + 1)
        );
    }

    #[test]
    fn a_reading_from_the_future_or_none_at_all_still_lands_in_a_row() {
        let at = now();
        let stats = vec![
            provider("skewed", "skewed", ago(at, -60)),
            provider("undatable", "undatable", "not a timestamp".to_owned()),
        ];
        let rows = rows(&stats, at);
        assert!(!rows[0].stale, "a clock that moved backwards is not age");
        assert!(
            rows[1].stale,
            "a number nobody can date must not look current"
        );
    }

    #[test]
    fn the_numbers_are_short_enough_for_a_bar() {
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1_000), "1.0k");
        assert_eq!(tokens(18_412), "18.4k");
        assert_eq!(tokens(200_000), "200.0k");
        assert_eq!(tokens(1_200_000), "1.2M");
        assert_eq!(amount(8.4), "8.40");
        assert_eq!(amount(-1.0), "-1.00");
        assert_eq!(amount(1234.5), "1234.50");
    }
}
