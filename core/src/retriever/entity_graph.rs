//! Entity-graph retriever (T-60).
//!
//! Walks the bitemporal facts table via DuckDB recursive CTEs to
//! surface captures connected to query-mentioned subjects. The
//! traversal follows `object → subject` edges: if a fact says
//! `(A, predicate, B)` and the query mentions A, we surface B's
//! facts too, up to 2 hops out.
//!
//! Why bother: BM25 + ANN both rank by surface similarity to the
//! query. Entity-graph traversal surfaces memories that don't
//! mention the query terms but ARE connected to them by typed
//! facts the user has already laid down. "What do you know about
//! my project?" pulls in captures about teammates, deadlines,
//! design choices etc. that share fact-graph connections to
//! "project" without containing that word.
//!
//! Today's scope (v0.2):
//! - Substring-match query against `FactsStore::subjects()` for
//!   the seed set.
//! - Hardcoded 2-hop depth bound; configurable in v0.2.1 if user
//!   feedback demands it.
//! - Returns CAPTURE event ids (not fact ids) via
//!   `source_events[0]` so the result lives in the same id-space
//!   as the lex/vec retrievers' hits.
//! - Retired facts are excluded from the walk.
//! - Confidence threshold matches T-56 smart-forgetting (≥ 0.7)
//!   to avoid surfacing low-signal speculative facts as graph
//!   evidence.

use super::{Filters, HybridHit, Retriever};
use crate::facts::FactsStore;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Slug used in `[retriever].plugins` config and in
/// `Retriever::name()`.
pub const NAME: &str = "entity-graph";

/// Source tag stamped on hits this retriever emits. Surfaced in
/// `HybridHit.sources` so audit traces can attribute results.
pub const SOURCE_TAG: &str = "entity-graph";

/// Maximum graph walk depth. 0 = seed only, 1 = one hop, 2 = two
/// hops. Hardcoded for v0.2 first cut; bumping it would
/// dramatically increase result set size on a dense graph and we
/// don't yet have a UX surface for tuning.
const MAX_DEPTH: u32 = 2;

/// Minimum confidence for facts considered as graph edges. Below
/// this threshold, the fact is treated as too speculative to
/// trust for transitive inference. Matches T-56's smart-forgetting
/// gate so the two systems agree on "what's trustworthy."
const MIN_EDGE_CONFIDENCE: f64 = 0.7;

pub struct EntityGraphRetriever {
    facts: Arc<Mutex<FactsStore>>,
}

impl EntityGraphRetriever {
    /// Build a retriever over a shared facts handle. Clones cheap;
    /// the Arc is what the registry stores.
    pub fn new(facts: Arc<Mutex<FactsStore>>) -> Self {
        Self { facts }
    }
}

#[async_trait]
impl Retriever for EntityGraphRetriever {
    fn name(&self) -> &str {
        NAME
    }

    async fn search(
        &self,
        query: &str,
        k: usize,
        _filters: &Filters,
    ) -> Result<Vec<HybridHit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let facts = self.facts.lock().await;

        // Seed: subjects (case-insensitive substring match against
        // query). `subjects()` includes retired rows for audit, but
        // the walk SQL below filters on retired_at IS NULL anyway,
        // so seeds that exist only via retired facts won't expand.
        let all_subjects = facts.subjects().context("read subjects for entity-graph seed")?;
        let query_lower = query.to_ascii_lowercase();
        let seeds: Vec<String> = all_subjects
            .into_iter()
            .filter_map(|(subject, _count)| {
                if query_lower.contains(&subject.to_ascii_lowercase()) {
                    Some(subject)
                } else {
                    None
                }
            })
            .collect();

        if seeds.is_empty() {
            // Query mentions no known subject → entity-graph has
            // nothing to add. Not an error; the registry just sees
            // an empty hit list from this retriever.
            return Ok(Vec::new());
        }

        let hits = facts
            .entity_graph_walk(&seeds, MAX_DEPTH, MIN_EDGE_CONFIDENCE, k)
            .context("entity-graph walk")?;
        Ok(hits
            .into_iter()
            .map(|row| HybridHit {
                event_id: row.capture_id,
                content: format!("{} {} {}", row.subject, row.predicate, row.object),
                score: row.score,
                sources: vec![SOURCE_TAG],
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_id::EventId;
    use crate::facts::Fact;
    use chrono::{DateTime, Utc};
    use tempfile::tempdir;

    fn ts(epoch: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(epoch, 0).unwrap()
    }

    fn fact(subject: &str, predicate: &str, object: &str, confidence: f64) -> Fact {
        Fact {
            id: EventId::new(),
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.into(),
            confidence,
            valid_from: ts(1_700_000_000),
            valid_to: None,
            recorded_at: ts(1_700_000_000),
            retired_at: None,
            source_events: vec![EventId::new()],
            policy_id: None,
            tags: Default::default(),
            kind: Default::default(),
        }
    }

    async fn store_with(facts: Vec<Fact>) -> (tempfile::TempDir, Arc<Mutex<FactsStore>>) {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        for f in facts {
            store.insert(&f).unwrap();
        }
        let arc = Arc::new(Mutex::new(store));
        (tmp, arc)
    }

    #[tokio::test]
    async fn returns_empty_when_query_mentions_no_known_subject() {
        let (_tmp, facts) = store_with(vec![fact("alice", "knows", "bob", 0.9)]).await;
        let r = EntityGraphRetriever::new(facts);
        let out = r.search("totally unrelated", 10, &Filters::default()).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn returns_empty_when_facts_store_has_no_subjects() {
        let (_tmp, facts) = store_with(vec![]).await;
        let r = EntityGraphRetriever::new(facts);
        let out = r.search("anything", 10, &Filters::default()).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn surfaces_one_hop_neighbour_of_known_subject() {
        // alice -knows-> bob -lives_in-> paris
        // Query mentions "alice"; we should surface (alice knows bob)
        // AND (bob lives_in paris) via one hop.
        let f_ab = fact("alice", "knows", "bob", 0.9);
        let f_bp = fact("bob", "lives_in", "paris", 0.9);
        let (_tmp, facts) = store_with(vec![f_ab.clone(), f_bp.clone()]).await;
        let r = EntityGraphRetriever::new(facts);
        let out = r
            .search("tell me about alice", 10, &Filters::default())
            .await
            .unwrap();
        // Seed fact (alice knows bob) AND its hop (bob lives_in paris).
        assert_eq!(out.len(), 2, "got: {out:?}");
        let objects: Vec<&str> = out.iter().map(|h| h.content.as_str()).collect();
        assert!(objects.iter().any(|c| c.contains("bob")));
        assert!(objects.iter().any(|c| c.contains("paris")));
        // Every hit carries the entity-graph source tag.
        for hit in &out {
            assert_eq!(hit.sources, vec![SOURCE_TAG]);
        }
    }

    #[tokio::test]
    async fn surfaces_two_hop_chain_but_not_three() {
        // a -> b -> c -> d. Query mentions a. We should see (a→b),
        // (b→c), (c→d). d→? at depth 3 is excluded.
        let f1 = fact("alpha", "linked_to", "beta", 0.9);
        let f2 = fact("beta", "linked_to", "gamma", 0.9);
        let f3 = fact("gamma", "linked_to", "delta", 0.9);
        let f4 = fact("delta", "linked_to", "epsilon", 0.9); // depth 3, excluded
        let (_tmp, facts) = store_with(vec![f1, f2, f3, f4]).await;
        let r = EntityGraphRetriever::new(facts);
        let out = r
            .search("alpha", 10, &Filters::default())
            .await
            .unwrap();
        // depth 0 (alpha→beta) + depth 1 (beta→gamma) + depth 2
        // (gamma→delta). delta→epsilon is depth 3, dropped.
        assert_eq!(out.len(), 3, "got: {out:?}");
        for hit in &out {
            assert!(
                !hit.content.contains("epsilon"),
                "depth-3 fact must not surface, got: {}",
                hit.content
            );
        }
    }

    #[tokio::test]
    async fn excludes_low_confidence_edges() {
        // Edge with confidence < 0.7 should not be traversed.
        let mut weak = fact("alice", "maybe_likes", "rust", 0.5);
        weak.confidence = 0.5;
        let (_tmp, facts) = store_with(vec![weak]).await;
        let r = EntityGraphRetriever::new(facts);
        let out = r
            .search("alice", 10, &Filters::default())
            .await
            .unwrap();
        // The seed subject exists, but the only edge is too
        // speculative; nothing surfaces.
        assert!(out.is_empty(), "got: {out:?}");
    }

    #[tokio::test]
    async fn excludes_retired_facts() {
        let mut retired_edge = fact("alice", "knows", "bob", 0.9);
        retired_edge.retired_at = Some(ts(1_700_000_500));
        let live = fact("alice", "lives_in", "paris", 0.9);
        let (_tmp, facts) = store_with(vec![retired_edge, live]).await;
        let r = EntityGraphRetriever::new(facts);
        let out = r
            .search("alice", 10, &Filters::default())
            .await
            .unwrap();
        // Only (alice lives_in paris) surfaces; the retired edge is
        // skipped.
        assert_eq!(out.len(), 1, "got: {out:?}");
        assert!(out[0].content.contains("lives_in paris"));
    }

    #[tokio::test]
    async fn k_zero_short_circuits() {
        let (_tmp, facts) = store_with(vec![fact("alice", "knows", "bob", 0.9)]).await;
        let r = EntityGraphRetriever::new(facts);
        let out = r.search("alice", 0, &Filters::default()).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn case_insensitive_subject_match() {
        let f = fact("Alice", "knows", "bob", 0.9);
        let (_tmp, facts) = store_with(vec![f]).await;
        let r = EntityGraphRetriever::new(facts);
        // Query mentions "alice" (lowercase); subject is "Alice".
        let out = r.search("alice", 10, &Filters::default()).await.unwrap();
        assert_eq!(out.len(), 1);
    }
}
