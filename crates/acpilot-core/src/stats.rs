//! Fill ratios for the integration stats bar.
//!
//! The bar is drawn by three UIs, so the arithmetic lives here once. The rules
//! are deliberately conservative: an unknown denominator produces `None` (the
//! bar renders as indeterminate), a missing numerator is derived from what was
//! reported, and an over-budget value clamps instead of overflowing the track.
//!
//! Nothing here formats or colours anything; that is presentation.

/// Fraction of a context window in use, clamped to `0.0..=1.0`.
///
/// Returns `None` when no window was reported, or when it is zero. Both cases
/// mean "the bar cannot be drawn", which is the honest answer: substituting a
/// default window is how a truncated prompt goes unnoticed.
#[must_use]
pub fn context_fill(used_tokens: u32, window_tokens: Option<u32>) -> Option<f32> {
    let window = window_tokens?;
    if window == 0 {
        return None;
    }
    // `u32` as `f32` is lossy above 2^24 tokens, but the ratio is what matters
    // and no context window is near that limit. Clamping keeps a bad reading
    // from painting outside the track.
    Some((used_tokens as f32 / window as f32).clamp(0.0, 1.0))
}

/// Fraction of a credit allowance spent, clamped to `0.0..=1.0`.
///
/// The denominator is, in order of preference:
///
/// 1. `limit`, when the plan reports one;
/// 2. `used + remaining`, when both halves were reported.
///
/// The numerator falls back to `limit - remaining` when only the balance was
/// reported, which is what most provider balance endpoints return after a period
/// rolls over. Returns `None` when the denominator is absent, zero, negative or
/// not finite, so no bar is drawn from numbers that cannot mean anything.
#[must_use]
pub fn credit_fill(used: Option<f64>, remaining: Option<f64>, limit: Option<f64>) -> Option<f32> {
    // One corrupt figure invalidates the reading; do not average it away.
    if used.is_some_and(|value| !value.is_finite())
        || remaining.is_some_and(|value| !value.is_finite())
        || limit.is_some_and(|value| !value.is_finite())
    {
        return None;
    }

    let total = match limit {
        // A stated but unusable ceiling is not permission to invent one.
        Some(limit) if limit > 0.0 => limit,
        Some(_) => return None,
        None => match (used, remaining) {
            (Some(used), Some(remaining)) => {
                let total = used + remaining;
                if total > 0.0 {
                    total
                } else {
                    return None;
                }
            }
            _ => return None,
        },
    };

    let spent = match used {
        Some(used) => used,
        None => total - remaining?,
    };

    Some((spent / total).clamp(0.0, 1.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-6
    }

    #[test]
    fn context_is_a_ratio_of_the_reported_window() {
        assert_eq!(context_fill(4096, Some(32768)), Some(0.125));
        assert_eq!(context_fill(0, Some(32768)), Some(0.0));
    }

    #[test]
    fn an_unknown_or_empty_window_is_not_a_full_bar() {
        assert_eq!(context_fill(4096, None), None);
        assert_eq!(context_fill(4096, Some(0)), None);
    }

    #[test]
    fn an_over_budget_context_clamps_instead_of_overflowing() {
        assert_eq!(context_fill(33_000, Some(32_768)), Some(1.0));
    }

    #[test]
    fn credits_use_the_plan_limit_when_it_is_known() {
        let fill = credit_fill(Some(25.0), Some(75.0), Some(100.0));
        assert!(fill.is_some_and(|fill| close(fill, 0.25)), "{fill:?}");
    }

    #[test]
    fn credits_derive_spend_from_a_balance_when_a_limit_is_known() {
        let fill = credit_fill(None, Some(3.0), Some(12.0));
        assert!(fill.is_some_and(|fill| close(fill, 0.75)), "{fill:?}");

        // A balance with no ceiling says nothing about how full the bar is.
        assert_eq!(credit_fill(None, Some(2.0), None), None);
    }

    #[test]
    fn credits_derive_the_total_when_only_spend_and_balance_exist() {
        let fill = credit_fill(Some(30.0), Some(70.0), None);
        assert!(fill.is_some_and(|fill| close(fill, 0.3)), "{fill:?}");
    }

    #[test]
    fn credits_with_nothing_reported_are_indeterminate() {
        assert_eq!(credit_fill(None, None, None), None);
        assert_eq!(credit_fill(Some(5.0), None, None), None);
        assert_eq!(credit_fill(None, Some(5.0), None), None);
        assert_eq!(credit_fill(Some(5.0), Some(5.0), Some(0.0)), None);
    }

    #[test]
    fn credits_clamp_refunds_and_overdrafts() {
        assert_eq!(
            credit_fill(Some(-10.0), Some(110.0), Some(100.0)),
            Some(0.0)
        );
        assert_eq!(
            credit_fill(Some(120.0), Some(-20.0), Some(100.0)),
            Some(1.0)
        );
        assert_eq!(credit_fill(Some(0.0), Some(0.0), None), None);
    }

    #[test]
    fn nonsense_is_not_drawn() {
        assert_eq!(credit_fill(Some(f64::NAN), Some(1.0), Some(10.0)), None);
        assert_eq!(
            credit_fill(Some(1.0), Some(f64::INFINITY), Some(10.0)),
            None
        );
        assert_eq!(credit_fill(Some(1.0), Some(9.0), Some(f64::NAN)), None);
    }
}
