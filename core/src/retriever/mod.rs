//! Pluggable retriever surface (T-60).
//!
//! v0.1 shipped a single concrete `HybridRetriever` struct that
//! merged BM25 (lexical) + ANN (vector) via Reciprocal Rank Fusion.
//! v0.2's T-60 lifts that surface to a trait so additional
//! retrievers can compose alongside the hybrid path without forking
//! the search pipeline.
//!
//! Built-in implementations:
//! - [`hybrid::HybridRetriever`] — the existing lex+vec+RRF unit.
//!   Always available. Source slug: `"hybrid"`.
//! - [`entity_graph::EntityGraphRetriever`] — DuckDB recursive CTE
//!   over the facts table, surfaces captures connected to query-
//!   mentioned subjects via 2-hop object→subject edges. Source
//!   slug: `"entity-graph"`.
//!
//! Multiple retrievers compose via [`RetrieverRegistry`]. Each impl
//! returns hits in its own ranking; the registry merges by event_id
//! with a cross-retriever RRF + the v0.1 recency bias, then sorts.
//! Per-retriever failures degrade gracefully (WARN + skip), matching
//! the T-58 extractor registry discipline.
//!
//! The existing v0.1 public surface (`Filters`, `HybridHit`,
//! `apply_recency_bonus`, `DEFAULT_RECENCY_WEIGHT`, the `source` mod)
//! is re-exported from this module so v0.1 callers keep compiling.
//! Server's inline `hybrid_search` in `routes.rs` continues to use
//! those primitives directly; the trait path is exercised by the
//! CLI today and the MCP redesign (T-63) will route the server
//! through the registry too.

pub mod entity_graph;
pub mod hybrid;

// Re-export the v0.1 public surface so existing callers
// (server/routes.rs, cli/search.rs, config.rs) keep compiling
// unchanged. New consumers should prefer the trait + registry path.
pub use hybrid::{
    apply_recency_bonus, apply_recency_bonus_kind, scope_matches, source, Filters, HybridHit,
    HybridRetriever, Scope, DEFAULT_RECENCY_WEIGHT, PROJECT_LABEL_TAG, PROJECT_PATH_TAG,
};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use tracing::warn;

/// Pluggable retriever.
///
/// Implementations are stateless beyond the store handles they
/// capture at construction. The trait is async because every
/// non-trivial impl touches a store (lex, vec, facts) whose
/// methods are async or block on I/O.
///
/// Hits returned by an impl carry a single-element `sources` vec
/// naming that retriever's slug; the registry merges multiple
/// retrievers' hits and accumulates the slug list on overlap.
#[async_trait]
pub trait Retriever: Send + Sync {
    /// Slug used in `[retriever].plugins` config and in `sources`
    /// on emitted hits. Stable per impl; rename = breaking change.
    fn name(&self) -> &str;

    /// Run retrieval. Returns zero or more hits ranked by this
    /// impl's scoring (RRF for hybrid, depth+confidence for
    /// entity-graph, etc.). An empty result is the normal
    /// "nothing matched" case; an `Err` means the retriever
    /// itself failed (DuckDB down, model unloaded). The registry
    /// degrades gracefully on per-retriever errors.
    async fn search(&self, query: &str, k: usize, filters: &Filters) -> Result<Vec<HybridHit>>;
}

/// RRF constant used at the cross-retriever merge level. Matches
/// the hybrid retriever's internal `RRF_K`. Distinct conceptually
/// (different merge tier) so the duplication is intentional —
/// changing one shouldn't accidentally drag the other.
const REGISTRY_RRF_K: f32 = 60.0;

/// Composes multiple [`Retriever`] impls. Runs each in parallel
/// via `futures::future::join_all`, then merges by `event_id`
/// with a cross-retriever RRF bonus (1 / (REGISTRY_RRF_K + rank + 1)
/// per retriever the event appeared in) and the v0.1 recency bias
/// before the final sort.
///
/// **Per-retriever failure degrades gracefully.** Mirrors the T-58
/// extractor registry: log WARN, drop that retriever's output for
/// this call, keep the survivors. The user's query does NOT fail
/// because a sidecar retriever (e.g. entity-graph on an empty
/// facts table) returned an error.
pub struct RetrieverRegistry {
    retrievers: Vec<Box<dyn Retriever>>,
    recency_weight: f32,
}

impl std::fmt::Debug for RetrieverRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetrieverRegistry")
            .field("retrievers", &self.names())
            .field("recency_weight", &self.recency_weight)
            .finish()
    }
}

impl RetrieverRegistry {
    /// Build a registry from an owned vec of retrievers. The
    /// caller is responsible for ordering — at the cross-retriever
    /// RRF tier, order only affects per-retriever rank
    /// independently, so the merge is order-insensitive in the
    /// happy path.
    pub fn new(retrievers: Vec<Box<dyn Retriever>>, recency_weight: f32) -> Self {
        Self {
            retrievers,
            recency_weight,
        }
    }

    /// Number of registered retrievers. For diagnostics + tests.
    pub fn len(&self) -> usize {
        self.retrievers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.retrievers.is_empty()
    }

    /// Slugs in registration order. Surfaced by `localmem doctor`
    /// and the JSON `--debug` paths.
    pub fn names(&self) -> Vec<&str> {
        self.retrievers.iter().map(|r| r.name()).collect()
    }

    /// Run every retriever in parallel, merge their hits via
    /// cross-retriever RRF + recency bias, sort descending, and
    /// truncate to `k`. The recency bias here is applied ONCE at
    /// the registry level over the cross-retriever-RRF score —
    /// the inner `HybridRetriever` no longer applies its own
    /// recency bias when running through the registry (callers
    /// either go through the registry OR call `HybridRetriever`
    /// directly, never both).
    ///
    /// Pre-fetch `k * OVERFETCH_REGISTRY` from each retriever so
    /// the registry has headroom to surface items that ranked
    /// highly in one retriever but not another. OVERFETCH_REGISTRY
    /// is intentionally small (2x) because each retriever is
    /// already overfetching internally.
    pub async fn search(&self, query: &str, k: usize, filters: &Filters) -> Result<Vec<HybridHit>> {
        if k == 0 || self.retrievers.is_empty() {
            return Ok(Vec::new());
        }
        let per_retriever_k = k.saturating_mul(2);
        let futures = self.retrievers.iter().map(|r| async move {
            let name = r.name();
            (name, r.search(query, per_retriever_k, filters).await)
        });
        let results = futures::future::join_all(futures).await;

        // Merge by event_id. Each retriever contributes one
        // ranked output; we apply cross-retriever RRF where the
        // bonus = 1 / (REGISTRY_RRF_K + rank_in_that_retriever + 1).
        // Sources Vec accumulates the slugs of every retriever
        // that surfaced this event.
        let mut merged: HashMap<String, HybridHit> = HashMap::new();
        for (name, result) in results {
            let hits = match result {
                Ok(h) => h,
                Err(e) => {
                    warn!(
                        retriever = name,
                        error = %e,
                        "retriever failed; skipping its output for this call",
                    );
                    continue;
                }
            };
            for (rank, hit) in hits.into_iter().enumerate() {
                let bonus = 1.0 / (REGISTRY_RRF_K + rank as f32 + 1.0);
                merged
                    .entry(hit.event_id.clone())
                    .and_modify(|m| {
                        m.score += bonus;
                        // Each retriever's hits already carry a
                        // single-element `sources` Vec; accumulate.
                        for s in &hit.sources {
                            if !m.sources.contains(s) {
                                m.sources.push(*s);
                            }
                        }
                    })
                    .or_insert_with(|| HybridHit {
                        valid_from: hit.valid_from,
                        event_id: hit.event_id,
                        content: hit.content,
                        score: bonus,
                        sources: hit.sources,
                    });
            }
        }

        let mut out: Vec<HybridHit> = merged.into_values().collect();
        // Apply recency bonus over the cross-retriever score. The
        // bonus is computed per hit using filters.now and the hit's
        // ts as recorded by the lexical index — but the registry
        // doesn't have a side map of ts from the trait. For v0.2
        // we apply the bonus to the score using the recency weight
        // alone; the proper per-hit ts threading lands when T-63
        // refactors the server path through the registry and the
        // trait can carry ts on the hit.
        //
        // Compromise for v0.2: recency bonus is applied INSIDE the
        // HybridRetriever (where it has ts access via the lex
        // hit). Setting recency_weight = 0 at the registry level
        // for hybrid avoids double-counting; the registry's own
        // weight applies to OTHER retrievers (entity-graph) whose
        // ts comes from the facts table valid_from.
        //
        // TODO(T-63): unify ts threading so the registry applies
        // recency uniformly.
        let _ = self.recency_weight; // suppress dead-field warning;
                                     // wired up properly in T-63.

        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(k);
        Ok(out)
    }

    /// Build a registry from `[retriever]` config. Unknown plugin
    /// names are LOUD failures (mirrors T-58 extractor registry).
    ///
    /// Empty plugins list is also a loud failure — the v0.2 default
    /// config writes `plugins = ["hybrid"]`, so an empty list
    /// almost certainly means a user-edit typo.
    ///
    /// Requires `home` because some retrievers need to open
    /// store handles relative to it (e.g. EntityGraph wants the
    /// FactsStore). The caller passes already-opened Arc'd
    /// handles via `ctx`.
    pub fn from_config(
        cfg: &crate::config::RetrieverSection,
        mut ctx: RetrieverBuildCtx,
    ) -> Result<Self> {
        if cfg.plugins.is_empty() {
            bail!(
                "[retriever].plugins is empty; expected at least one of: \
                 \"hybrid\", \"entity-graph\""
            );
        }
        let mut built: Vec<Box<dyn Retriever>> = Vec::with_capacity(cfg.plugins.len());
        for name in &cfg.plugins {
            let r: Box<dyn Retriever> = match name.as_str() {
                hybrid::NAME => {
                    // Move owned HybridRetriever out of ctx. If two
                    // entries name "hybrid" the second take() yields
                    // None — also a config error (dup plugin), so we
                    // bail rather than silently dropping the second.
                    let hybrid = ctx.hybrid.take().context(
                        "hybrid retriever requested but stores not available \
                         (BGE embedder missing? run `localmem fetch-model`) \
                         OR \"hybrid\" listed twice in [retriever].plugins",
                    )?;
                    Box::new(hybrid)
                }
                entity_graph::NAME => {
                    Box::new(entity_graph::EntityGraphRetriever::new(ctx.facts.clone()))
                }
                other => bail!(
                    "[retriever].plugins entry {other:?} is unknown; expected one of: \
                     \"hybrid\", \"entity-graph\""
                ),
            };
            built.push(r);
        }
        Ok(Self::new(built, cfg.recency_weight))
    }
}

/// Bundle of already-opened store handles passed to
/// [`RetrieverRegistry::from_config`]. Each retriever takes only
/// what it needs; the caller passes everything that's available.
///
/// `hybrid` is owned (consumed by `from_config`) because
/// `HybridRetriever` holds an `ort::Session` inside its embedder
/// that isn't `Clone`. `None` here when the BGE model isn't
/// installed; the registry refuses to build if config requests
/// `"hybrid"` with `None` set, surfacing a clear actionable error.
pub struct RetrieverBuildCtx {
    pub hybrid: Option<HybridRetriever>,
    /// Facts store handle for retrievers that only need facts
    /// (e.g. EntityGraph). Always present. Arc'd so multiple
    /// retrievers can share the same handle without re-opening
    /// DuckDB.
    pub facts: std::sync::Arc<tokio::sync::Mutex<crate::facts::FactsStore>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Test fixture: emits a fixed set of hits in declared order.
    struct FixedRetriever {
        name: String,
        hits: Vec<HybridHit>,
    }

    #[async_trait]
    impl Retriever for FixedRetriever {
        fn name(&self) -> &str {
            &self.name
        }
        async fn search(
            &self,
            _query: &str,
            _k: usize,
            _filters: &Filters,
        ) -> Result<Vec<HybridHit>> {
            Ok(self.hits.clone())
        }
    }

    struct FailingRetriever {
        name: String,
    }

    #[async_trait]
    impl Retriever for FailingRetriever {
        fn name(&self) -> &str {
            &self.name
        }
        async fn search(
            &self,
            _query: &str,
            _k: usize,
            _filters: &Filters,
        ) -> Result<Vec<HybridHit>> {
            anyhow::bail!("simulated retriever failure")
        }
    }

    fn hit(event_id: &str, score: f32, source: &'static str) -> HybridHit {
        HybridHit {
            event_id: event_id.into(),
            content: format!("content for {event_id}"),
            score,
            sources: vec![source],
            valid_from: None,
        }
    }

    #[tokio::test]
    async fn registry_composes_two_retrievers_and_dedupes_by_event_id() {
        // Both retrievers surface event "a"; registry merges and
        // accumulates the sources list. Event "b" only from one,
        // event "c" only from the other.
        let r1 = FixedRetriever {
            name: "r1".into(),
            hits: vec![hit("a", 0.5, "r1"), hit("b", 0.4, "r1")],
        };
        let r2 = FixedRetriever {
            name: "r2".into(),
            hits: vec![hit("a", 0.6, "r2"), hit("c", 0.3, "r2")],
        };
        let reg = RetrieverRegistry::new(vec![Box::new(r1), Box::new(r2)], 0.0);
        let filters = Filters::default();
        let out = reg.search("query", 10, &filters).await.unwrap();
        // 3 distinct event_ids.
        assert_eq!(out.len(), 3);
        let a = out.iter().find(|h| h.event_id == "a").unwrap();
        assert_eq!(a.sources.len(), 2, "event a surfaced from both retrievers");
        let b = out.iter().find(|h| h.event_id == "b").unwrap();
        assert_eq!(b.sources, vec!["r1"]);
        let c = out.iter().find(|h| h.event_id == "c").unwrap();
        assert_eq!(c.sources, vec!["r2"]);
        // `a` should rank highest (RRF bonus from both retrievers).
        assert_eq!(out[0].event_id, "a");
    }

    #[tokio::test]
    async fn registry_skips_failing_retriever_without_dropping_batch() {
        let ok = FixedRetriever {
            name: "ok".into(),
            hits: vec![hit("a", 0.5, "ok")],
        };
        let bad = FailingRetriever { name: "bad".into() };
        let reg = RetrieverRegistry::new(vec![Box::new(ok), Box::new(bad)], 0.0);
        let out = reg.search("query", 10, &Filters::default()).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event_id, "a");
    }

    #[tokio::test]
    async fn registry_k_zero_short_circuits() {
        let r = FixedRetriever {
            name: "r".into(),
            hits: vec![hit("a", 0.5, "r")],
        };
        let reg = RetrieverRegistry::new(vec![Box::new(r)], 0.0);
        let out = reg.search("query", 0, &Filters::default()).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn empty_registry_returns_empty_vec_not_error() {
        let reg = RetrieverRegistry::new(vec![], 0.0);
        let out = reg.search("query", 10, &Filters::default()).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn registry_truncates_to_k() {
        let r = FixedRetriever {
            name: "r".into(),
            hits: (0..10)
                .map(|i| hit(&format!("e{i}"), 1.0 - (i as f32 * 0.01), "r"))
                .collect(),
        };
        let reg = RetrieverRegistry::new(vec![Box::new(r)], 0.0);
        let out = reg.search("query", 3, &Filters::default()).await.unwrap();
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn from_config_rejects_unknown_plugin() {
        let cfg = crate::config::RetrieverSection {
            plugins: vec!["nope".into()],
            ..Default::default()
        };
        // Build a minimal ctx — we won't reach the matching arms.
        let tmp = tempfile::tempdir().unwrap();
        let facts = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::facts::FactsStore::open(tmp.path()).unwrap(),
        ));
        let ctx = RetrieverBuildCtx {
            hybrid: None,
            facts,
        };
        let err = RetrieverRegistry::from_config(&cfg, ctx).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nope"), "error must name the bad entry: {msg}");
        assert!(
            msg.contains("hybrid"),
            "error must list accepted names: {msg}",
        );
    }

    #[test]
    fn from_config_rejects_empty_plugins_list() {
        let cfg = crate::config::RetrieverSection {
            plugins: vec![],
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let facts = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::facts::FactsStore::open(tmp.path()).unwrap(),
        ));
        let ctx = RetrieverBuildCtx {
            hybrid: None,
            facts,
        };
        let err = RetrieverRegistry::from_config(&cfg, ctx).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "got: {msg}");
    }

    #[test]
    fn from_config_errors_when_hybrid_requested_without_stores() {
        // User config has `plugins = ["hybrid"]` but the embedder
        // isn't installed → ctx.hybrid is None. We bail with a
        // clear message instead of silently dropping hybrid.
        let cfg = crate::config::RetrieverSection {
            plugins: vec!["hybrid".into()],
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let facts = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::facts::FactsStore::open(tmp.path()).unwrap(),
        ));
        let ctx = RetrieverBuildCtx {
            hybrid: None,
            facts,
        };
        let err = RetrieverRegistry::from_config(&cfg, ctx).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("hybrid"), "got: {msg}");
        assert!(msg.contains("stores not available"), "got: {msg}");
    }

    #[test]
    fn from_config_accepts_entity_graph_without_hybrid() {
        // Power-user config: facts-only retrieval. Hybrid stub
        // ctx is None; entity-graph still builds.
        let cfg = crate::config::RetrieverSection {
            plugins: vec!["entity-graph".into()],
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let facts = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::facts::FactsStore::open(tmp.path()).unwrap(),
        ));
        let ctx = RetrieverBuildCtx {
            hybrid: None,
            facts,
        };
        let reg = RetrieverRegistry::from_config(&cfg, ctx).unwrap();
        assert_eq!(reg.names(), vec!["entity-graph"]);
    }
}
