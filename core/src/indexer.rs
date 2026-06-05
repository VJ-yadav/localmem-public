//! Indexer: routes each event to the right derived store.
//!
//! For capture events two orthogonal pipelines run:
//! - T-10 (Group B): embed via [`crate::embed`], write the vector to
//!   [`crate::vectors`], index the text in [`crate::lexical`].
//! - T-14 (Group C): run the rule-based extractor and persist any facts
//!   to `events.jsonl` and [`crate::facts`].
//!
//! Built by Groups B and C in parallel; each contributed orthogonal methods
//! on the same struct. The full-pipeline [`Indexer::new`] wires all five
//! stores; [`Indexer::facts_only`] is a constructor for environments
//! without the BGE-small ONNX model (offline CI, facts-only tests).

use crate::embed::Embedder;
use crate::event::{Event, EventKind, FactPayload, Source};
use crate::event_id::EventId;
use crate::event_log::EventLog;
use crate::extractor::{ExtractedFact, ExtractorRegistry};
use crate::facts::{Fact, FactsStore};
use crate::lexical::LexicalIndex;
use crate::vectors::VectorStore;
use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Map;

/// Bundle of derived-store writers wired together for the write pipeline.
///
/// The vector trio (`embedder`, `vector_store`, `lexical_index`) is optional
/// so a facts-only Indexer can be constructed without the ONNX model. The
/// `extractor` + `facts` pair is always required because the facts pipeline
/// has no heavy dependency and is exercised by unit tests on every platform.
pub struct Indexer {
    embedder: Option<Embedder>,
    vector_store: Option<VectorStore>,
    lexical_index: Option<LexicalIndex>,
    extractor: ExtractorRegistry,
    facts: FactsStore,
}

impl Indexer {
    /// Full-pipeline constructor. All five stores must be opened by the
    /// caller; this struct just bundles them. The `extractor` is the
    /// composed [`ExtractorRegistry`] (T-58) — the indexer itself is
    /// agnostic to whether the registry is rules-only or composed
    /// with LLM impls.
    pub fn new(
        embedder: Embedder,
        vector_store: VectorStore,
        lexical_index: LexicalIndex,
        extractor: ExtractorRegistry,
        facts: FactsStore,
    ) -> Self {
        Self {
            embedder: Some(embedder),
            vector_store: Some(vector_store),
            lexical_index: Some(lexical_index),
            extractor,
            facts,
        }
    }

    /// Facts-only constructor. [`Self::index_event`] becomes a no-op; the
    /// facts pipeline ([`Self::process_capture_facts`]) still works. Used
    /// by tests and any environment that does not load the ONNX embedder.
    pub fn facts_only(extractor: ExtractorRegistry, facts: FactsStore) -> Self {
        Self {
            embedder: None,
            vector_store: None,
            lexical_index: None,
            extractor,
            facts,
        }
    }

    /// Index a single event in the vector + lexical pipeline (T-10).
    ///
    /// `capture` events get embedded, written to LanceDB, and added to the
    /// Tantivy index. Other event kinds are no-ops. If the vector trio is
    /// not configured (facts-only mode), this is also a silent no-op.
    pub async fn index_event(&mut self, event: &Event) -> Result<()> {
        let EventKind::Capture(payload) = &event.kind else {
            return Ok(());
        };

        let Some(embedder) = self.embedder.as_mut() else {
            return Ok(());
        };
        let Some(vector_store) = self.vector_store.as_ref() else {
            return Ok(());
        };
        let Some(lexical_index) = self.lexical_index.as_mut() else {
            return Ok(());
        };

        let vector = embedder
            .embed(&payload.text)
            .context("embed capture content")?;
        vector_store
            .add(&event.id.to_string(), &vector, &payload.text, event.ts)
            .await
            .context("write embedding to vectors.lance")?;

        lexical_index
            .index_event(event)
            .context("index capture in lexical.tantivy")?;
        lexical_index
            .commit()
            .context("commit lexical writer after capture")?;
        Ok(())
    }

    /// Run rule-based extraction on a capture event and persist any facts
    /// it produced (T-14).
    ///
    /// For each extracted `(subject, predicate, object)`:
    /// 1. Append a `fact` event to `events.jsonl` (source of truth; replay
    ///    rebuilds the DuckDB row from this).
    /// 2. Insert the matching row into [`FactsStore`].
    ///
    /// Returns the ids of the new fact events. Non-capture inputs are a
    /// no-op returning an empty vec.
    ///
    /// Note: the policy decision (commit / dedup / skip / forget) is layered
    /// on top of this by Group D (T-15+). Once that lands, this method
    /// moves behind the policy gate.
    pub async fn process_capture_facts(
        &self,
        capture: &Event,
        event_log: &EventLog,
    ) -> Result<Vec<EventId>> {
        let EventKind::Capture(payload) = &capture.kind else {
            return Ok(Vec::new());
        };
        // T-58: registry runs every configured extractor in parallel.
        // The replay path is async (via `cli::replay::run`), so the
        // .await here is free; nothing in the existing call chain was
        // sync-only.
        let extracted = self
            .extractor
            .extract(&payload.text, Some(&payload.kind))
            .await
            .context("registry extract")?;
        let mut out = Vec::with_capacity(extracted.len());
        for ef in extracted {
            let fact_event = build_fact_event(&ef, capture);
            event_log
                .append(&fact_event)
                .context("append derived fact event")?;
            let fact = Fact::from_event(fact_event.id, fact_payload(&fact_event), Utc::now(), None);
            self.facts.insert(&fact).context("insert derived fact")?;
            out.push(fact_event.id);
        }
        Ok(out)
    }
}

fn build_fact_event(ef: &ExtractedFact, source_capture: &Event) -> Event {
    // valid_from defaults to the capture's recorded time. Without a deeper
    // language model we have no signal that the fact was true earlier or
    // later than the moment we observed it; "valid as of when we saw it"
    // is the conservative default. The user can override later via an
    // `update` event.
    let valid_from = source_capture.ts;
    // T-51b: tags / T-52: kind both inherit from the source capture
    // so replay reconstructs facts.tags + kind directly from
    // events.jsonl without a join back to the capture.
    let (inherited_tags, inherited_kind) = match &source_capture.kind {
        EventKind::Capture(p) => (p.tags.clone(), p.kind.clone()),
        _ => (
            std::collections::BTreeMap::new(),
            crate::kind::Kind::default(),
        ),
    };
    Event::new(
        EventKind::Fact(FactPayload {
            subject: ef.subject.clone(),
            predicate: ef.predicate.clone(),
            object: ef.object.clone(),
            confidence: ef.confidence,
            valid_from,
            valid_to: None,
            derived_from: vec![source_capture.id],
            kind: inherited_kind,
            tags: inherited_tags,
            extra: Map::new(),
        }),
        // Provenance: keep the source app of the original capture so the
        // journal can show which tool led to this fact.
        Source {
            app: source_capture.source.app.clone(),
            host: source_capture.source.host.clone(),
            user: source_capture.source.user.clone(),
        },
    )
}

fn fact_payload(event: &Event) -> &FactPayload {
    match &event.kind {
        EventKind::Fact(p) => p,
        // The event was constructed locally above with a Fact kind, so this
        // is unreachable at runtime. We avoid `unreachable!()` to keep the
        // panic guard in non-test paths.
        _ => panic!("build_fact_event must produce a Fact-kinded event"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{self, test_assets, EMBEDDING_DIM};
    use crate::event::CapturePayload;
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

    fn fact() -> Event {
        Event::new(
            EventKind::Fact(FactPayload {
                subject: "user".into(),
                predicate: "prefers".into(),
                object: "rust".into(),
                confidence: 0.9,
                valid_from: Utc::now(),
                valid_to: None,
                derived_from: vec![EventId::new()],
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

    fn facts_setup(home: &std::path::Path) -> (Indexer, EventLog) {
        let facts = FactsStore::open(home).unwrap();
        let event_log = EventLog::open(home).unwrap();
        let indexer = Indexer::facts_only(ExtractorRegistry::rules_only(), facts);
        (indexer, event_log)
    }

    async fn full_setup(home: &std::path::Path) -> Option<Indexer> {
        let dir = test_assets::ensure_model()?;
        let embedder = Embedder::load(&dir).expect("load BGE-small");
        let vector_store = VectorStore::open(home, EMBEDDING_DIM)
            .await
            .expect("open vector store");
        let lexical_index = LexicalIndex::open(home).expect("open lexical index");
        let facts = FactsStore::open(home).unwrap();
        Some(Indexer::new(
            embedder,
            vector_store,
            lexical_index,
            ExtractorRegistry::rules_only(),
            facts,
        ))
    }

    // ---- Group C tests: facts pipeline ----

    #[tokio::test]
    async fn capture_with_matching_rule_emits_fact_event_and_row() {
        let tmp = tempdir().unwrap();
        let (indexer, event_log) = facts_setup(tmp.path());

        let cap = capture("I prefer functional Rust.");
        event_log.append(&cap).unwrap();

        let fact_ids = indexer
            .process_capture_facts(&cap, &event_log)
            .await
            .unwrap();
        assert_eq!(fact_ids.len(), 1);

        let events: Vec<Event> = event_log
            .iter()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].kind, EventKind::Capture(_)));
        if let EventKind::Fact(p) = &events[1].kind {
            assert_eq!(p.subject, "user");
            assert_eq!(p.predicate, "prefers");
            assert_eq!(p.object, "functional Rust");
            assert_eq!(p.derived_from, vec![cap.id]);
        } else {
            panic!("expected fact event, got {:?}", events[1].kind);
        }

        assert_eq!(indexer.facts.count().unwrap(), 1);
        let rows = indexer.facts.facts_for_subject("user").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].object, "functional Rust");
        assert_eq!(rows[0].source_events, vec![cap.id]);
    }

    #[tokio::test]
    async fn capture_with_no_matching_rule_is_noop() {
        let tmp = tempdir().unwrap();
        let (indexer, event_log) = facts_setup(tmp.path());

        let cap = capture("Hello world");
        event_log.append(&cap).unwrap();

        let fact_ids = indexer
            .process_capture_facts(&cap, &event_log)
            .await
            .unwrap();
        assert!(fact_ids.is_empty());
        assert_eq!(indexer.facts.count().unwrap(), 0);

        let events: Vec<Event> = event_log
            .iter()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(events.len(), 1, "only the capture should be in the log");
    }

    #[tokio::test]
    async fn non_capture_event_is_noop() {
        let tmp = tempdir().unwrap();
        let (indexer, event_log) = facts_setup(tmp.path());

        let fact_ev = fact();
        let out = indexer
            .process_capture_facts(&fact_ev, &event_log)
            .await
            .unwrap();
        assert!(out.is_empty());
        assert_eq!(indexer.facts.count().unwrap(), 0);
    }

    #[tokio::test]
    async fn fact_event_provenance_matches_capture_source_app() {
        let tmp = tempdir().unwrap();
        let (indexer, event_log) = facts_setup(tmp.path());
        let mut cap = capture("Rust is fast");
        cap.source.app = "claude-code".into();
        event_log.append(&cap).unwrap();
        indexer
            .process_capture_facts(&cap, &event_log)
            .await
            .unwrap();

        let events: Vec<Event> = event_log
            .iter()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let fact_event = events.iter().find(|e| matches!(e.kind, EventKind::Fact(_)));
        let fact_event = fact_event.expect("fact event exists");
        assert_eq!(fact_event.source.app, "claude-code");
    }

    // ---- Group B tests: vector + lexical pipeline ----

    #[tokio::test]
    async fn capture_event_lands_in_both_stores() {
        let tmp = tempdir().unwrap();
        let Some(mut idx) = full_setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };

        let ev = capture("stripe webhook signature verification");
        idx.index_event(&ev).await.unwrap();
        // Drop the indexer so its writer lock on the lexical dir is
        // released before we open independent verifier handles below.
        drop(idx);

        let lex = LexicalIndex::open(tmp.path()).unwrap();
        let lex_hits = lex.search("stripe", 5, None).unwrap();
        assert_eq!(lex_hits.len(), 1);
        assert_eq!(lex_hits[0].event_id, ev.id.to_string());

        let vec_store = VectorStore::open(tmp.path(), EMBEDDING_DIM).await.unwrap();
        assert_eq!(vec_store.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn non_capture_events_are_skipped() {
        let tmp = tempdir().unwrap();
        let Some(mut idx) = full_setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        idx.index_event(&fact()).await.unwrap();
        drop(idx);

        let vec_store = VectorStore::open(tmp.path(), EMBEDDING_DIM).await.unwrap();
        assert_eq!(vec_store.count().await.unwrap(), 0);
        let lex = LexicalIndex::open(tmp.path()).unwrap();
        assert_eq!(lex.doc_count(), 0);
    }

    #[tokio::test]
    async fn capture_embedding_is_searchable() {
        let tmp = tempdir().unwrap();
        let Some(mut idx) = full_setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };

        let ev = capture("Rust is a memory-safe systems programming language.");
        idx.index_event(&ev).await.unwrap();
        drop(idx);

        let mut embedder = Embedder::load(test_assets::ensure_model().unwrap()).unwrap();
        let query = embedder.embed("memory-safe systems language").unwrap();

        let vec_store = VectorStore::open(tmp.path(), EMBEDDING_DIM).await.unwrap();
        let hits = vec_store.search(&query, 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event_id, ev.id.to_string());
    }

    // Touch `embed::EMBEDDING_DIM` from the wider module to keep the lint
    // happy on builds where the heavy integration tests are skipped.
    #[test]
    fn embedding_dim_constant_is_384() {
        assert_eq!(embed::EMBEDDING_DIM, 384);
    }
}
