//! The async understanding worker (the unified memory-layer design, Output A) and
//! the shared fact-persistence path it uses.
//!
//! `understand_worker` mirrors `embed_worker`: it pulls committed captures off
//! a bounded queue, runs the LLM [`Decomposer`] OFF the write path, and
//! persists the resulting facts. It never runs inside a hook and never blocks a
//! write, so the recursion + latency scar (a self-triggering hook) cannot recur. A
//! failed decomposition is logged and skipped; the raw capture remains the
//! source of truth, so the understanding is recoverable by re-running the pass.
//!
//! `persist_facts` is the per-fact promotion logic (build payload, T-56
//! contradiction resolution, emit `Fact`/`Update`, insert, journal). It used to
//! be inline in `routes::write`; the synchronous rules path and this async LLM
//! path now share it, so the two cannot drift.

use crate::event::{
    Event, EventKind, FactPayload, PolicyAction, Source, UnderstandingPayload, UnderstoodEntity,
    UpdatePayload,
};
use crate::event_log::EventLog;
use crate::extractor::ExtractedFact;
use crate::facts::{Fact, FactsStore};
use crate::journal::{Journal, JournalEntry as DerivedJournalEntry};
use crate::understanding::{DecomposeOptions, Decomposer, Decomposition};
use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Map;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

/// Bounded queue for captures awaiting understanding. Mirrors `EMBED_QUEUE_CAP`:
/// applies backpressure so a write burst can't grow memory unboundedly.
pub(crate) const UNDERSTAND_QUEUE_CAP: usize = 2048;

/// One capture awaiting decomposition. Carries the whole event so the worker
/// inherits the capture's tags, kind, valid-time, and source on the facts it
/// derives, with no join back to the log.
pub struct UnderstandJob {
    pub capture: Event,
}

/// Background understanding worker. Processes captures one at a time: each is a
/// separate LLM prompt, and sequential processing is naturally throttled, which
/// is what we want on a single user's machine (no thundering herd against the
/// local model). Runs until the channel closes (`AppState` dropped).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn understand_worker(
    mut rx: mpsc::Receiver<UnderstandJob>,
    decomposer: Arc<dyn Decomposer>,
    user_subject: String,
    model: String,
    event_log: Arc<EventLog>,
    facts: Arc<Mutex<FactsStore>>,
    journal: Arc<Journal>,
    pending: Arc<AtomicUsize>,
    briefing_dirty: Arc<Mutex<std::collections::HashSet<String>>>,
    // embed-both (intelligence v2): the worker also embeds the decomposed
    // summary via the SAME embed queue the write path uses. `None` when no
    // embedder is loaded (lex-only mode) — then we simply skip it.
    embed_tx: Option<mpsc::Sender<crate::server::EmbedJob>>,
    embed_pending: Arc<AtomicUsize>,
) {
    while let Some(job) = rx.recv().await {
        let capture = job.capture;
        // Ephemeral working memory (tool-use traces) never seeds the durable
        // understanding layer: decomposing it wastes a model call and pollutes
        // the entity graph + profile with command/file-path noise. The raw
        // trace stays in the event log for audit/replay; it just doesn't become
        // knowledge. This is the single choke point every enqueue source flows
        // through, so the rule holds for `/write` and backfill alike.
        if is_ephemeral_capture(&capture) {
            pending.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let (text, opts) = decompose_inputs(&capture, &user_subject);
        if text.trim().is_empty() {
            pending.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        match decomposer.decompose(&text, &opts).await {
            Ok(decomp) => {
                // Emit the understanding event FIRST: it carries summary + intent
                // + entities for the briefing/viewer AND marks the capture as
                // understood (the idempotency key a future backfill diffs on).
                // Then promote the facts, which fill the entity profile + graph.
                if let Err(err) = emit_understanding(&event_log, &capture, &decomp, &model) {
                    warn!(error = %err, event_id = %capture.id, "understanding: append event failed");
                }
                // P2 typed-graph NODE layer: record each decomposed entity as a
                // mention so the graph renders typed, deduplicated nodes (one
                // "localmem", not 50). Same store the facts land in; replay
                // rebuilds this identically from the Understanding event.
                if !decomp.entities.is_empty() {
                    let valid_from = match &capture.kind {
                        EventKind::Capture(p) => p.effective_capture_instant(capture.ts),
                        _ => capture.ts,
                    };
                    let source = capture.id.to_string();
                    let guard = facts.lock().await;
                    for e in &decomp.entities {
                        if let Err(err) =
                            guard.insert_entity_mention(&e.name, &e.kind, valid_from, &source)
                        {
                            warn!(error = %err, event_id = %capture.id, "understanding: insert entity mention failed");
                        }
                    }
                }
                // Mark this capture's project dirty so the debounced refresher
                // re-briefs it — keeps the cached briefing warm as work flows in.
                if let EventKind::Capture(p) = &capture.kind {
                    if let Some(proj) = p.tags.get("project") {
                        briefing_dirty.lock().await.insert(proj.clone());
                    }
                }
                // embed-both: also embed the DECOMPOSED summary under the SAME
                // capture id, giving semantic search a precision layer (sharp
                // "what's known") on top of the raw recall floor that `/write`
                // already embedded. Keying by the capture id means the
                // retriever's RRF-by-event_id merges the raw + summary hits into
                // ONE (boosted) result — no dedup logic needed. Replay rebuilds
                // this from the Understanding event, so it stays recomputable.
                if let (Some(tx), false) = (embed_tx.as_ref(), decomp.summary.trim().is_empty()) {
                    let (ts, tags) = match &capture.kind {
                        EventKind::Capture(p) => {
                            (p.effective_capture_instant(capture.ts), p.tags.clone())
                        }
                        _ => (capture.ts, std::collections::BTreeMap::new()),
                    };
                    embed_pending.fetch_add(1, Ordering::SeqCst);
                    let job = crate::server::EmbedJob {
                        event_id: capture.id.to_string(),
                        text: decomp.summary.clone(),
                        ts,
                        tags,
                    };
                    if tx.send(job).await.is_err() {
                        embed_pending.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                match persist_facts(
                    &event_log,
                    &facts,
                    &journal,
                    &capture,
                    &decomp.facts,
                    "understanding",
                )
                .await
                {
                    Ok(n) => info!(
                        event_id = %capture.id,
                        facts = n,
                        entities = decomp.entities.len(),
                        "understood capture"
                    ),
                    Err(err) => {
                        warn!(error = %err, event_id = %capture.id, "understanding: persist facts failed")
                    }
                }
            }
            Err(err) => warn!(
                error = %err,
                event_id = %capture.id,
                "understanding: decompose failed; capture stays raw (recoverable by re-running)",
            ),
        }
        // No longer pending regardless of outcome (a failure is recoverable by
        // re-running), so `/drain` can make progress.
        pending.fetch_sub(1, Ordering::SeqCst);
    }
}

/// True when a capture is short-lived working memory and must NOT seed the
/// durable understanding layer. Two equivalent signals, either is sufficient:
/// an `ephemeral:*` retention tag (the semantic "short-lived" marker the
/// hooks stamp on tool-use traces), or the `trace` sub-kind. The raw capture is
/// still a first-class event in the log — this only governs whether the
/// understanding worker spends a model call decomposing it into facts/entities.
fn is_ephemeral_capture(capture: &Event) -> bool {
    match &capture.kind {
        EventKind::Capture(p) => p.is_ephemeral(),
        _ => false,
    }
}

/// Append the `Understanding` event for a decomposed capture (its summary,
/// intent, typed entities, and the model that produced them), with tags and
/// valid-time inherited from the source capture. Append-only and immutable like
/// every event: a better model later emits a NEW understanding rather than
/// mutating this one.
fn emit_understanding(
    event_log: &EventLog,
    capture: &Event,
    decomp: &Decomposition,
    model: &str,
) -> Result<()> {
    let (tags, valid_from) = match &capture.kind {
        EventKind::Capture(p) => (p.tags.clone(), p.effective_capture_instant(capture.ts)),
        _ => (BTreeMap::new(), capture.ts),
    };
    let entities = decomp
        .entities
        .iter()
        .map(|e| UnderstoodEntity {
            name: e.name.clone(),
            kind: e.kind.clone(),
        })
        .collect();
    let payload = UnderstandingPayload {
        source_id: capture.id,
        summary: decomp.summary.clone(),
        intent: decomp.intent.clone(),
        entities,
        references: decomp.references.clone(),
        salience: decomp.salience.clone(),
        model: model.to_string(),
        valid_from,
        tags,
        extra: serde_json::Map::new(),
    };
    let event = Event::new(EventKind::Understanding(payload), fact_source(capture));
    event_log
        .append(&event)
        .context("append understanding event")
}

/// Whether a capture is worth the (async, LLM) understanding pass. Skips
/// EPHEMERAL captures — the tool-traces the PostToolUse hook writes with
/// `retention=ephemeral:` (e.g. `[Bash] ...`, `[mcp__localmem__memory_search]`).
/// Those are operational noise, not signal: understanding them wastes the model
/// and pollutes the briefing with the user's own memory-tool calls echoing back.
/// The raw trace is still captured + searchable; it just isn't decomposed.
pub(crate) fn worth_understanding(capture: &Event) -> bool {
    match &capture.kind {
        EventKind::Capture(p) => !p
            .tags
            .get(crate::reserved_tags::KEY_RETENTION)
            .map(|v| v.starts_with(crate::reserved_tags::RETENTION_EPHEMERAL_PREFIX))
            .unwrap_or(false),
        _ => false,
    }
}

/// Decompose inputs from a capture: the RAW text (the source of truth, not the
/// rewritten variant) plus options carrying the opaque `source` label, so the
/// model gets provenance context with zero per-tool branching.
fn decompose_inputs(capture: &Event, user_subject: &str) -> (String, DecomposeOptions) {
    let text = match &capture.kind {
        EventKind::Capture(p) => p.text.clone(),
        _ => String::new(),
    };
    let opts = DecomposeOptions {
        user_subject: user_subject.to_string(),
        source: Some(capture.source.app.clone()).filter(|s| !s.is_empty()),
    };
    (text, opts)
}

/// Promote extracted facts into the event log + facts store, running T-56
/// contradiction resolution per fact. Shared by `routes::write` (the synchronous
/// rules path) and `understand_worker` (the async LLM path). `rule` labels the
/// originating pipeline on the fact's provenance and any contradiction journal
/// entries. Returns the number of facts persisted.
pub(crate) async fn persist_facts(
    event_log: &EventLog,
    facts: &Mutex<FactsStore>,
    journal: &Journal,
    capture: &Event,
    extracted: &[ExtractedFact],
    rule: &str,
) -> Result<u32> {
    let mut count = 0u32;
    for ef in extracted {
        let new_payload = build_fact_payload(capture, ef);
        let new_event_id = crate::event_id::EventId::new();
        let mut new_fact = Fact::from_event(
            new_event_id,
            &new_payload,
            Utc::now(),
            Some(rule.to_string()),
        );

        // T-56 + P1: valid-time-ordered resolution. The DB UPDATE happens inside
        // resolve_contradiction; it also sets new_fact.retired_at when an
        // existing newer fact bounds this (older) one.
        let retired_ids = {
            let facts_guard = facts.lock().await;
            facts_guard
                .resolve_contradiction(&mut new_fact)
                .context("resolve_contradiction")?
        };

        let source = fact_source(capture);
        let event = if let Some(supersedes_id) = retired_ids.first().copied() {
            Event::with_id(
                new_event_id,
                EventKind::Update(UpdatePayload {
                    supersedes_id,
                    new_fact: new_payload.clone(),
                    extra: Map::new(),
                }),
                source,
            )
        } else {
            Event::with_id(new_event_id, EventKind::Fact(new_payload.clone()), source)
        };
        event_log
            .append(&event)
            .context("append derived fact / update event")?;
        {
            let facts_guard = facts.lock().await;
            facts_guard.insert(&new_fact).context("insert fact row")?;
        }

        for retired_id in &retired_ids {
            let entry = DerivedJournalEntry {
                ts: Utc::now(),
                action: PolicyAction::Update,
                rule: "smart_forgetting".into(),
                input_id: new_event_id,
                reasoning: Some(format!(
                    "retired {retired_id}: subject={} predicate={} new_object={:?}",
                    new_payload.subject, new_payload.predicate, new_payload.object,
                )),
            };
            journal.append(&entry).context("journal contradiction")?;
        }

        count += 1;
    }
    Ok(count)
}

/// Build a `FactPayload` for one extractor hit. T-51b tags / T-52 kind /
/// P1 valid-time all inherit from the source capture so the facts table can be
/// filtered and grouped without a join back to events.jsonl.
pub(crate) fn build_fact_payload(capture: &Event, ef: &ExtractedFact) -> FactPayload {
    let (inherited_tags, inherited_kind, valid_from) = match &capture.kind {
        EventKind::Capture(p) => (
            p.tags.clone(),
            p.kind.clone(),
            p.effective_capture_instant(capture.ts),
        ),
        _ => (BTreeMap::new(), crate::kind::Kind::default(), capture.ts),
    };
    FactPayload {
        subject: ef.subject.clone(),
        predicate: ef.predicate.clone(),
        object: ef.object.clone(),
        confidence: ef.confidence,
        valid_from,
        valid_to: None,
        derived_from: vec![capture.id],
        kind: inherited_kind,
        tags: inherited_tags,
        extra: Map::new(),
    }
}

/// Source for a derived fact/update event: inherits the capture's app/host/user
/// so the audit trail surfaces which tool led to the fact even after T-56
/// retires it via an `Update`.
fn fact_source(capture: &Event) -> Source {
    Source {
        app: capture.source.app.clone(),
        host: capture.source.host.clone(),
        user: capture.source.user.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CapturePayload, Source};
    use crate::journal::Journal;
    use crate::understanding::{DecomposeOptions, Decomposition};
    use async_trait::async_trait;
    use tempfile::TempDir;

    /// Returns a canned decomposition, so the worker is exercised end to end
    /// (queue -> decompose -> persist -> facts store) with no live model.
    struct StubDecomposer(Decomposition);

    #[async_trait]
    impl Decomposer for StubDecomposer {
        async fn decompose(&self, _text: &str, _opts: &DecomposeOptions) -> Result<Decomposition> {
            Ok(self.0.clone())
        }
    }

    fn capture_event(text: &str) -> Event {
        Event::new(
            EventKind::Capture(CapturePayload {
                text: text.to_string(),
                ..Default::default()
            }),
            Source {
                app: "claude-code".into(),
                host: "test".into(),
                user: Some("vijay".into()),
            },
        )
    }

    #[test]
    fn ephemeral_captures_skip_understanding_signal_does_not() {
        use std::collections::BTreeMap;
        // A normal signal capture (default Note kind, no retention) is decomposed.
        assert!(!is_ephemeral_capture(&capture_event(
            "Vijay prefers terse responses without preamble."
        )));

        // A tool-use trace is recognized by its ephemeral retention tag...
        let mut tags = BTreeMap::new();
        tags.insert("retention".to_string(), "ephemeral:7d".to_string());
        let by_retention = Event::new(
            EventKind::Capture(CapturePayload {
                text: "[Bash] cd /Users/vjsnapp/DATA_LAB/localmem".into(),
                tags,
                ..Default::default()
            }),
            Source {
                app: "claude-code".into(),
                host: "t".into(),
                user: None,
            },
        );
        assert!(is_ephemeral_capture(&by_retention));

        // ...and equivalently by its `trace` sub-kind (defense for older traces
        // written before retention tagging).
        let by_kind = Event::new(
            EventKind::Capture(CapturePayload {
                text: "[Read] events.jsonl".into(),
                kind: crate::kind::Kind::Other("trace".into()),
                ..Default::default()
            }),
            Source {
                app: "claude-code".into(),
                host: "t".into(),
                user: None,
            },
        );
        assert!(is_ephemeral_capture(&by_kind));
    }

    #[tokio::test]
    async fn worker_persists_decomposed_facts_into_the_store() {
        let dir = TempDir::new().unwrap();
        let home = dir.path();
        let event_log = Arc::new(EventLog::open(home).unwrap());
        let facts = Arc::new(Mutex::new(FactsStore::open(home).unwrap()));
        let journal = Arc::new(Journal::open(home).unwrap());
        let pending = Arc::new(AtomicUsize::new(0));

        let decomp = Decomposition {
            summary: "Vijay prefers local-first storage.".into(),
            intent: "record a preference".into(),
            entities: vec![],
            facts: vec![ExtractedFact {
                subject: "user".into(),
                predicate: "prefers".into(),
                object: "local-first storage".into(),
                confidence: 0.9,
            }],
            ..Default::default()
        };
        let decomposer: Arc<dyn Decomposer> = Arc::new(StubDecomposer(decomp));

        let (tx, rx) = mpsc::channel::<UnderstandJob>(8);
        let handle = tokio::spawn(understand_worker(
            rx,
            decomposer,
            "user".to_string(),
            "test-model".to_string(),
            event_log.clone(),
            facts.clone(),
            journal.clone(),
            pending.clone(),
            Arc::new(Mutex::new(std::collections::HashSet::new())),
            None,
            Arc::new(AtomicUsize::new(0)),
        ));

        let capture = capture_event("I like to keep my data local.");
        // The capture itself must be in the log so the derived fact's
        // derived_from id resolves; the worker only appends the fact event.
        event_log.append(&capture).unwrap();
        pending.fetch_add(1, Ordering::SeqCst);
        tx.send(UnderstandJob { capture }).await.unwrap();

        // Close the channel so the worker drains and exits.
        drop(tx);
        handle.await.unwrap();
        assert_eq!(pending.load(Ordering::SeqCst), 0, "pending settles to zero");

        let guard = facts.lock().await;
        let rows = guard.facts_for_subject("user").unwrap();
        assert!(
            rows.iter()
                .any(|f| f.predicate == "prefers" && f.object == "local-first storage"),
            "the decomposed fact landed in the facts store, got: {rows:?}"
        );
        drop(guard);

        // The understanding event is also appended, carrying the summary +
        // intent + model and pointing back at the capture (the briefing's data
        // and the backfill idempotency marker).
        let understanding: Vec<_> = event_log
            .iter()
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|e| match e.kind {
                EventKind::Understanding(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(understanding.len(), 1, "exactly one understanding event");
        assert_eq!(
            understanding[0].summary,
            "Vijay prefers local-first storage."
        );
        assert_eq!(understanding[0].model, "test-model");
    }

    #[test]
    fn worth_understanding_skips_ephemeral_traces() {
        // A signal capture (no retention tag) is understood.
        assert!(worth_understanding(&capture_event("a real decision")));

        // An ephemeral tool-trace (retention=ephemeral:7d) is skipped.
        let mut trace = capture_event("[Bash] ls");
        if let EventKind::Capture(p) = &mut trace.kind {
            p.tags.insert(
                crate::reserved_tags::KEY_RETENTION.to_string(),
                "ephemeral:7d".to_string(),
            );
        }
        assert!(!worth_understanding(&trace));
    }

    #[tokio::test]
    async fn worker_skips_empty_capture_without_persisting() {
        let dir = TempDir::new().unwrap();
        let home = dir.path();
        let event_log = Arc::new(EventLog::open(home).unwrap());
        let facts = Arc::new(Mutex::new(FactsStore::open(home).unwrap()));
        let journal = Arc::new(Journal::open(home).unwrap());
        let pending = Arc::new(AtomicUsize::new(0));

        // A decomposer that would yield a fact, to prove the empty-text guard
        // short-circuits BEFORE the model is consulted.
        let decomp = Decomposition {
            facts: vec![ExtractedFact {
                subject: "user".into(),
                predicate: "x".into(),
                object: "y".into(),
                confidence: 0.5,
            }],
            ..Default::default()
        };
        let decomposer: Arc<dyn Decomposer> = Arc::new(StubDecomposer(decomp));

        let (tx, rx) = mpsc::channel::<UnderstandJob>(8);
        let handle = tokio::spawn(understand_worker(
            rx,
            decomposer,
            "user".to_string(),
            "test-model".to_string(),
            event_log.clone(),
            facts.clone(),
            journal.clone(),
            pending.clone(),
            Arc::new(Mutex::new(std::collections::HashSet::new())),
            None,
            Arc::new(AtomicUsize::new(0)),
        ));

        pending.fetch_add(1, Ordering::SeqCst);
        tx.send(UnderstandJob {
            capture: capture_event("   "),
        })
        .await
        .unwrap();
        drop(tx);
        handle.await.unwrap();

        assert_eq!(pending.load(Ordering::SeqCst), 0);
        let guard = facts.lock().await;
        let rows = guard.facts_for_subject("user").unwrap();
        assert!(rows.is_empty(), "no facts persisted for empty capture");
    }
}
