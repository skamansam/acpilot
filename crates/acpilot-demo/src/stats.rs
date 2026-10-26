//! What the stats bar counts, one row per integration (P6-13).
//!
//! The bar exists so a person can see the cost of a session while it runs instead
//! of reading it in a bill. That is a design decision as much as a data one, which
//! is why the shape is here: provider, model, tokens in each direction, money,
//! latency, and whether the thing answered at all.

/// One provider and model, as the bar shows it.
#[derive(Debug, Clone)]
pub struct Integration {
    /// The provider, as a person names it.
    pub provider: String,
    /// The model it is running.
    pub model: String,
    /// Tokens sent.
    pub tokens_in: u64,
    /// Tokens received.
    pub tokens_out: u64,
    /// What the turn has cost so far.
    pub cost_usd: f64,
    /// Time to first token on the last turn.
    pub latency_ms: u64,
    /// Whether the last request answered. False is drawn, not hidden: a provider
    /// that stopped answering is the one thing this bar has to say.
    pub healthy: bool,
}

impl Integration {
    /// The total tokens the row counts.
    #[must_use]
    pub fn tokens(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }
}

/// The two integrations this design is argued with: one local, one remote.
#[must_use]
pub fn integrations() -> Vec<Integration> {
    vec![
        Integration {
            provider: "lmstudio".to_owned(),
            model: "qwen2.5-coder-7b".to_owned(),
            tokens_in: 18_412,
            tokens_out: 3_208,
            cost_usd: 0.0,
            latency_ms: 214,
            healthy: true,
        },
        Integration {
            provider: "anthropic".to_owned(),
            model: "claude-sonnet-4".to_owned(),
            tokens_in: 5_120,
            tokens_out: 1_940,
            cost_usd: 0.084,
            latency_ms: 1_380,
            healthy: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_has_a_local_row_and_a_paid_one() {
        let rows = integrations();
        assert!(
            rows.iter().any(|row| row.cost_usd == 0.0),
            "local runs are free"
        );
        assert!(
            rows.iter().any(|row| row.cost_usd > 0.0),
            "a paid row shows money"
        );
        assert_eq!(rows[0].tokens(), 18_412 + 3_208);
    }
}
