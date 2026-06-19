//! P3 (intelligence-v2 §2.4): freshness as decay over a belief's age.
//!
//! A belief in the facts store is "current" when it is the latest unrefuted
//! statement, but current is NOT the same as verified-true: a high-importance
//! belief can sit as current for a year with no re-confirmation and quietly go
//! stale. This module turns a belief's age into a `0.0..=1.0` freshness signal
//! and a staleness flag, so surfaces can show "still true?" instead of
//! presenting every current belief as equally trustworthy.
//!
//! It reuses the per-kind half-lives from T-73 (`[retriever].decay_half_life`,
//! e.g. decision=365d, preference=180d, fact=90d) that today only weight
//! retrieval ranking. Freshness = `0.5 ^ (age_days / half_life_days)`: 1.0 the
//! day it was written, 0.5 after one half-life, 0.25 after two, and so on.

use chrono::{DateTime, Utc};
use std::collections::HashMap;

/// Below this freshness a current belief is considered stale enough to review.
/// 0.5 == exactly one half-life has elapsed since it was last written.
pub const STALE_THRESHOLD: f64 = 0.5;

/// Fallback half-life (days) for a kind with no configured entry. Matches the
/// `fact` default so an unknown kind ages on the same neutral curve.
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 90.0;

/// Freshness in `0.0..=1.0` for a belief of `kind` last written at `valid_from`,
/// evaluated at `now`. `half_lives` is `Config::decay_half_lives_in_days()`.
/// Future-dated beliefs (clock skew / imported) clamp to 1.0.
pub fn freshness(
    kind: &str,
    valid_from: DateTime<Utc>,
    now: DateTime<Utc>,
    half_lives: &HashMap<String, f64>,
) -> f64 {
    let hl = half_lives
        .get(kind)
        .copied()
        .filter(|d| *d > 0.0)
        .unwrap_or(DEFAULT_HALF_LIFE_DAYS);
    let age_days = (now - valid_from).num_seconds().max(0) as f64 / 86_400.0;
    0.5_f64.powf(age_days / hl).clamp(0.0, 1.0)
}

/// Whether a freshness value is below the review threshold.
pub fn is_stale(freshness: f64) -> bool {
    freshness < STALE_THRESHOLD
}

/// Whether a kind carries enough weight that a stale current belief is worth
/// surfacing for re-confirmation. Chatter (`note`) and short-lived items
/// (`todo`) are excluded: only durable claims merit a "still true?" prompt.
pub fn is_high_importance(kind: &crate::kind::Kind) -> bool {
    matches!(
        kind,
        crate::kind::Kind::Decision
            | crate::kind::Kind::Preference
            | crate::kind::Kind::Constraint
            | crate::kind::Kind::Fact
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hl() -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("decision".to_string(), 365.0);
        m.insert("fact".to_string(), 90.0);
        m
    }

    fn days_ago(n: i64, now: DateTime<Utc>) -> DateTime<Utc> {
        now - chrono::Duration::days(n)
    }

    #[test]
    fn fresh_when_just_written() {
        let now = Utc::now();
        assert!((freshness("fact", now, now, &hl()) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn half_at_one_half_life() {
        let now = Utc::now();
        let f = freshness("fact", days_ago(90, now), now, &hl());
        assert!(
            (f - 0.5).abs() < 0.01,
            "≈0.5 after one 90d half-life, got {f}"
        );
    }

    #[test]
    fn decision_ages_slower_than_fact() {
        let now = Utc::now();
        let old = days_ago(120, now);
        assert!(
            freshness("decision", old, now, &hl()) > freshness("fact", old, now, &hl()),
            "a 365d-half-life decision stays fresher than a 90d-half-life fact at the same age"
        );
    }

    #[test]
    fn unknown_kind_uses_default() {
        let now = Utc::now();
        let f = freshness("mystery", days_ago(90, now), now, &hl());
        assert!((f - 0.5).abs() < 0.01, "default 90d half-life applies");
    }

    #[test]
    fn future_dated_clamps_to_one() {
        let now = Utc::now();
        assert!(
            (freshness("fact", now + chrono::Duration::days(5), now, &hl()) - 1.0).abs() < 1e-9
        );
    }

    #[test]
    fn staleness_flips_at_threshold() {
        assert!(!is_stale(0.6));
        assert!(is_stale(0.4));
    }
}
