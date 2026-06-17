//! `localmem recall` handler.
//!
//! Entity-centric bitemporal lookup. See SPEC.md "localmem recall" and
//! TASKS.md T-38. Without `--at-time` we return the audit view (every
//! fact about the entity, including retired rows). With `--at-time` we
//! return only facts believed true at that instant, matching the
//! `memory_recall` MCP tool contract.

use crate::facts::{Fact, FactsStore};
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct RecallOutput {
    pub entity: String,
    pub facts: Vec<RecallFact>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecallFact {
    pub predicate: String,
    pub object: String,
    #[serde(default)]
    pub confidence: f64,
    pub valid_from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<String>,
    pub sources: Vec<String>,
}

impl RecallFact {
    fn from_fact(f: Fact) -> Self {
        Self {
            predicate: f.predicate,
            object: f.object,
            confidence: f.confidence,
            valid_from: f.valid_from.to_rfc3339_opts(SecondsFormat::Millis, true),
            valid_to: f
                .valid_to
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true)),
            retired_at: f
                .retired_at
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true)),
            sources: f.source_events.iter().map(|e| e.to_string()).collect(),
        }
    }
}

/// Entry point for the `recall` subcommand.
///
/// `tags` is the T-51b container-tag filter: facts must carry every
/// `(key, value)` pair in the map to surface. An empty map disables
/// filtering, preserving v0.1 behavior.
///
/// `recall` is the audit-grade entity pull. Per SPEC_V0_2 "container-
/// tag model", entity-only recall is the *one* read path that
/// surfaces `visibility=private` captures (T-51c). Retention TTL
/// still applies on this path: expired ephemeral memories stay
/// hidden everywhere, including audit.
pub fn run(
    home: Option<&str>,
    entity: &str,
    at_time: Option<DateTime<Utc>>,
    tags: BTreeMap<String, String>,
    as_json: bool,
) -> Result<()> {
    let home = resolve_home(home)?;

    // Route through the running server when up (the facts DuckDB lock is
    // exclusive); fall back to an in-process read otherwise.
    {
        let mut body = serde_json::json!({ "entity": entity, "tags": tags });
        if let Some(t) = at_time {
            body["at_time"] = serde_json::Value::String(t.to_rfc3339());
        }
        if let Some(v) = crate::cli::server_post(&home, "/recall", body) {
            let facts: Vec<RecallFact> = v
                .get("facts")
                .and_then(|f| serde_json::from_value(f.clone()).ok())
                .unwrap_or_default();
            return emit(
                &RecallOutput {
                    entity: entity.to_string(),
                    facts,
                },
                as_json,
            );
        }
    }

    let store = FactsStore::open(&home).context("open facts store")?;
    let tag_filter = if tags.is_empty() { None } else { Some(&tags) };
    let visibility = crate::reserved_tags::Visibility::IncludePrivate;
    let now = Utc::now();
    let facts = match at_time {
        // facts_at_time predates T-51b/T-51c; we keep its signature
        // and apply the same filter stack in-process. Per-subject
        // result sets are small, so the cost is negligible.
        Some(t) => {
            let rows = store.facts_at_time(entity, t)?;
            rows.into_iter()
                .filter(|fact| {
                    if let Some(f) = tag_filter {
                        if !crate::tag_match::matches(&fact.tags, f) {
                            return false;
                        }
                    }
                    crate::reserved_tags::is_visible(&fact.tags, fact.valid_from, now, visibility)
                })
                .collect()
        }
        None => store.facts_for_subject_filtered(entity, tag_filter, visibility, now)?,
    };
    let out = RecallOutput {
        entity: entity.to_string(),
        facts: facts.into_iter().map(RecallFact::from_fact).collect(),
    };
    emit(&out, as_json)
}

fn emit(out: &RecallOutput, as_json: bool) -> Result<()> {
    if as_json {
        let json = serde_json::json!({
            "ok": true,
            "entity": out.entity,
            "facts": out.facts,
        });
        println!("{json}");
        return Ok(());
    }
    if out.facts.is_empty() {
        println!("no facts about {:?}", out.entity);
        return Ok(());
    }
    for (i, f) in out.facts.iter().enumerate() {
        let retired = f
            .retired_at
            .as_deref()
            .map(|t| format!(" retired={t}"))
            .unwrap_or_default();
        let valid_to = f
            .valid_to
            .as_deref()
            .map(|t| format!(" valid_to={t}"))
            .unwrap_or_default();
        println!(
            "[{}] {} {} (conf={:.2}) valid_from={}{}{}",
            i + 1,
            f.predicate,
            f.object,
            f.confidence,
            f.valid_from,
            valid_to,
            retired,
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
    use crate::event_id::EventId;
    use crate::facts::Fact;
    use tempfile::tempdir;

    fn ts(epoch: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(epoch, 0).unwrap()
    }

    fn sample_fact(subject: &str, object: &str, valid_from: DateTime<Utc>) -> Fact {
        Fact {
            id: EventId::new(),
            subject: subject.into(),
            predicate: "lives_in".into(),
            object: object.into(),
            confidence: 0.8,
            valid_from,
            valid_to: None,
            recorded_at: valid_from,
            retired_at: None,
            source_events: vec![EventId::new()],
            tags: Default::default(),
            policy_id: None,
            kind: Default::default(),
        }
    }

    #[test]
    fn run_emits_no_facts_message_when_entity_unknown() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        // No facts inserted. run should succeed and print "no facts about".
        run(tmp.path().to_str(), "ghost", None, BTreeMap::new(), false).unwrap();
    }

    #[test]
    fn run_with_tags_filters_returned_facts() {
        // T-51b: --tags applied at the recall layer drops facts whose
        // source capture didn't carry the requested tag.
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let mut lm = sample_fact("user", "rust", ts(1_700_000_000));
        lm.tags.insert("project".into(), "localmem".into());
        let mut other = sample_fact("user", "go", ts(1_700_000_000));
        other.tags.insert("project".into(), "other".into());
        store.insert(&lm).unwrap();
        store.insert(&other).unwrap();
        drop(store);

        let store = FactsStore::open(tmp.path()).unwrap();
        let mut filter = BTreeMap::new();
        filter.insert("project".into(), "localmem".into());
        let rows = store
            .facts_for_subject_filtered(
                "user",
                Some(&filter),
                crate::reserved_tags::Visibility::IncludePrivate,
                Utc::now(),
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].object, "rust");
    }

    #[test]
    fn audit_view_returns_retired_rows() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let mut f = sample_fact("user", "Tokyo", ts(1_700_000_000));
        f.retired_at = Some(ts(1_700_000_500));
        store.insert(&f).unwrap();
        drop(store);

        // Audit recall (no at_time) must include the retired row.
        let store = FactsStore::open(tmp.path()).unwrap();
        let rows = store.facts_for_subject("user").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].retired_at.is_some());
    }

    #[test]
    fn at_time_view_hides_retired_rows() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let mut f = sample_fact("user", "Tokyo", ts(1_700_000_000));
        f.retired_at = Some(ts(1_700_000_500));
        store.insert(&f).unwrap();
        drop(store);

        let store = FactsStore::open(tmp.path()).unwrap();
        // After retirement → empty.
        let rows = store.facts_at_time("user", ts(1_700_001_000)).unwrap();
        assert!(rows.is_empty());
        // Before retirement → 1 row, not retired-yet at that time.
        let rows = store.facts_at_time("user", ts(1_700_000_100)).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn recall_fact_round_trips_through_serialization() {
        let f = sample_fact("user", "rust", ts(1_700_000_000));
        let rf = RecallFact::from_fact(f.clone());
        assert_eq!(rf.predicate, "lives_in");
        assert_eq!(rf.object, "rust");
        assert!(rf.valid_to.is_none());
        let json = serde_json::to_value(&rf).unwrap();
        assert!(json["valid_to"].is_null() || json.get("valid_to").is_none());
    }
}
