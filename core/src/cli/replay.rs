//! `localmem replay` handler.
//!
//! Walks `events.jsonl` and rebuilds every derived store from scratch.
//! See ARCHITECTURE.md invariant 3 ("`localmem replay` is deterministic")
//! and TASKS.md task T-25.
//!
//! Rebuild flow:
//! 1. Rename `<home>/derived/` to `<home>/derived.old/` (atomic on local
//!    filesystems) so a crash mid-replay leaves the old derived state
//!    intact and the next replay can swap it back.
//! 2. Open fresh stores under the empty `derived/` directory.
//! 3. Walk the event log, dispatching by `EventKind`. Captures flow
//!    through policy + journal + lex/vec indexing; facts re-insert into
//!    DuckDB; forgets retire their targets; updates retire-then-insert.
//! 4. Remove `derived.old/` once the new build is complete.
//!
//! Replay never writes to `events.jsonl`. It is read-only against the
//! source of truth. Fact extraction does NOT run during replay because
//! the historical fact events already live in `events.jsonl` and will be
//! re-inserted by the Fact branch below; re-running the extractor would
//! corrupt the log (process_capture_facts appends).

use crate::embed::{Embedder, EMBEDDING_DIM};
use crate::event::{Event, EventKind, PolicyAction};
use crate::event_log::EventLog;
use crate::facts::{Fact, FactsStore};
use crate::journal::{Journal, JournalEntry};
use crate::lexical::{LexicalIndex, LexicalResultExt};
use crate::policy::{EvalContext, Policy};
use crate::vectors::VectorStore;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Sliding-window size for the policy's recent-event context, matching
/// TASKS.md T-16 ("last 100 events").
const POLICY_RECENT_WINDOW: usize = 100;

/// Counters reported at the end of a replay run. Pure observability;
/// callers should not branch on these.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplayStats {
    pub events_seen: u64,
    pub captures_seen: u64,
    pub committed: u64,
    pub facts_inserted: u64,
    pub forgets: u64,
    pub updates: u64,
    /// T-52b: count of `UpdateCapture` events re-applied. Each one
    /// re-indexes the target capture's mutable metadata (`done` state
    /// for todos today; pinned/archived flags later) without touching
    /// content or derived facts.
    pub capture_updates: u64,
    pub policy_events_skipped: u64,
    pub import_events_skipped: u64,
}

/// Entry point for the `replay` subcommand.
pub async fn run(home: Option<&str>, as_json: bool) -> Result<()> {
    let home = resolve_home(home)?;
    let stats = replay_home(&home).await?;
    emit_stats(&stats, as_json)
}

/// Rebuild every derived store under `home`. Pulled out of [`run`] so
/// integration tests can exercise the full pipeline without spawning the
/// CLI binary.
pub async fn replay_home(home: &Path) -> Result<ReplayStats> {
    let event_log = EventLog::open(home).context("open event log for read")?;

    let derived = home.join("derived");
    let stash = home.join("derived.old");
    swap_derived(&derived, &stash).context("stash existing derived dir")?;

    // Open every store on the fresh derived/. The embedder is optional:
    // a home without the BGE assets can still rebuild facts + lexical,
    // which is enough to satisfy the trust promise for those stores. The
    // vector store is gated on embedder availability so a missing model
    // does not leave behind an empty vectors.lance with the wrong schema.
    let model_dir = resolve_model_dir(home);
    let mut embedder = match Embedder::load(&model_dir) {
        Ok(e) => Some(e),
        Err(err) => {
            warn!(
                model_dir = %model_dir.display(),
                error = %err,
                "embedder unavailable; replay will skip vector store"
            );
            None
        }
    };
    let vector_store = if embedder.is_some() {
        Some(
            VectorStore::open(home, EMBEDDING_DIM)
                .await
                .context("open vector store")?,
        )
    } else {
        None
    };
    let mut lexical = LexicalIndex::open(home).lex_context("open lexical index")?;
    let facts = FactsStore::open(home).context("open facts store")?;
    let journal = Journal::open(home).context("open journal")?;
    let policy = Policy::load(home).context("load policy")?;

    let mut stats = ReplayStats::default();
    let mut recent: Vec<Event> = Vec::with_capacity(POLICY_RECENT_WINDOW);

    for ev_result in event_log.iter().context("open event log iterator")? {
        let event = ev_result.context("read event from event log")?;
        stats.events_seen += 1;
        match &event.kind {
            EventKind::Capture(payload) => {
                stats.captures_seen += 1;
                let decision = policy
                    .evaluate(&event, &EvalContext { recent: &recent })
                    .context("evaluate policy during replay")?;
                let entry = JournalEntry::from_decision(&decision, event.id, event.ts);
                journal
                    .append(&entry)
                    .context("append journal entry during replay")?;
                if decision.action == PolicyAction::Commit {
                    if let (Some(emb), Some(vs)) = (embedder.as_mut(), vector_store.as_ref()) {
                        let vec = emb
                            .embed(&payload.text)
                            .context("embed capture during replay")?;
                        vs.add(&event.id.to_string(), &vec, &payload.text, event.ts)
                            .await
                            .context("write embedding during replay")?;
                    }
                    lexical
                        .index_event(&event)
                        .context("index capture in lexical during replay")?;
                    stats.committed += 1;
                }
                push_recent(&mut recent, event);
            }
            EventKind::Fact(payload) => {
                // The fact event already lives in events.jsonl from the
                // original write; replay just re-materializes its DuckDB
                // row. We do NOT re-run the extractor here (that would
                // append new fact events and corrupt the log).
                let fact = Fact::from_event(event.id, payload, event.ts, None);
                facts.insert(&fact).context("insert fact during replay")?;
                stats.facts_inserted += 1;
            }
            EventKind::Forget(payload) => {
                facts
                    .retire_facts_for_target(&payload.target_id.to_string(), event.ts)
                    .context("retire facts on forget during replay")?;
                stats.forgets += 1;
            }
            EventKind::Update(payload) => {
                // Retire the superseded fact, then materialize the new one
                // under the Update event's own id. The new fact's lineage
                // points at the same source captures the payload carries.
                facts
                    .retire_facts_for_target(&payload.supersedes_id.to_string(), event.ts)
                    .context("retire superseded fact during replay")?;
                let fact = Fact::from_event(event.id, &payload.new_fact, event.ts, None);
                facts
                    .insert(&fact)
                    .context("insert new fact for update during replay")?;
                stats.updates += 1;
            }
            EventKind::UpdateCapture(payload) => {
                // T-52b: re-apply the latest mutable-metadata state to
                // the lex index. Currently this is just the todo
                // `done` flag; future fields on `UpdateCapturePayload`
                // (pinned, archived, ...) land here additively.
                if payload.done.is_some() {
                    lexical
                        .apply_capture_update(payload)
                        .context("re-apply capture update during replay")?;
                }
                stats.capture_updates += 1;
            }
            EventKind::Policy(_) => {
                // The journal is rebuilt above by re-evaluating policy on
                // each capture. Skipping the explicit policy event avoids
                // double-writing entries to journal.log.
                stats.policy_events_skipped += 1;
            }
            EventKind::Import(_) => {
                // Marker event; the underlying capture/fact events that
                // landed in events.jsonl carry the actual data.
                stats.import_events_skipped += 1;
            }
        }
    }
    lexical
        .commit()
        .context("commit lexical writer after replay")?;

    // Drop the stash now that the new derived state is durable.
    if stash.exists() {
        std::fs::remove_dir_all(&stash)
            .with_context(|| format!("remove stash at {}", stash.display()))?;
    }

    info!(?stats, "replay complete");
    Ok(stats)
}

/// Rename `derived` to `derived.old`, creating an empty `derived` after.
/// A prior, stale `derived.old` is removed first so this is safe to
/// re-run after an aborted replay.
fn swap_derived(derived: &Path, stash: &Path) -> Result<()> {
    if stash.exists() {
        std::fs::remove_dir_all(stash)
            .with_context(|| format!("remove stale stash at {}", stash.display()))?;
    }
    if derived.exists() {
        std::fs::rename(derived, stash)
            .with_context(|| format!("rename {} to {}", derived.display(), stash.display()))?;
    }
    std::fs::create_dir_all(derived)
        .with_context(|| format!("create empty derived dir at {}", derived.display()))?;
    Ok(())
}

fn push_recent(recent: &mut Vec<Event>, event: Event) {
    if recent.len() == POLICY_RECENT_WINDOW {
        recent.remove(0);
    }
    recent.push(event);
}

fn emit_stats(stats: &ReplayStats, as_json: bool) -> Result<()> {
    if as_json {
        let json = serde_json::json!({
            "ok": true,
            "events": stats.events_seen,
            "captures": stats.captures_seen,
            "committed": stats.committed,
            "facts": stats.facts_inserted,
            "forgets": stats.forgets,
            "updates": stats.updates,
            "policy_skipped": stats.policy_events_skipped,
            "imports_skipped": stats.import_events_skipped,
        });
        println!("{json}");
    } else {
        println!(
            "replay: events={} captures={} committed={} facts={} forgets={} updates={}",
            stats.events_seen,
            stats.captures_seen,
            stats.committed,
            stats.facts_inserted,
            stats.forgets,
            stats.updates,
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

/// Mirror of [`crate::cli::search::resolve_model_dir`]. Kept private here
/// to avoid pulling the search module into the replay module's surface;
/// the override semantics are identical so users see consistent behavior.
fn resolve_model_dir(home: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var("LOCALMEM_MODEL_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home.join("models").join("bge-small-en-v1.5")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        CapturePayload, FactPayload, ForgetPayload, ImportPayload, PolicyPayload, Source,
        UpdatePayload,
    };
    use crate::event_id::EventId;
    use chrono::Utc;
    use serde_json::Map;
    use tempfile::tempdir;

    fn capture(text: &str) -> Event {
        Event::new(
            EventKind::Capture(CapturePayload {
                text: text.into(),
                rewritten_text: None,
                kind: Default::default(),
                mime: None,
                attachments: vec![],
                tags: Default::default(),
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        )
    }

    fn fact_event(subject: &str, object: &str, derived_from: EventId) -> Event {
        Event::new(
            EventKind::Fact(FactPayload {
                subject: subject.into(),
                predicate: "prefers".into(),
                object: object.into(),
                confidence: 0.7,
                valid_from: Utc::now(),
                valid_to: None,
                derived_from: vec![derived_from],
                kind: Default::default(),
                tags: Default::default(),
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "h".into(),
                user: None,
            },
        )
    }

    fn forget_event(target_id: EventId) -> Event {
        Event::new(
            EventKind::Forget(ForgetPayload {
                target_id,
                reason: "unit test".into(),
                scope: None,
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "h".into(),
                user: None,
            },
        )
    }

    /// Disable embedder lookup for fast offline tests by pointing the env
    /// override at a non-existent path. Replay then runs facts + lexical
    /// without touching ONNX runtime.
    fn force_no_embedder() {
        std::env::set_var("LOCALMEM_MODEL_DIR", "/this/path/does/not/exist");
    }

    fn restore_embedder_env() {
        std::env::remove_var("LOCALMEM_MODEL_DIR");
    }

    #[tokio::test]
    async fn empty_log_replays_to_empty_stores() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        // EventLog::open creates events.jsonl on first call; the iterator
        // simply yields nothing, and replay should not error.
        let _log = EventLog::open(tmp.path()).unwrap();
        let stats = replay_home(tmp.path()).await.unwrap();
        restore_embedder_env();
        assert_eq!(stats, ReplayStats::default());
        // Derived dir exists with an empty facts.duckdb + lexical index.
        let derived = tmp.path().join("derived");
        assert!(derived.is_dir());
        let facts = FactsStore::open(tmp.path()).unwrap();
        assert_eq!(facts.count().unwrap(), 0);
        let lex = LexicalIndex::open(tmp.path()).unwrap();
        assert_eq!(lex.doc_count(), 0);
    }

    #[tokio::test]
    async fn replay_recreates_facts_for_simple_log() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        // Pre-write a capture + its derived fact event into the log.
        let log = EventLog::open(tmp.path()).unwrap();
        let cap = capture("I prefer functional Rust, even on weekends.");
        log.append(&cap).unwrap();
        let fact = fact_event("user", "rust", cap.id);
        log.append(&fact).unwrap();
        drop(log);

        let stats = replay_home(tmp.path()).await.unwrap();
        restore_embedder_env();
        assert_eq!(stats.events_seen, 2);
        assert_eq!(stats.captures_seen, 1);
        assert_eq!(stats.committed, 1, "policy should commit the long capture");
        assert_eq!(stats.facts_inserted, 1);

        // Facts row materialized with the right lineage.
        let facts = FactsStore::open(tmp.path()).unwrap();
        let rows = facts.facts_for_subject("user").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].object, "rust");
        assert_eq!(rows[0].source_events, vec![cap.id]);

        // Lexical index has the capture text reachable.
        let lex = LexicalIndex::open(tmp.path()).unwrap();
        let hits = lex.search("functional rust", 5, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event_id, cap.id.to_string());
    }

    #[tokio::test]
    async fn replay_with_forget_marks_fact_retired() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        let log = EventLog::open(tmp.path()).unwrap();
        let cap = capture("My SSN should never have been said: I prefer privacy.");
        log.append(&cap).unwrap();
        let fact = fact_event("user", "privacy", cap.id);
        log.append(&fact).unwrap();
        let forget = forget_event(cap.id);
        log.append(&forget).unwrap();
        drop(log);

        replay_home(tmp.path()).await.unwrap();
        restore_embedder_env();

        let facts = FactsStore::open(tmp.path()).unwrap();
        let rows = facts.facts_for_subject("user").unwrap();
        assert_eq!(rows.len(), 1, "row stays for audit");
        assert!(
            rows[0].retired_at.is_some(),
            "forget event must set retired_at on derived fact"
        );
    }

    #[tokio::test]
    async fn replay_twice_is_idempotent() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        let log = EventLog::open(tmp.path()).unwrap();
        let cap = capture("I prefer functional Rust over OO ceremony.");
        log.append(&cap).unwrap();
        let fact = fact_event("user", "rust", cap.id);
        log.append(&fact).unwrap();
        drop(log);

        let first = replay_home(tmp.path()).await.unwrap();
        let second = replay_home(tmp.path()).await.unwrap();
        restore_embedder_env();
        assert_eq!(first, second, "replay must be idempotent on stats");

        // And the underlying derived state must converge: same facts count,
        // same lexical doc count, same journal length.
        let facts = FactsStore::open(tmp.path()).unwrap();
        assert_eq!(facts.count().unwrap(), 1);
        let lex = LexicalIndex::open(tmp.path()).unwrap();
        assert_eq!(lex.doc_count(), 1);
        let journal = Journal::open(tmp.path()).unwrap();
        let entries: Vec<_> = journal.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        // Two replays each writing one journal entry per capture: the second
        // replay's swap drops the old journal.log along with the rest of
        // derived/, so the count is exactly one capture's worth.
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn replay_recovers_when_stale_derived_old_exists() {
        // Simulate an aborted prior replay that left derived.old behind.
        // The swap should clean it up before renaming the current derived.
        let tmp = tempdir().unwrap();
        force_no_embedder();
        let log = EventLog::open(tmp.path()).unwrap();
        let cap = capture("Another long enough preference statement here.");
        log.append(&cap).unwrap();
        drop(log);

        // Plant a stale derived.old with junk.
        let stash = tmp.path().join("derived.old");
        std::fs::create_dir_all(&stash).unwrap();
        std::fs::write(stash.join("junk.txt"), "leftover from a crash").unwrap();

        replay_home(tmp.path()).await.unwrap();
        restore_embedder_env();
        assert!(!stash.exists(), "stale stash must be removed");
        assert!(tmp.path().join("derived").is_dir());
    }

    #[tokio::test]
    async fn replay_skips_policy_and_import_events() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        let log = EventLog::open(tmp.path()).unwrap();
        let cap = capture("A capture long enough to trip the high_signal rule.");
        log.append(&cap).unwrap();
        let policy_ev = Event::new(
            EventKind::Policy(PolicyPayload {
                rule: "high_signal".into(),
                input_id: cap.id,
                action: PolicyAction::Commit,
                reasoning: Some("captured at write time".into()),
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "h".into(),
                user: None,
            },
        );
        log.append(&policy_ev).unwrap();
        let import_ev = Event::new(
            EventKind::Import(ImportPayload {
                source_format: "chatgpt".into(),
                count: 42,
                batch_id: "b1".into(),
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "h".into(),
                user: None,
            },
        );
        log.append(&import_ev).unwrap();
        drop(log);

        let stats = replay_home(tmp.path()).await.unwrap();
        restore_embedder_env();
        assert_eq!(stats.policy_events_skipped, 1);
        assert_eq!(stats.import_events_skipped, 1);
        // Captures still flow through normally.
        assert_eq!(stats.captures_seen, 1);
    }

    #[tokio::test]
    async fn replay_update_event_retires_old_and_inserts_new() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        let log = EventLog::open(tmp.path()).unwrap();

        let cap = capture("I used to live in Tokyo, then I moved to Berlin.");
        log.append(&cap).unwrap();
        let old_fact = fact_event("user", "Tokyo", cap.id);
        let old_fact_id = old_fact.id;
        log.append(&old_fact).unwrap();
        let update_ev = Event::new(
            EventKind::Update(UpdatePayload {
                supersedes_id: old_fact_id,
                new_fact: FactPayload {
                    subject: "user".into(),
                    predicate: "lives_in".into(),
                    object: "Berlin".into(),
                    confidence: 0.8,
                    valid_from: Utc::now(),
                    valid_to: None,
                    derived_from: vec![cap.id],
                    kind: Default::default(),
                    tags: Default::default(),
                    extra: Map::new(),
                },
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "h".into(),
                user: None,
            },
        );
        log.append(&update_ev).unwrap();
        drop(log);

        let stats = replay_home(tmp.path()).await.unwrap();
        restore_embedder_env();
        assert_eq!(stats.updates, 1);

        let facts = FactsStore::open(tmp.path()).unwrap();
        let rows = facts.facts_for_subject("user").unwrap();
        assert_eq!(rows.len(), 2);
        let tokyo = rows.iter().find(|r| r.object == "Tokyo").unwrap();
        let berlin = rows.iter().find(|r| r.object == "Berlin").unwrap();
        assert!(tokyo.retired_at.is_some(), "Tokyo row must be retired");
        assert!(berlin.retired_at.is_none(), "Berlin row must be live");
    }

    #[tokio::test]
    async fn replay_emits_stats_with_correct_counts() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        let log = EventLog::open(tmp.path()).unwrap();
        let cap1 = capture("First long-enough preference statement about Rust.");
        let cap2 = capture("Second long-enough preference statement about Haskell.");
        log.append(&cap1).unwrap();
        log.append(&cap2).unwrap();
        log.append(&fact_event("user", "rust", cap1.id)).unwrap();
        log.append(&fact_event("user", "haskell", cap2.id)).unwrap();
        log.append(&forget_event(cap1.id)).unwrap();
        drop(log);

        let stats = replay_home(tmp.path()).await.unwrap();
        restore_embedder_env();
        assert_eq!(stats.events_seen, 5);
        assert_eq!(stats.captures_seen, 2);
        assert_eq!(stats.committed, 2);
        assert_eq!(stats.facts_inserted, 2);
        assert_eq!(stats.forgets, 1);
    }
}
