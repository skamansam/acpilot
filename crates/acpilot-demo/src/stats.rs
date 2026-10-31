//! What the stats bar counts, one row per integration (P6-13).
//!
//! The bar exists so a person can see what a session is costing while it runs
//! instead of reading it in a bill — and, just as much, so a person can see what
//! nobody is reporting. These fixtures are the design argued against, which is why
//! they cover every case the bar has to draw rather than the two that look good in
//! a screenshot:
//!
//! | Row | What it exercises |
//! | --- | --- |
//! | `lmstudio` | a local server: a context bar from the server, no credit bar at all |
//! | `anthropic` | a paid provider: a plan ceiling, so spend and balance have a denominator |
//! | `claude-code` | an agent estimating from its own transcript — styled as an estimate |
//! | `openrouter` | a quiet balance with no ceiling, and a reading old enough to be marked |
//!
//! The protocol type is the point: a bar drawn from these is drawn from the rows the
//! daemon would send, so wiring it up changes the source and not the drawing.

use acpilot_proto::stats::{
    ContextStats, CreditStats, IntegrationKind, IntegrationStats, StatSource,
};

/// How old the one stale row is, in minutes.
const STALE_MINUTES: i64 = 18;

/// The integrations this design is argued with: one local, one paid, one agent, and
/// one that went quiet.
///
/// `now` is a parameter rather than read here so a test can draw the same bar twice
/// and so "eighteen minutes ago" is eighteen minutes rather than whenever the
/// fixture was written.
#[must_use]
pub fn integrations(now: jiff::Timestamp) -> Vec<IntegrationStats> {
    let now_text = now.to_string();
    let stale = (now - jiff::SignedDuration::from_secs(STALE_MINUTES * 60)).to_string();
    vec![
        IntegrationStats {
            id: "lmstudio".to_owned(),
            kind: IntegrationKind::Provider,
            // The contract has no model field: a bar is a row per integration, and
            // which model a provider is serving belongs to the lab's answer, not to
            // the cost of a turn. A label that carried it would make every row twice
            // as wide for one word.
            label: "lmstudio".to_owned(),
            context: Some(ContextStats {
                used_tokens: 18_412,
                window_tokens: Some(32_768),
                source: StatSource::Server,
            }),
            // A local server has no balance to read, and the bar says that by
            // drawing no credit track rather than a zero.
            credits: None,
            updated_at: now_text.clone(),
        },
        IntegrationStats {
            id: "anthropic".to_owned(),
            kind: IntegrationKind::Provider,
            label: "anthropic".to_owned(),
            context: Some(ContextStats {
                used_tokens: 5_120,
                window_tokens: Some(200_000),
                source: StatSource::Server,
            }),
            credits: Some(CreditStats {
                used: Some(8.4),
                remaining: Some(11.6),
                limit: Some(20.0),
                currency: "USD".to_owned(),
                source: StatSource::Configured,
            }),
            updated_at: now_text,
        },
        IntegrationStats {
            id: "claude-code".to_owned(),
            kind: IntegrationKind::Agent,
            label: "Claude Code".to_owned(),
            // No window: an agent reports how much it has written, and nobody says
            // what its window is. The track is drawn in the estimated style and the
            // fill is indeterminate.
            context: Some(ContextStats {
                used_tokens: 41_000,
                window_tokens: None,
                source: StatSource::Estimated,
            }),
            credits: None,
            updated_at: stale.clone(),
        },
        IntegrationStats {
            id: "openrouter".to_owned(),
            kind: IntegrationKind::Provider,
            label: "openrouter".to_owned(),
            // A provider that has not served a turn: no conversation, so no context
            // row — and a balance with no plan ceiling behind it, which shows the
            // number and draws no fill.
            context: None,
            credits: Some(CreditStats {
                used: None,
                remaining: Some(4.5),
                limit: None,
                currency: "credits".to_owned(),
                source: StatSource::Server,
            }),
            updated_at: stale,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpilot_stats::{Row, rows};

    fn at() -> jiff::Timestamp {
        "2026-10-30T12:00:00Z".parse().expect("a timestamp")
    }

    fn bar() -> Vec<Row> {
        rows(&integrations(at()), at())
    }

    #[test]
    fn the_bar_has_one_row_per_case_it_has_to_draw() {
        let bar = bar();
        assert_eq!(bar.len(), 4);
        assert_eq!(
            bar.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["lmstudio", "anthropic", "claude-code", "openrouter"],
            "the daemon's order, which a bar must not rearrange"
        );
        assert_eq!(bar[2].kind, IntegrationKind::Agent);
    }

    #[test]
    fn a_local_server_has_a_context_track_and_no_credit_track() {
        let row = &bar()[0];
        let context = row.context.as_ref().expect("a context track");
        assert_eq!(context.text, "18.4k / 32.8k");
        assert_eq!(context.fill, Some(0.56188965), "{:?}", context.fill);
        assert!(row.credits.is_none(), "nothing to owe a local server");
        assert!(!row.stale);
    }

    #[test]
    fn a_plan_ceiling_makes_the_credit_track_measurable() {
        let credits = bar()[1].credits.clone().expect("a credit track");
        assert_eq!(credits.text, "8.40 / 20.00 USD");
        assert!((credits.fill.expect("a fill") - 0.42).abs() < 1e-6);
        assert_eq!(credits.source, StatSource::Configured);
    }

    #[test]
    fn an_estimate_is_drawn_as_one_and_an_unknown_window_as_indeterminate() {
        let row = &bar()[2];
        let context = row.context.as_ref().expect("a context track");
        assert_eq!(context.source, StatSource::Estimated);
        assert_eq!(context.fill, None, "no window means no fill to draw");
        assert_eq!(context.text, "41.0k / ?");
        assert!(
            row.stale,
            "the design needs a row where the reading went quiet"
        );
        assert_eq!(row.sources(), "context estimated · credits —");
    }

    #[test]
    fn a_balance_with_no_ceiling_still_reads_and_still_has_no_fill() {
        let row = &bar()[3];
        assert!(row.context.is_none(), "no conversation, so no context row");
        let credits = row.credits.clone().expect("a balance");
        assert_eq!(credits.text, "4.50 credits left");
        assert_eq!(credits.fill, None);
    }
}
