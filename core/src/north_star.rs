//! North Star cumulative usage telemetry (SPEC-intelligence-v2 §2.9).
//!
//! The headline business metric: "this week / month / year you (or your team)
//! retrieved precise context costing N tokens = $M, a fraction of dumping your
//! whole history." Every `/search` records ONE line here: a timestamp, the
//! token count it served, the result count, the accounting model, and the
//! dollar cost. That is ALL it records: NO query text, NO content, NO snippets.
//! It is content-free usage telemetry, local-only, so it never violates the
//! no-plaintext-leaves-the-machine promise (MOAT 5).
//!
//! It lives at `<home>/north_star.jsonl`, NOT under `derived/`, because it is a
//! record of READS, not of memory, so it is not recomputable from the event log
//! (invariant 2 governs derived stores, not usage logs). Deleting it loses only
//! historical savings stats; memory is untouched.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// Usage-telemetry filename at the home root.
pub const LOG_FILE: &str = "north_star.jsonl";

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    ts: DateTime<Utc>,
    tokens: usize,
    results: usize,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
}

/// Append one retrieval to the usage log. Best-effort and content-free: any IO
/// or serialization failure is swallowed, because telemetry must never fail a
/// search (MOAT 4: no read path depends on this succeeding).
pub fn record_retrieval(
    home: &Path,
    tokens: usize,
    results: usize,
    model: &str,
    cost_usd: Option<f64>,
) {
    let rec = Record {
        ts: Utc::now(),
        tokens,
        results,
        model: model.to_string(),
        cost_usd,
    };
    let Ok(line) = serde_json::to_string(&rec) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join(LOG_FILE))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Aggregated savings over one time window.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Window {
    pub retrievals: u64,
    pub tokens_served: u64,
    pub cost_usd: f64,
    /// Estimated tokens saved vs dumping relevant raw history, using the config
    /// `baseline_multiplier`: `tokens_served * (multiplier - 1)`. Labeled an
    /// estimate until the A/B harness (P6) supplies a measured ratio.
    pub est_tokens_saved: u64,
    pub est_cost_saved_usd: f64,
}

impl Window {
    fn add(&mut self, tokens: usize, cost: Option<f64>) {
        self.retrievals += 1;
        self.tokens_served += tokens as u64;
        self.cost_usd += cost.unwrap_or(0.0);
    }
    fn finish(&mut self, multiplier: f64) {
        let factor = (multiplier - 1.0).max(0.0);
        self.est_tokens_saved = (self.tokens_served as f64 * factor) as u64;
        self.est_cost_saved_usd = self.cost_usd * factor;
    }
}

/// The full North Star rollup the dashboard panel + an agent can read.
#[derive(Debug, Clone, Serialize)]
pub struct Rollup {
    pub today: Window,
    pub last_7d: Window,
    pub last_30d: Window,
    pub all_time: Window,
    pub baseline_multiplier: f64,
    /// Earliest recorded retrieval (RFC3339), so "since" can be shown. `None`
    /// when there is no usage yet.
    pub since: Option<String>,
}

/// Compute the rollup from the usage log. A missing/empty log yields all-zero
/// windows (a fresh install has saved nothing yet, which is honest, not an
/// error). `now` is injected so the windows are testable.
pub fn rollup_at(home: &Path, baseline_multiplier: f64, now: DateTime<Utc>) -> Rollup {
    let mut r = Rollup {
        today: Window::default(),
        last_7d: Window::default(),
        last_30d: Window::default(),
        all_time: Window::default(),
        baseline_multiplier,
        since: None,
    };
    let day_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|d| DateTime::<Utc>::from_naive_utc_and_offset(d, Utc))
        .unwrap_or(now);
    let w7 = now - Duration::days(7);
    let w30 = now - Duration::days(30);
    let mut earliest: Option<DateTime<Utc>> = None;

    if let Ok(text) = std::fs::read_to_string(home.join(LOG_FILE)) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<Record>(line) else {
                continue;
            };
            earliest = Some(earliest.map_or(rec.ts, |e| e.min(rec.ts)));
            r.all_time.add(rec.tokens, rec.cost_usd);
            if rec.ts >= w30 {
                r.last_30d.add(rec.tokens, rec.cost_usd);
            }
            if rec.ts >= w7 {
                r.last_7d.add(rec.tokens, rec.cost_usd);
            }
            if rec.ts >= day_start {
                r.today.add(rec.tokens, rec.cost_usd);
            }
        }
    }
    for w in [
        &mut r.today,
        &mut r.last_7d,
        &mut r.last_30d,
        &mut r.all_time,
    ] {
        w.finish(baseline_multiplier);
    }
    r.since = earliest.map(|t| t.to_rfc3339());
    r
}

/// Convenience: rollup as of now.
pub fn rollup(home: &Path, baseline_multiplier: f64) -> Rollup {
    rollup_at(home, baseline_multiplier, Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn empty_log_is_all_zero() {
        let tmp = tempdir().unwrap();
        let r = rollup(tmp.path(), 10.0);
        assert_eq!(r.all_time, Window::default());
        assert!(r.since.is_none());
    }

    #[test]
    fn records_accumulate_and_savings_estimate_applies() {
        let tmp = tempdir().unwrap();
        record_retrieval(tmp.path(), 100, 3, "gpt-4o", Some(0.00025));
        record_retrieval(tmp.path(), 200, 5, "gpt-4o", Some(0.00050));
        let r = rollup(tmp.path(), 10.0);
        assert_eq!(r.all_time.retrievals, 2);
        assert_eq!(r.all_time.tokens_served, 300);
        // multiplier 10 => saved 9x the served tokens.
        assert_eq!(r.all_time.est_tokens_saved, 2700);
        assert!((r.all_time.cost_usd - 0.00075).abs() < 1e-9);
        assert!(r.since.is_some());
    }

    #[test]
    fn windows_bucket_by_age() {
        let tmp = tempdir().unwrap();
        // Hand-write records at known ages (record_retrieval stamps "now", so we
        // write the file directly to control timestamps).
        let now = Utc::now();
        let mk = |ts: DateTime<Utc>, tokens: usize| {
            format!(
                r#"{{"ts":"{}","tokens":{},"results":1,"model":"gpt-4o"}}"#,
                ts.to_rfc3339(),
                tokens
            )
        };
        let lines = [
            mk(now - Duration::hours(1), 10),  // today, 7d, 30d, all
            mk(now - Duration::days(3), 20),   // 7d, 30d, all
            mk(now - Duration::days(15), 40),  // 30d, all
            mk(now - Duration::days(100), 80), // all only
        ]
        .join("\n");
        std::fs::write(tmp.path().join(LOG_FILE), lines).unwrap();
        let r = rollup_at(tmp.path(), 1.0, now);
        assert_eq!(r.today.tokens_served, 10);
        assert_eq!(r.last_7d.tokens_served, 30);
        assert_eq!(r.last_30d.tokens_served, 70);
        assert_eq!(r.all_time.tokens_served, 150);
        // multiplier 1.0 => no savings claimed.
        assert_eq!(r.all_time.est_tokens_saved, 0);
    }
}
