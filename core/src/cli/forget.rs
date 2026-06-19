//! `localmem forget` handler.
//!
//! Soft-deletes a memory by appending a `forget` event AND retiring any
//! matching facts in DuckDB. See SPEC.md "localmem forget" and TASKS.md
//! T-40.
//!
//! `--target ID` accepts either a capture id (retires every fact derived
//! from that capture) or a fact id (retires that one fact). Both flows
//! land the same single `forget` event in `events.jsonl` and the same
//! single `retire_facts_for_target` call against the facts table, so
//! replay reproduces the operation deterministically.
//!
//! `--criteria JSON` is v0.1-limited to `{"subject": "...", "predicate": "..."}`
//! matches; broader queries (regex, full-text, time-range) come later.

use crate::event::{Event, EventKind, ForgetPayload, Source};
use crate::event_id::EventId;
use crate::event_log::EventLog;
use crate::facts::FactsStore;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Map;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForgetOutput {
    /// Ids of forget events appended to the log. v0.1 always emits one;
    /// the array shape leaves room for criteria-batched forgets later.
    pub forgotten_event_ids: Vec<String>,
    pub facts_retired: u64,
}

/// Parsed `--criteria` payload. The JSON shape is open by SPEC but v0.1
/// pins it to subject/predicate match; unknown keys are tolerated for
/// forward compat.
#[derive(Debug, Clone, Deserialize)]
struct Criteria {
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    predicate: Option<String>,
}

/// Entry point for the `forget` subcommand.
pub fn run(
    home: Option<&str>,
    target: Option<&str>,
    criteria: Option<&str>,
    reason: Option<&str>,
    as_json: bool,
) -> Result<()> {
    let home = resolve_home(home)?;
    match (target, criteria) {
        (Some(_), Some(_)) => bail!("forget requires exactly one of --target or --criteria"),
        (None, None) => bail!("forget requires either --target ID or --criteria JSON"),
        (Some(t), None) => {
            let out = forget_by_target(&home, t, reason.unwrap_or("user-forget"))?;
            emit(&out, as_json)
        }
        (None, Some(c)) => {
            let parsed: Criteria =
                serde_json::from_str(c).context("parse --criteria as JSON object")?;
            let out =
                forget_by_criteria(&home, &parsed, reason.unwrap_or("user-forget (criteria)"))?;
            emit(&out, as_json)
        }
    }
}

/// Forget a single capture or fact by id.
pub fn forget_by_target(
    home: &std::path::Path,
    target_str: &str,
    reason: &str,
) -> Result<ForgetOutput> {
    let target_id: EventId = target_str
        .parse()
        .with_context(|| format!("parse --target as event id: {target_str:?}"))?;

    let event_log = EventLog::open(home).context("open event log")?;
    let forget_event = Event::new(
        EventKind::Forget(ForgetPayload {
            target_id,
            reason: reason.to_string(),
            scope: None,
            extra: Map::new(),
        }),
        cli_source(),
    );
    event_log
        .append(&forget_event)
        .context("append forget event")?;

    let facts = crate::cli::open_facts(home)?;
    let n = facts
        .retire_facts_for_target(&target_id.to_string(), forget_event.ts)
        .context("retire facts on forget")?;

    Ok(ForgetOutput {
        forgotten_event_ids: vec![forget_event.id.to_string()],
        facts_retired: n,
    })
}

/// Forget every fact matching the supplied criteria. v0.1 supports
/// subject + predicate equality. We emit one `forget` event per matched
/// fact so the log reflects the exact retirement applied.
fn forget_by_criteria(
    home: &std::path::Path,
    criteria: &Criteria,
    reason: &str,
) -> Result<ForgetOutput> {
    if criteria.subject.is_none() && criteria.predicate.is_none() {
        bail!("forget --criteria requires at least `subject` or `predicate`");
    }

    let facts = crate::cli::open_facts(home)?;
    // We rely on the audit-view query so retired rows are skipped:
    // re-running forget on already-retired facts produces zero work
    // (idempotency under repeat).
    let candidates = match criteria.subject.as_deref() {
        Some(subject) => facts.facts_for_subject(subject)?,
        None => facts.all_live_facts(Utc::now(), None)?,
    };
    let matched: Vec<_> = candidates
        .into_iter()
        .filter(|f| f.retired_at.is_none())
        .filter(|f| match &criteria.predicate {
            Some(p) => &f.predicate == p,
            None => true,
        })
        .collect();
    if matched.is_empty() {
        return Ok(ForgetOutput {
            forgotten_event_ids: Vec::new(),
            facts_retired: 0,
        });
    }

    let event_log = EventLog::open(home).context("open event log")?;
    let mut event_ids = Vec::with_capacity(matched.len());
    let mut total = 0u64;
    for fact in &matched {
        let forget_event = Event::new(
            EventKind::Forget(ForgetPayload {
                target_id: fact.id,
                reason: reason.to_string(),
                scope: Some("criteria".into()),
                extra: Map::new(),
            }),
            cli_source(),
        );
        event_log
            .append(&forget_event)
            .context("append forget event")?;
        total += facts
            .retire_facts_for_target(&fact.id.to_string(), forget_event.ts)
            .context("retire fact on criteria forget")?;
        event_ids.push(forget_event.id.to_string());
    }
    Ok(ForgetOutput {
        forgotten_event_ids: event_ids,
        facts_retired: total,
    })
}

fn cli_source() -> Source {
    Source {
        app: "cli".into(),
        host: std::env::var("HOSTNAME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "localhost".into()),
        user: std::env::var("USER").ok().filter(|s| !s.is_empty()),
    }
}

fn emit(out: &ForgetOutput, as_json: bool) -> Result<()> {
    if as_json {
        let json = serde_json::json!({
            "ok": true,
            "forgotten_event_ids": out.forgotten_event_ids,
            "facts_retired": out.facts_retired,
        });
        println!("{json}");
    } else {
        println!(
            "forgot {} event(s); {} fact(s) retired",
            out.forgotten_event_ids.len(),
            out.facts_retired,
        );
    }
    Ok(())
}

fn resolve_home(override_: Option<&str>) -> Result<PathBuf> {
    if let Some(h) = override_.filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var("HOME")
        .context("HOME environment variable is not set; pass --home explicitly")?;
    Ok(PathBuf::from(home).join(".localmem"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::init::init_home;
    use crate::facts::Fact;
    use tempfile::tempdir;

    fn ts(epoch: i64) -> chrono::DateTime<Utc> {
        chrono::DateTime::<Utc>::from_timestamp(epoch, 0).unwrap()
    }

    fn sample_fact(subject: &str, predicate: &str, object: &str) -> Fact {
        Fact {
            id: EventId::new(),
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.into(),
            confidence: 0.8,
            valid_from: ts(1_700_000_000),
            valid_to: None,
            recorded_at: ts(1_700_000_000),
            retired_at: None,
            tags: Default::default(),
            source_events: vec![EventId::new()],
            policy_id: None,
            kind: Default::default(),
        }
    }

    #[test]
    fn forget_by_fact_id_retires_one_row() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let facts = FactsStore::open(tmp.path()).unwrap();
        let f = sample_fact("user", "prefers", "rust");
        let fact_id = f.id;
        facts.insert(&f).unwrap();
        drop(facts);

        let out = forget_by_target(tmp.path(), &fact_id.to_string(), "test").unwrap();
        assert_eq!(out.facts_retired, 1);
        assert_eq!(out.forgotten_event_ids.len(), 1);

        let facts = FactsStore::open(tmp.path()).unwrap();
        let rows = facts.facts_for_subject("user").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].retired_at.is_some());
    }

    #[test]
    fn forget_by_capture_id_retires_all_derived_facts() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let facts = FactsStore::open(tmp.path()).unwrap();
        let capture_id = EventId::new();
        let mut a = sample_fact("user", "prefers", "rust");
        a.source_events = vec![capture_id];
        let mut b = sample_fact("user", "prefers", "lifetimes");
        b.source_events = vec![capture_id];
        facts.insert(&a).unwrap();
        facts.insert(&b).unwrap();
        drop(facts);

        let out = forget_by_target(tmp.path(), &capture_id.to_string(), "spring cleaning").unwrap();
        assert_eq!(out.facts_retired, 2);
        let facts = FactsStore::open(tmp.path()).unwrap();
        let rows = facts.facts_for_subject("user").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.retired_at.is_some()));
    }

    #[test]
    fn forget_appends_event_to_log() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let facts = FactsStore::open(tmp.path()).unwrap();
        let f = sample_fact("user", "prefers", "rust");
        let fact_id = f.id;
        facts.insert(&f).unwrap();
        drop(facts);

        forget_by_target(tmp.path(), &fact_id.to_string(), "user request").unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        let events: Vec<_> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, EventKind::Forget(_)));
    }

    #[test]
    fn forget_unknown_target_returns_zero_without_panic() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let id = EventId::new();
        // Nothing inserted; forget should still emit the event and report
        // zero facts retired (idempotent against no-op).
        let out = forget_by_target(tmp.path(), &id.to_string(), "test").unwrap();
        assert_eq!(out.facts_retired, 0);
        assert_eq!(out.forgotten_event_ids.len(), 1);
    }

    #[test]
    fn forget_by_criteria_matches_subject_and_predicate() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let facts = FactsStore::open(tmp.path()).unwrap();
        facts
            .insert(&sample_fact("user", "prefers", "rust"))
            .unwrap();
        facts
            .insert(&sample_fact("user", "lives_in", "berlin"))
            .unwrap();
        facts
            .insert(&sample_fact("alice", "prefers", "haskell"))
            .unwrap();
        drop(facts);

        let criteria = Criteria {
            subject: Some("user".into()),
            predicate: Some("prefers".into()),
        };
        let out = forget_by_criteria(tmp.path(), &criteria, "scoped").unwrap();
        assert_eq!(out.facts_retired, 1);

        let facts = FactsStore::open(tmp.path()).unwrap();
        let rows = facts.facts_for_subject("user").unwrap();
        let prefers = rows.iter().find(|r| r.predicate == "prefers").unwrap();
        let lives = rows.iter().find(|r| r.predicate == "lives_in").unwrap();
        assert!(prefers.retired_at.is_some());
        assert!(lives.retired_at.is_none(), "lives_in must remain live");
    }

    #[test]
    fn empty_criteria_errors_clearly() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let criteria = Criteria {
            subject: None,
            predicate: None,
        };
        let err = forget_by_criteria(tmp.path(), &criteria, "nope").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("subject") || msg.contains("predicate"));
    }
}
