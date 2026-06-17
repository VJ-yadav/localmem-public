//! Hybrid retriever: lexical (BM25) + vector (ANN) blended by Reciprocal
//! Rank Fusion, with an optional bitemporal filter.
//!
//! Why RRF instead of a weighted score sum: vector scores from
//! [`crate::vectors::VectorStore::search`] are in `[0, 1]` (transformed from
//! L2 distance) while BM25 scores from [`crate::lexical::LexicalIndex::search`]
//! are unbounded floats whose range varies with corpus statistics. A direct
//! weighted blend needs per-corpus calibration and is brittle; RRF combines
//! by rank position rather than raw score and is the folklore default for
//! exactly this asymmetry. See docs/INTEGRATION.md "Why Reciprocal Rank Fusion".
//!
//! Temporal filter: when the caller provides an `at_time`, hits whose
//! downstream facts have all been retired by that time are dropped. The SQL
//! lives in [`crate::facts::FactsStore::is_event_valid_at`] so this module
//! does not embed query strings.

use crate::embed::Embedder;
use crate::facts::FactsStore;
use crate::lexical::LexicalIndex;
use crate::vectors::VectorStore;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use tokio::sync::Mutex;

/// Slug used in `[retriever].plugins` config and in
/// `Retriever::name()`. Stable per impl; rename = breaking change.
pub const NAME: &str = "hybrid";

/// RRF normalization constant. 60 is the folklore default from Cormack et al.
/// (2009). Larger values flatten the curve (closer to "rank doesn't matter");
/// smaller values steepen it (top rank dominates). 60 strikes a balance that
/// works without tuning for typical web/corpus sizes.
const RRF_K: f32 = 60.0;

/// Multiplier applied to `k` when fetching from each retriever before RRF
/// merging. Overfetching gives RRF room to surface items that rank highly in
/// one retriever but not the other.
const OVERFETCH: usize = 3;

/// T-57: default recency-bias weight. Calibrated so the bonus for a
/// freshly-written capture is the same order of magnitude as one RRF
/// rank position (≈0.016 at rank 0), letting recency break ties
/// without overwhelming strong lexical / vector signal. Per
/// SPEC_V0_2 "Recency-biased retrieval".
pub const DEFAULT_RECENCY_WEIGHT: f32 = 0.01;

/// Time constant for the recency decay. 30 days means a 30-day-old
/// capture gets `1/e` of the freshly-written bonus; a 90-day-old
/// capture gets `1/e^3` ≈ 5%. Matches the spec phrasing
/// `exp(-age_days / 30)`.
const RECENCY_TAU_DAYS: f64 = 30.0;

/// Apply the recency bias term to a retrieved-hit score. Shared
/// between [`HybridRetriever::search`] and the duplicated hybrid
/// path on the HTTP `/search` route so both surfaces stay aligned.
///
/// `weight == 0.0` short-circuits. Otherwise the bonus is
/// `weight * exp(-age_days / RECENCY_TAU_DAYS)` where `age_days` is
/// the gap from `ts` to `now` clamped at zero (a capture whose ts
/// is in the future — clock skew — still gets the maximum bonus
/// rather than a negative age).
///
/// This is the uniform-tau path retained for backwards compat. T-73
/// callers reach for [`apply_recency_bonus_kind`] when per-kind
/// half-lives are configured.
pub fn apply_recency_bonus(score: f32, ts: DateTime<Utc>, now: DateTime<Utc>, weight: f32) -> f32 {
    if weight == 0.0 {
        return score;
    }
    let age_seconds = (now - ts).num_seconds().max(0) as f64;
    let age_days = age_seconds / 86_400.0;
    let decay = (-age_days / RECENCY_TAU_DAYS).exp() as f32;
    score + weight * decay
}

/// T-73: kind-aware recency bonus. Uses `weight * 0.5^(age_days /
/// half_life_days)` when `half_life_days` is set; otherwise falls
/// back to the legacy uniform exp-decay (`apply_recency_bonus`) so a
/// missing entry in the config map preserves v0.1 behavior exactly.
///
/// The half-life form `0.5^(age/H)` makes the bonus exactly half its
/// fresh value at age=H, which matches the SPEC_V0_2 phrasing and
/// Memento's per-kind half-lives. Both forms agree at age=0 (both
/// return `weight`) so a 0-age fact's bonus is identical regardless
/// of which path applies.
pub fn apply_recency_bonus_kind(
    score: f32,
    ts: DateTime<Utc>,
    now: DateTime<Utc>,
    weight: f32,
    half_life_days: Option<f64>,
) -> f32 {
    if weight == 0.0 {
        return score;
    }
    let age_seconds = (now - ts).num_seconds().max(0) as f64;
    let age_days = age_seconds / 86_400.0;
    let decay = match half_life_days {
        Some(h) if h > 0.0 => 0.5f64.powf(age_days / h) as f32,
        _ => (-age_days / RECENCY_TAU_DAYS).exp() as f32,
    };
    score + weight * decay
}

/// Tag attached to a hit to indicate which retriever surfaced it.
pub mod source {
    pub const LEX: &str = "lex";
    pub const VEC: &str = "vec";
}

/// Query-time filters applied across all retrieval paths (T-51, T-51c).
///
/// Empty defaults preserve v0.1 behavior; every existing test that
/// doesn't care about filtering passes `&Filters::default()`. Future
/// task slices will extend this struct (kind, source) so the
/// retriever surface stays stable as filters grow.
/// Project-scope predicate (SPEC-intelligence-v2 §2.8): restrict retrieval to
/// the current project PLUS user-common (global) memory, never another project.
/// `key` is the scoping tag (`project_path`, the collision-proof key the hooks
/// stamp, or `project`). A hit passes when its `key` tag equals `value`, or, when
/// `include_global`, the hit carries no `key` tag at all (an untagged, explicit
/// `memory_write` = user-common capture). Cross-project retrieval is the absence
/// of a scope, set only on explicit opt-in.
#[derive(Debug, Clone)]
pub struct Scope {
    pub key: String,
    pub value: String,
    pub include_global: bool,
}

/// Canonical tag key for the COLLISION-PROOF project scope: the full working
/// directory the memory was captured in (set by the capture hook, see
/// `core/src/cli/hooks.rs`). This is the key every scoped read should use; the
/// readable [`PROJECT_LABEL_TAG`] is for display only and can collide across
/// repos that share a basename.
pub const PROJECT_PATH_TAG: &str = "project_path";

/// Readable project label (basename of the cwd). Display-only; not collision
/// proof. Kept as a constant so the one place that still scopes by label
/// (legacy / a user typing a name) references the same string as the hook.
pub const PROJECT_LABEL_TAG: &str = "project";

impl Scope {
    /// The canonical project scope: match the collision-proof `project_path`
    /// tag, and include global (untagged) user-common memory. This is the
    /// single constructor every channel (search, profile, recall, events,
    /// graph, journal, ...) should use so scoping behaves identically.
    pub fn project_path(value: impl Into<String>) -> Self {
        Self {
            key: PROJECT_PATH_TAG.to_string(),
            value: value.into(),
            include_global: true,
        }
    }

    /// Scope by the readable `project` label instead of the full path. Use only
    /// when a path is unavailable (a user typed a name). Still includes global.
    pub fn project_label(value: impl Into<String>) -> Self {
        Self {
            key: PROJECT_LABEL_TAG.to_string(),
            value: value.into(),
            include_global: true,
        }
    }
}

/// SPEC §2.8 project-scope predicate, as a free function so EVERY read path
/// (the retriever, facts-backed reads, event-backed reads) enforces the exact
/// same rule. `true` when scoping is off, or the hit carries the scoped
/// key=value, or (when `include_global`) the hit has no such tag (untagged =
/// user-common). A hit tagged with ANOTHER project's value returns `false`.
pub fn scope_matches(tags: &BTreeMap<String, String>, scope: &Option<Scope>) -> bool {
    match scope {
        None => true,
        Some(s) => match tags.get(&s.key) {
            Some(v) => v == &s.value,
            None => s.include_global,
        },
    }
}

#[derive(Debug, Clone)]
pub struct Filters {
    /// Subset match on the capture's container tags (T-51). A hit
    /// passes when every `(key, value)` in this map matches the
    /// capture's tags exactly. Empty = no tag filtering.
    pub tags: BTreeMap<String, String>,
    /// SPEC §2.8 project scope. `None` = unscoped (legacy / explicit "all").
    pub scope: Option<Scope>,
    /// Reserved-tag visibility policy (T-51c). [`Visibility::Default`]
    /// excludes captures tagged `visibility=private`;
    /// [`Visibility::IncludePrivate`] surfaces them and is the
    /// audit-grade mode used only by entity-only `recall(entity=X)`.
    pub visibility: crate::reserved_tags::Visibility,
    /// Reference instant for the retention TTL check (T-51c). Stored
    /// on the filters so a single query is consistent across lex +
    /// vec + facts paths that might evaluate `now` at slightly
    /// different times.
    pub now: DateTime<Utc>,
    /// T-60: bitemporal filter. When set, hits whose downstream
    /// facts have all been retired by `at_time` are dropped. Lives
    /// on `Filters` (rather than as a separate `search` param) so
    /// the `Retriever` trait's signature is uniform across impls.
    /// `None` (the default) disables the filter and matches v0.1
    /// behaviour exactly.
    pub at_time: Option<DateTime<Utc>>,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            tags: BTreeMap::new(),
            scope: None,
            visibility: crate::reserved_tags::Visibility::Default,
            now: Utc::now(),
            at_time: None,
        }
    }
}

impl Filters {
    /// Convenience: build a Filters from a tag map alone. Visibility
    /// and `now` take their defaults.
    pub fn with_tags(tags: BTreeMap<String, String>) -> Self {
        Self {
            tags,
            ..Self::default()
        }
    }

    /// Apply the reserved-tag predicate to a `(tags, ts)` pair using
    /// this filter's `visibility` and `now`. Returns true when the
    /// hit should surface.
    pub fn passes_reserved(&self, tags: &BTreeMap<String, String>, ts: DateTime<Utc>) -> bool {
        crate::reserved_tags::is_visible(tags, ts, self.now, self.visibility)
    }

    /// SPEC §2.8 project-scope predicate. Delegates to the shared
    /// [`scope_matches`] free function so the retriever and every other read
    /// path enforce one identical rule.
    pub fn passes_scope(&self, tags: &BTreeMap<String, String>) -> bool {
        scope_matches(tags, &self.scope)
    }
}

/// Merged hit returned by [`HybridRetriever::search`]. `score` is the RRF
/// sum across the retrievers that found this event; `sources` lists those
/// retrievers (`["lex"]`, `["vec"]`, or both) in insertion order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HybridHit {
    pub event_id: String,
    pub content: String,
    pub score: f32,
    pub sources: Vec<&'static str>,
    /// Valid-time of the hit (when the underlying capture/fact actually
    /// occurred), threaded from the per-hit ts the retriever already computes
    /// for recency. Surfaced so callers (server `/search`, CLI) can do
    /// temporal reasoning. `None` only when the ts side-map has no entry,
    /// which shouldn't happen for an indexed capture. Completes the T-63 TODO.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<DateTime<Utc>>,
}

/// Bundle of retrieval primitives wired together for hybrid search.
///
/// The retriever takes `&mut self` because the embedder's `embed` call holds
/// an `&mut Session` from `ort`. The lexical index is read-only here so no
/// mutability flows from that side.
pub struct HybridRetriever {
    /// Embedder lives behind a `Mutex` because `embed` takes
    /// `&mut Session`. Interior mutability lets the public
    /// [`Self::search`] method take `&self`, which is required for
    /// `Box<dyn Retriever>` dispatch through the T-60 registry.
    /// One query at a time per retriever instance is the contract;
    /// the Mutex is held only across one embed call.
    embedder: std::sync::Arc<Mutex<Option<Embedder>>>,
    vectors: std::sync::Arc<Option<VectorStore>>,
    /// Wrapped in Mutex so the struct is `Sync` (Tantivy's
    /// internals are not Sync via the writer-lock path). T-60
    /// `Retriever` trait requires `Send + Sync` for dyn dispatch.
    lexical: std::sync::Arc<Mutex<LexicalIndex>>,
    /// Wrapped in `Arc<Mutex>` so the EntityGraphRetriever and
    /// HybridRetriever can share one DuckDB connection. DuckDB's
    /// `Connection` carries a `RefCell` (not Sync) so the Mutex
    /// is also what satisfies the `Sync` trait-bound here.
    facts: std::sync::Arc<Mutex<FactsStore>>,
    /// T-57: recency-bias weight. Set via [`Self::with_recency_weight`]
    /// after construction so the existing two-arg constructor surface
    /// stays source-compatible.
    recency_weight: f32,
    /// T-73: per-kind half-life (in days). Empty map (the default
    /// constructed via [`Self::new`]) means uniform exp-decay applies
    /// to every hit; populated map applies `0.5^(age/H)` to captures
    /// whose stored kind matches a key. Unknown / missing kinds fall
    /// back to the uniform tau path so backwards compat is preserved.
    decay_half_life_days: std::collections::HashMap<String, f64>,
    /// Phase 2 / T-74: MMR diversity lambda in `[0,1]`. `None` disables the
    /// diversity re-rank (default), preserving pure relevance ordering.
    mmr_lambda: Option<f32>,
    /// Phase 2 / T-74b: optional cross-encoder reranker, shared behind a mutex
    /// like the embedder (`Session::run` needs `&mut`). `None` inside = no
    /// reranking (default).
    reranker: std::sync::Arc<Mutex<Option<crate::rerank::Reranker>>>,
}

impl HybridRetriever {
    /// Construct a retriever over already-opened stores. Each store retains
    /// its own ownership rules (Embedder needs &mut, VectorStore is async).
    pub fn new(
        embedder: Embedder,
        vectors: VectorStore,
        lexical: LexicalIndex,
        facts: FactsStore,
    ) -> Self {
        Self::new_shared_facts(
            embedder,
            vectors,
            lexical,
            std::sync::Arc::new(Mutex::new(facts)),
        )
    }

    /// Construct from an already-Arc'd FactsStore so multiple
    /// retrievers (HybridRetriever + EntityGraphRetriever) can
    /// share one DuckDB connection. The CLI registry path uses
    /// this; the legacy direct-CLI construction goes through
    /// [`Self::new`] above.
    pub fn new_shared_facts(
        embedder: Embedder,
        vectors: VectorStore,
        lexical: LexicalIndex,
        facts: std::sync::Arc<Mutex<FactsStore>>,
    ) -> Self {
        Self::new_shared(
            std::sync::Arc::new(Mutex::new(Some(embedder))),
            std::sync::Arc::new(Some(vectors)),
            std::sync::Arc::new(Mutex::new(lexical)),
            facts,
        )
    }

    /// Construct from fully-shared handles (T-63). This is the form the server
    /// uses: it shares the same `Arc<Mutex<…>>` stores it already holds in
    /// `AppState` (and the write path uses), so ONE retrieval path serves both
    /// CLI and server with no duplicated search code. `embedder` and `vectors`
    /// are `Option` so the retriever degrades to lexical-only when the BGE
    /// model isn't installed, instead of failing.
    pub fn new_shared(
        embedder: std::sync::Arc<Mutex<Option<Embedder>>>,
        vectors: std::sync::Arc<Option<VectorStore>>,
        lexical: std::sync::Arc<Mutex<LexicalIndex>>,
        facts: std::sync::Arc<Mutex<FactsStore>>,
    ) -> Self {
        Self {
            decay_half_life_days: std::collections::HashMap::new(),
            embedder,
            vectors,
            lexical,
            facts,
            recency_weight: DEFAULT_RECENCY_WEIGHT,
            mmr_lambda: None,
            reranker: std::sync::Arc::new(Mutex::new(None)),
        }
    }

    /// Override the recency-bias weight (T-57). `0.0` disables the
    /// bonus entirely; the CLI search handler threads `[retriever]
    /// .recency_weight` through here.
    pub fn with_recency_weight(mut self, weight: f32) -> Self {
        self.recency_weight = weight;
        self
    }

    /// T-73: set per-kind half-lives (days). Empty map (the default)
    /// preserves T-57's uniform exp-decay path. Keys are canonical
    /// kind names (`fact`, `preference`, `decision`, `constraint`,
    /// `todo`, `note`); unknown kind values fall back to uniform tau.
    pub fn with_decay_half_lives(
        mut self,
        half_lives: std::collections::HashMap<String, f64>,
    ) -> Self {
        self.decay_half_life_days = half_lives;
        self
    }

    /// Phase 2 / T-74: set the MMR diversity lambda. `None` (default) disables
    /// MMR. See [`crate::config::RetrieverSection::mmr_lambda`].
    pub fn with_mmr_lambda(mut self, lambda: Option<f32>) -> Self {
        self.mmr_lambda = lambda;
        self
    }

    /// Phase 2 / T-74b: attach an optional cross-encoder reranker, shared
    /// behind a mutex (like the embedder). `None` inside disables reranking.
    pub fn with_reranker(
        mut self,
        reranker: std::sync::Arc<Mutex<Option<crate::rerank::Reranker>>>,
    ) -> Self {
        self.reranker = reranker;
        self
    }

    /// Run a hybrid search.
    ///
    /// Steps:
    /// 1. Fetch `OVERFETCH * k` hits from each retriever (lex and vec).
    /// 2. Apply `filters.tags` at each layer: lex filters via stored
    ///    tags on the document, vec filters post-search by looking up
    ///    each hit's tags against the lex index (the vector store does
    ///    not carry tag metadata).
    /// 3. Merge by `event_id` using RRF (sum of `1 / (k_rrf + rank + 1)`
    ///    from each retriever the event appeared in).
    /// 4. If `at_time` is set, drop events whose facts have all been retired.
    /// 5. Sort by descending score and truncate to `k`.
    pub async fn search(
        &self,
        query: &str,
        k: usize,
        at_time: Option<DateTime<Utc>>,
        filters: &Filters,
    ) -> Result<Vec<HybridHit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let fetch = k.saturating_mul(OVERFETCH);
        let active_tags: Option<&BTreeMap<String, String>> = if filters.tags.is_empty() {
            None
        } else {
            Some(&filters.tags)
        };

        // Lexical pass first: BM25 is cheap and synchronous. If it errors we
        // surface the error rather than silently degrading to vec-only,
        // because empty BM25 results are common and meaningful (no exact
        // term hit) but errors mean the index is broken. The lex layer
        // filters tags inline via overfetch + post-filter; we additionally
        // strip hits that violate the reserved-tag rules (T-51c) using
        // the ts + tags fields the lex hit carries.
        let lex_hits: Vec<crate::lexical::LexicalHit> = {
            let lex = self.lexical.lock().await;
            let raw = lex
                .search(query, fetch, active_tags)
                .context("hybrid: lexical pass")?;
            raw.into_iter()
                .filter(|h| filters.passes_reserved(&h.tags, h.ts) && filters.passes_scope(&h.tags))
                .collect()
        };

        // Vector pass: embed the query, then ANN search. Embedding is the
        // bottleneck on a cold cache. The embedder is wrapped in a Mutex so
        // this struct's `search` can be `&self` (required for trait
        // dispatch through `Box<dyn Retriever>`); contention is bounded
        // because each retriever instance serves one query at a time.
        // Vector pass is optional (T-63): with no embedder/vector store
        // (e.g. the BGE model isn't installed) we degrade to lexical-only
        // rather than fail. This is what lets the server route every search
        // through one retrieval path regardless of model availability.
        let query_vec = {
            let mut emb = self.embedder.lock().await;
            match emb.as_mut() {
                Some(e) => Some(e.embed(query).context("hybrid: embed query")?),
                None => None,
            }
        };
        let vec_hits_raw = match (query_vec.as_ref(), self.vectors.as_ref().as_ref()) {
            (Some(qv), Some(vs)) => vs.search(qv, fetch).await.context("hybrid: vector pass")?,
            _ => Vec::new(),
        };

        // Vec hits don't carry tag metadata; look up via the lex index
        // (every capture is indexed in lex by construction, so this is
        // always answerable). One `meta_for` call per vec hit handles
        // both the tag subset filter (T-51) and the reserved-tag rules
        // (T-51c) AND seeds the `ts` side map T-57 needs for the
        // recency bonus. `fetch` is small (≤ k * OVERFETCH), so the
        // cost stays bounded.
        let mut vec_hits = Vec::with_capacity(vec_hits_raw.len());
        // T-57 + T-73: side map from event_id → (capture ts, kind).
        // Lex hits supply both directly; vec hits via the `meta_for`
        // lookup. Stored outside HybridHit so the public hit shape
        // stays unchanged.
        let mut meta_by_id: HashMap<String, (DateTime<Utc>, String)> = HashMap::new();
        {
            // Hold the lex lock once for the whole vec-hit
            // enrichment loop so we don't pay the lock acquisition
            // cost per hit.
            let lex = self.lexical.lock().await;
            for vh in vec_hits_raw {
                // COHESION (§2.8): filter by the vector row's OWN tags + ts, not a
                // borrowed lexical lookup that can diverge. Each store
                // self-describes, so scope/tag/reserved filtering is identical on
                // the lex and vec paths and the two cannot disagree.
                if let Some(tag_filter) = active_tags {
                    if !crate::tag_match::matches(&vh.tags, tag_filter) {
                        continue;
                    }
                }
                if !filters.passes_reserved(&vh.tags, vh.ts) {
                    continue;
                }
                if !filters.passes_scope(&vh.tags) {
                    continue;
                }
                // `kind` is only the recency half-life input; a lexical miss
                // defaults harmlessly (ranking nicety, not correctness).
                let kind = lex
                    .meta_for(&vh.event_id)
                    .context("hybrid: lookup vec-hit kind")?
                    .map(|m| m.kind)
                    .unwrap_or_default();
                meta_by_id.insert(vh.event_id.clone(), (vh.ts, kind));
                vec_hits.push(vh);
            }
        }

        // RRF merge keyed by event_id. The first INSERT sets the content; later
        // hits for the same id only add score. P5 (§2.6): process VEC hits FIRST
        // so the best-scoring CHUNK (vec_hits are relevance-ordered) becomes the
        // snippet, not a lexical full-text dump. Only a lexical-ONLY hit falls
        // back to its snippet, which the post-merge cap then bounds.
        let mut merged: HashMap<String, HybridHit> = HashMap::new();
        for (rank, h) in vec_hits.iter().enumerate() {
            let bonus = rrf_score(rank);
            merged
                .entry(h.event_id.clone())
                .and_modify(|m| {
                    m.score += bonus;
                    if !m.sources.contains(&source::VEC) {
                        m.sources.push(source::VEC);
                    }
                })
                .or_insert_with(|| HybridHit {
                    event_id: h.event_id.clone(),
                    content: h.content.clone(),
                    score: bonus,
                    sources: vec![source::VEC],
                    valid_from: None, // filled from meta_by_id below
                });
        }
        for (rank, h) in lex_hits.iter().enumerate() {
            let bonus = rrf_score(rank);
            meta_by_id
                .entry(h.event_id.clone())
                .or_insert_with(|| (h.ts, h.kind.clone()));
            merged
                .entry(h.event_id.clone())
                .and_modify(|m| {
                    m.score += bonus;
                    if !m.sources.contains(&source::LEX) {
                        m.sources.push(source::LEX);
                    }
                })
                .or_insert_with(|| HybridHit {
                    event_id: h.event_id.clone(),
                    content: h.snippet.clone(),
                    score: bonus,
                    sources: vec![source::LEX],
                    valid_from: None, // filled from meta_by_id below
                });
        }

        // Bitemporal filter. A missing fact row means the capture never
        // produced a fact, which is the "keep" case in is_event_valid_at.
        // The filter runs after merging so the SQL touches at most
        // `merged.len()` rows. Hold the facts lock once for the
        // whole pass rather than per-hit re-acquiring.
        if let Some(t) = at_time {
            let mut keep: HashMap<String, HybridHit> = HashMap::with_capacity(merged.len());
            let facts = self.facts.lock().await;
            for (event_id, hit) in merged {
                let valid = facts
                    .is_event_valid_at(&event_id, t)
                    .with_context(|| format!("temporal filter for {event_id}"))?;
                if valid {
                    keep.insert(event_id, hit);
                }
            }
            merged = keep;
        }

        let mut out: Vec<HybridHit> = merged.into_values().collect();
        // P5 (§2.6): cap every snippet so no single hit dumps a huge blob into an
        // agent's context (the measured 14K-token failure). A full chunk passes
        // uncut; a lexical-only full-text hit is bounded. This also keeps the
        // reranker scoring sharp snippets, not essays.
        for h in out.iter_mut() {
            h.content = crate::chunk::cap_words(&h.content, crate::chunk::SNIPPET_CAP_WORDS);
        }
        // T-63 completion: surface each hit's valid-time. The ts already lives
        // in the `meta_by_id` side-map (seeded from the lex hit's ts and the
        // vec hit's meta lookup) for the recency bonus; thread it onto the
        // public hit so the server `/search` response and CLI can do temporal
        // reasoning instead of receiving a hardcoded `None`.
        for hit in out.iter_mut() {
            if let Some((ts, _kind)) = meta_by_id.get(&hit.event_id) {
                hit.valid_from = Some(*ts);
            }
        }
        // T-57: apply recency bias. Computed off filters.now so the
        // reference instant matches the retention/visibility checks
        // upstream (a single, consistent "now" across the query).
        // Hits without a ts in the side map (shouldn't happen given
        // every capture is indexed) skip the bonus, falling back to
        // RRF-only ranking.
        if self.recency_weight != 0.0 {
            for hit in out.iter_mut() {
                if let Some((ts, kind)) = meta_by_id.get(&hit.event_id) {
                    // T-73: kind-aware half-life lookup. Empty kind
                    // string (legacy docs) and unknown extension
                    // kinds resolve to `None` → uniform tau via
                    // `apply_recency_bonus_kind`'s fallback branch.
                    let half_life = if kind.is_empty() {
                        None
                    } else {
                        self.decay_half_life_days.get(kind).copied()
                    };
                    hit.score = apply_recency_bonus_kind(
                        hit.score,
                        *ts,
                        filters.now,
                        self.recency_weight,
                        half_life,
                    );
                }
            }
        }
        // Descending score order. `partial_cmp` returns `None` only for NaN
        // values; our scores are sums of finite positive floats, so falling
        // back to `Ordering::Equal` would only fire on a corrupted invariant.
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Phase 2 / T-74b: cross-encoder rerank — rescore the top-N candidates
        // by true query-doc relevance before diversity/truncation. No-op when
        // no reranker model is loaded.
        let mut out = self.apply_rerank(query, out).await?;
        // Phase 2 / T-74: optional MMR diversity re-rank over the top
        // candidates. Falls back to plain score-order truncation when MMR is
        // disabled or no embedder is available.
        match self.mmr_lambda {
            Some(lambda) => self.apply_mmr(out, lambda, k).await,
            None => {
                out.truncate(k);
                Ok(out)
            }
        }
    }

    /// Phase 2 / T-74b: cross-encoder rescore of the top-N candidates. Rescores
    /// up to `RERANK_CANDIDATES` hits by true query-doc relevance (overwriting
    /// each hit's score) and reorders by the new scores. Returns the input
    /// unchanged when no reranker model is loaded, so callers degrade to the
    /// first-stage ranking.
    async fn apply_rerank(&self, query: &str, mut hits: Vec<HybridHit>) -> Result<Vec<HybridHit>> {
        const RERANK_CANDIDATES: usize = 30;
        let mut guard = self.reranker.lock().await;
        let rr = match guard.as_mut() {
            Some(r) => r,
            None => return Ok(hits),
        };
        if hits.len() > RERANK_CANDIDATES {
            hits.truncate(RERANK_CANDIDATES);
        }
        if hits.is_empty() {
            return Ok(hits);
        }
        let docs: Vec<&str> = hits.iter().map(|h| h.content.as_str()).collect();
        let scores = rr.rerank(query, &docs).context("rerank candidates")?;
        for (h, s) in hits.iter_mut().zip(scores.iter()) {
            h.score = *s;
        }
        Ok(crate::rerank::reorder_by_scores(
            hits,
            &scores,
            RERANK_CANDIDATES,
        ))
    }

    /// Phase 2 / T-74: re-rank `hits` (already sorted by relevance) with
    /// Maximal Marginal Relevance and truncate to `k`. Embeds each candidate's
    /// content so diversity is measured on the same space as retrieval, bounded
    /// to `k * MMR_CANDIDATES_PER_K` candidates to cap embedding cost. When no
    /// embedder is present we cannot measure similarity, so we degrade to plain
    /// score-order truncation.
    async fn apply_mmr(
        &self,
        mut hits: Vec<HybridHit>,
        lambda: f32,
        k: usize,
    ) -> Result<Vec<HybridHit>> {
        const MMR_CANDIDATES_PER_K: usize = 4;
        let cap = k.saturating_mul(MMR_CANDIDATES_PER_K).max(k);
        if hits.len() > cap {
            hits.truncate(cap);
        }
        let embeddings: Vec<Vec<f32>> = {
            let mut emb_guard = self.embedder.lock().await;
            let emb = match emb_guard.as_mut() {
                Some(e) => e,
                None => {
                    hits.truncate(k);
                    return Ok(hits);
                }
            };
            let mut v = Vec::with_capacity(hits.len());
            for h in &hits {
                v.push(emb.embed(&h.content).context("mmr: embed candidate")?);
            }
            v
        };
        let items: Vec<(f32, Vec<f32>)> = hits
            .iter()
            .zip(embeddings)
            .map(|(h, e)| (h.score, e))
            .collect();
        let order = mmr_select(&items, lambda, k);
        let mut slots: Vec<Option<HybridHit>> = hits.into_iter().map(Some).collect();
        let mut selected = Vec::with_capacity(order.len());
        for i in order {
            if let Some(h) = slots[i].take() {
                selected.push(h);
            }
        }
        Ok(selected)
    }
}

fn rrf_score(rank: usize) -> f32 {
    // Rank-0 (the top hit) gets the largest bonus. The `+1` shifts to a
    // 1-based rank so the top hit scores `1 / (k + 1)` per the RRF formula.
    1.0 / (RRF_K + rank as f32 + 1.0)
}

/// Cosine similarity of two vectors. Returns 0 when either is the zero vector.
fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// Maximal Marginal Relevance selection (T-74). Greedily picks up to `k`
/// indices from `items` (each `(relevance_score, embedding)`) maximizing
/// `lambda * normalized_relevance - (1 - lambda) * max_sim_to_already_picked`.
/// Relevance is min-max normalized to `[0,1]` so it combines comparably with
/// cosine similarity. `lambda = 1.0` is pure relevance (the input order);
/// lower values trade relevance for diversity. Returns selected indices in
/// pick order.
fn mmr_select(items: &[(f32, Vec<f32>)], lambda: f32, k: usize) -> Vec<usize> {
    let n = items.len();
    let target = k.min(n);
    if target == 0 {
        return Vec::new();
    }
    let lambda = lambda.clamp(0.0, 1.0);
    let (mut min_r, mut max_r) = (f32::INFINITY, f32::NEG_INFINITY);
    for (r, _) in items {
        min_r = min_r.min(*r);
        max_r = max_r.max(*r);
    }
    let span = (max_r - min_r).max(f32::EPSILON);
    let norm_rel = |i: usize| (items[i].0 - min_r) / span;

    let mut selected: Vec<usize> = Vec::with_capacity(target);
    let mut remaining: Vec<usize> = (0..n).collect();
    while selected.len() < target && !remaining.is_empty() {
        let mut best_pos = 0usize;
        let mut best_score = f32::NEG_INFINITY;
        for (pos, &i) in remaining.iter().enumerate() {
            let max_sim = selected
                .iter()
                .map(|&s| cosine_sim(&items[i].1, &items[s].1))
                .fold(0.0f32, f32::max);
            let score = lambda * norm_rel(i) - (1.0 - lambda) * max_sim;
            if score > best_score {
                best_score = score;
                best_pos = pos;
            }
        }
        selected.push(remaining.remove(best_pos));
    }
    selected
}

// ---------------------------------------------------------------------------
// T-60: Retriever trait impl. Delegates to the existing `search` method.
// `at_time` rides on the Filters via the trait surface (Filters::at_time);
// for backward compat the legacy `search(query, k, at_time, filters)` API
// stays available for direct (non-trait) callers like the server's inline
// hybrid_search.
// ---------------------------------------------------------------------------

#[async_trait]
impl super::Retriever for HybridRetriever {
    fn name(&self) -> &str {
        NAME
    }

    async fn search(&self, query: &str, k: usize, filters: &Filters) -> Result<Vec<HybridHit>> {
        // The trait method has no separate at_time param; the
        // bitemporal filter rides on Filters.at_time. For T-60
        // first cut, the legacy `search()` accepts at_time
        // directly; we read it off filters here.
        Self::search(self, query, k, filters.at_time, filters).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{test_assets, EMBEDDING_DIM};
    use crate::event::{CapturePayload, Event, EventKind, FactPayload, Source};
    use crate::event_id::EventId;
    use crate::facts::Fact;
    use chrono::Utc;
    use serde_json::Map;
    use tempfile::tempdir;

    #[test]
    fn scope_matches_enforces_project_plus_global() {
        // The one predicate every read path delegates to. Locks the SPEC §2.8
        // semantic: unscoped passes all; scoped keeps the project + untagged
        // global, drops another project; include_global=false drops untagged.
        let tagged = |k: &str, v: &str| {
            let mut m = BTreeMap::new();
            m.insert(k.to_string(), v.to_string());
            m
        };
        let lm = tagged(PROJECT_PATH_TAG, "/home/lm");
        let other = tagged(PROJECT_PATH_TAG, "/home/other");
        let global = BTreeMap::new();

        // Unscoped: everything passes.
        assert!(scope_matches(&lm, &None));
        assert!(scope_matches(&global, &None));

        // Scoped (project + global): in-project and untagged pass, other drops.
        let scope = Some(Scope::project_path("/home/lm"));
        assert!(scope_matches(&lm, &scope), "in-project passes");
        assert!(scope_matches(&global, &scope), "untagged global passes");
        assert!(!scope_matches(&other, &scope), "another project dropped");

        // include_global=false: untagged global is excluded too.
        let strict = Some(Scope {
            key: PROJECT_PATH_TAG.to_string(),
            value: "/home/lm".to_string(),
            include_global: false,
        });
        assert!(scope_matches(&lm, &strict));
        assert!(!scope_matches(&global, &strict), "global excluded when off");
    }

    #[test]
    fn mmr_select_diversifies_near_duplicates() {
        // item0 and item1 share an embedding (near-duplicates); item2 is
        // orthogonal. With lambda 0.5, after the top item the orthogonal one
        // beats the near-duplicate.
        let items = vec![
            (1.0f32, vec![1.0, 0.0]),
            (0.9f32, vec![1.0, 0.0]),
            (0.5f32, vec![0.0, 1.0]),
        ];
        assert_eq!(mmr_select(&items, 0.5, 2), vec![0, 2]);
    }

    #[test]
    fn mmr_select_lambda_one_is_pure_relevance() {
        let items = vec![
            (1.0f32, vec![1.0, 0.0]),
            (0.9f32, vec![1.0, 0.0]),
            (0.5f32, vec![0.0, 1.0]),
        ];
        // lambda 1.0 ignores diversity -> pure relevance order.
        assert_eq!(mmr_select(&items, 1.0, 2), vec![0, 1]);
    }

    #[test]
    fn mmr_select_edge_cases() {
        assert!(mmr_select(&[], 0.7, 5).is_empty());
        let one = vec![(1.0f32, vec![1.0])];
        assert!(mmr_select(&one, 0.7, 0).is_empty());
        assert_eq!(mmr_select(&one, 0.7, 5), vec![0]); // k > n clamps to n
    }

    fn capture(text: &str) -> Event {
        Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
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

    /// Build a HybridRetriever wired to fresh stores under `home`. Returns
    /// `None` if the BGE-small ONNX weights are not available so tests that
    /// need a real embedder can skip cleanly on offline CI.
    async fn setup(home: &std::path::Path) -> Option<HybridRetriever> {
        let dir = test_assets::ensure_model()?;
        let embedder = Embedder::load(&dir).expect("load BGE-small");
        let vectors = VectorStore::open(home, EMBEDDING_DIM)
            .await
            .expect("open vector store");
        let lexical = LexicalIndex::open(home).expect("open lexical index");
        let facts = FactsStore::open(home).expect("open facts store");
        Some(HybridRetriever::new(embedder, vectors, lexical, facts))
    }

    /// Index a single capture event through every store the retriever
    /// touches. We deliberately use the underlying stores rather than the
    /// `Indexer` to avoid a circular setup with `process_capture_facts`.
    async fn index_capture(retriever: &mut HybridRetriever, ev: &Event) {
        let payload = match &ev.kind {
            EventKind::Capture(p) => p,
            _ => panic!("test helper expects a capture event"),
        };
        // T-60 interior-mutability refactor: embedder lives behind a
        // Mutex so production search() can be &self. Test helpers
        // unlock it here to mirror what the production path does.
        let vec = {
            let mut emb = retriever.embedder.lock().await;
            emb.as_mut()
                .expect("test embedder present")
                .embed(&payload.text)
                .expect("embed test capture")
        };
        // §2.8 cohesion: the vector row carries the capture's OWN tags, so the
        // vec path filters (scope/reserved) on its own data exactly like prod.
        let tags_json = serde_json::to_string(&payload.tags).unwrap_or_else(|_| "{}".into());
        retriever
            .vectors
            .as_ref()
            .as_ref()
            .expect("test vector store present")
            .add(&ev.id.to_string(), &vec, &payload.text, &tags_json, ev.ts)
            .await
            .expect("add vector row");
        // lexical is also Mutex-wrapped post-T-60; unlock to mutate.
        let mut lex = retriever.lexical.lock().await;
        lex.index_event(ev).expect("index capture in tantivy");
        lex.commit().expect("commit lexical writer");
    }

    fn make_fact(source_event: EventId, valid_from: chrono::DateTime<Utc>) -> Fact {
        Fact {
            id: EventId::new(),
            subject: "user".into(),
            predicate: "prefers".into(),
            object: "rust".into(),
            confidence: 0.7,
            valid_from,
            valid_to: None,
            recorded_at: valid_from,
            retired_at: None,
            source_events: vec![source_event],
            policy_id: None,
            kind: Default::default(),
            tags: Default::default(),
        }
    }

    fn from_event_payload(payload: &FactPayload, id: EventId) -> Fact {
        Fact {
            id,
            subject: payload.subject.clone(),
            predicate: payload.predicate.clone(),
            object: payload.object.clone(),
            confidence: payload.confidence,
            valid_from: payload.valid_from,
            valid_to: payload.valid_to,
            recorded_at: payload.valid_from,
            retired_at: None,
            source_events: payload.derived_from.clone(),
            tags: Default::default(),
            policy_id: None,
            kind: Default::default(),
        }
    }

    #[tokio::test]
    async fn empty_query_returns_empty() {
        // k=0 short-circuit must not even touch the embedder, so this test
        // can run without the model on disk. We still need to construct a
        // retriever, which requires the embedder, so gate on availability.
        let tmp = tempdir().unwrap();
        let Some(r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        let out = r
            .search("anything", 0, None, &Filters::default())
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn lex_only_hit_surfaces() {
        // A capture matched by BM25 (exact-term recall) that the embedder
        // would not surface in vector top-N must still appear in hybrid
        // output via the lex path.
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };

        // The keyword-rich target sits among unrelated noise.
        let target = capture("ULID 01HXY-DEADBEEF identifies the rogue request");
        index_capture(&mut r, &target).await;
        for i in 0..10 {
            let noise = capture(&format!("note {i}: thoughts on cats, dogs, and weather"));
            index_capture(&mut r, &noise).await;
        }

        let hits = r
            .search("01HXY-DEADBEEF", 5, None, &Filters::default())
            .await
            .unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        assert_eq!(hits[0].event_id, target.id.to_string());
        // The lex retriever must be credited on the merged hit.
        assert!(hits[0].sources.contains(&source::LEX));
    }

    #[tokio::test]
    async fn vec_only_hit_surfaces() {
        // A semantic paraphrase that BM25 would miss must still rank because
        // the embedder pulls it in.
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };

        // Target is a paraphrase: shares no rare lexical terms with the
        // query "what foods do I avoid".
        let target = capture("I never eat shellfish because of an allergy.");
        index_capture(&mut r, &target).await;
        for i in 0..10 {
            let noise = capture(&format!(
                "build {i}: cargo build --release succeeded at 02:31"
            ));
            index_capture(&mut r, &noise).await;
        }

        let hits = r
            .search("which foods do I avoid", 5, None, &Filters::default())
            .await
            .unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        // Target should be in the top hits. The vec retriever must be on
        // the sources list for it.
        let target_hit = hits
            .iter()
            .find(|h| h.event_id == target.id.to_string())
            .expect("target must surface via vector path");
        assert!(target_hit.sources.contains(&source::VEC));
    }

    #[tokio::test]
    async fn both_retrievers_rank_higher_than_either_alone() {
        // A hit that scores in both retrievers' top-N must rank above a hit
        // that scores in only one. RRF's whole point is to surface
        // multi-retriever consensus.
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };

        // both_hit matches the query both lexically (rare ULID) and
        // semantically (talks about Rust).
        let both = capture("ULID 01HXY-BOTH refers to my preference for functional Rust.");
        // lex_only matches only the lexical ULID exactly but is otherwise
        // semantically unrelated.
        let lex_only = capture("ULID 01HXY-LEXONLY is from a totally unrelated stripe webhook.");
        // vec_only is semantically about Rust but shares no rare term with
        // the query's ULID.
        let vec_only = capture("Functional Rust avoids macros where possible.");
        index_capture(&mut r, &both).await;
        index_capture(&mut r, &lex_only).await;
        index_capture(&mut r, &vec_only).await;

        // The query carries both signals: rare ULID for the lex path and
        // semantic content for the vec path.
        let hits = r
            .search("01HXY-BOTH functional Rust", 5, None, &Filters::default())
            .await
            .unwrap();
        assert!(!hits.is_empty());
        // both_hit must be the top hit.
        assert_eq!(hits[0].event_id, both.id.to_string());
        // And it must be credited to both retrievers.
        assert!(hits[0].sources.contains(&source::LEX));
        assert!(hits[0].sources.contains(&source::VEC));
    }

    #[tokio::test]
    async fn temporal_filter_drops_retired_fact() {
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };

        let ev = capture("I prefer Rust over Python for systems work.");
        index_capture(&mut r, &ev).await;

        // Insert a fact derived from this capture, retired yesterday.
        let yesterday = Utc::now() - chrono::Duration::days(1);
        let mut fact = make_fact(ev.id, yesterday - chrono::Duration::days(7));
        fact.retired_at = Some(yesterday);
        {
            let facts = r.facts.lock().await;
            facts.insert(&fact).unwrap();
        }

        // No `at_time` → keep (current state is "every fact is retired" but
        // we don't filter without an explicit time).
        let now_hits = r
            .search("Rust", 5, None, &Filters::default())
            .await
            .unwrap();
        assert!(
            now_hits.iter().any(|h| h.event_id == ev.id.to_string()),
            "no temporal filter should keep the hit"
        );

        // at_time = now → the only derived fact is retired, drop the hit.
        let filtered = r
            .search("Rust", 5, Some(Utc::now()), &Filters::default())
            .await
            .unwrap();
        assert!(
            !filtered.iter().any(|h| h.event_id == ev.id.to_string()),
            "retired fact should hide the capture under at_time=now"
        );
    }

    #[tokio::test]
    async fn temporal_filter_keeps_capture_without_facts() {
        // A raw capture the extractor never produced a fact for must pass
        // the temporal filter (the "no row" case in is_event_valid_at).
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };

        let ev = capture("Just a passing thought, no extractable fact here.");
        index_capture(&mut r, &ev).await;

        let hits = r
            .search("passing thought", 5, Some(Utc::now()), &Filters::default())
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.event_id == ev.id.to_string()),
            "capture without derived facts must survive temporal filter"
        );
    }

    #[tokio::test]
    async fn k_limits_output_size() {
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };

        for i in 0..20 {
            let ev = capture(&format!("rust note {i} on lifetimes and ownership"));
            index_capture(&mut r, &ev).await;
        }
        let hits = r
            .search("rust note", 7, None, &Filters::default())
            .await
            .unwrap();
        assert!(
            hits.len() <= 7,
            "expected at most 7 hits, got {}",
            hits.len()
        );
    }

    #[tokio::test]
    async fn same_event_id_does_not_dedupe_to_zero_score() {
        // The merge accumulates RRF scores; an event appearing in both
        // retrievers must have a strictly higher score than the same event
        // appearing in only one. Concretely: rank-0 in both > rank-0 in one.
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        // T-57: pin recency weight to 0 so this test asserts the pure
        // RRF math. A separate test covers the recency-biased path.
        r = r.with_recency_weight(0.0);

        // Single capture that the query will pin to rank 0 in both retrievers.
        let ev = capture("ULID 01HXY-DUAL identifies my functional Rust preference.");
        index_capture(&mut r, &ev).await;
        let hits = r
            .search("01HXY-DUAL functional Rust", 1, None, &Filters::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        // Score for rank-0 in two retrievers: 2 * 1 / (RRF_K + 1).
        let expected = 2.0 * rrf_score(0);
        assert!(
            (hits[0].score - expected).abs() < 1e-6,
            "expected dual-retriever rank-0 score ~ {expected}, got {}",
            hits[0].score
        );
        assert_eq!(hits[0].sources.len(), 2);
    }

    // T-51: tag filter applied across both retrieval paths.

    fn capture_with_tags(text: &str, pairs: &[(&str, &str)]) -> Event {
        let mut tags = BTreeMap::new();
        for (k, v) in pairs {
            tags.insert((*k).to_string(), (*v).to_string());
        }
        Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
                text: text.into(),
                rewritten_text: None,
                kind: Default::default(),
                mime: None,
                attachments: vec![],
                tags,
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        )
    }

    #[tokio::test]
    async fn hybrid_filter_drops_non_matching_captures_from_both_paths() {
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };

        // Three captures sharing the search terms but tagged differently.
        // Only `lm` should survive the filter. The vec retriever should
        // still see all three (they're semantically similar) but the
        // post-filter on lex tags must drop the two non-matching ones.
        let lm = capture_with_tags(
            "I prefer functional Rust patterns for systems work",
            &[("project", "localmem")],
        );
        let other = capture_with_tags(
            "I prefer functional Rust patterns for systems work",
            &[("project", "other")],
        );
        let untagged = capture("I prefer functional Rust patterns for systems work");
        index_capture(&mut r, &lm).await;
        index_capture(&mut r, &other).await;
        index_capture(&mut r, &untagged).await;

        let filter = Filters::with_tags(BTreeMap::from([("project".into(), "localmem".into())]));
        let hits = r
            .search("functional Rust", 10, None, &filter)
            .await
            .unwrap();
        let ids: Vec<String> = hits.iter().map(|h| h.event_id.clone()).collect();
        assert_eq!(ids, vec![lm.id.to_string()], "filter must keep only lm");
    }

    #[tokio::test]
    async fn hybrid_empty_filter_returns_all_matches() {
        // An empty Filters must behave like no-filter: every capture the
        // retrievers surface should pass through unchanged.
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        let a = capture_with_tags("rust async note", &[("project", "lm")]);
        let b = capture("rust async note");
        index_capture(&mut r, &a).await;
        index_capture(&mut r, &b).await;

        let hits = r
            .search("rust async", 10, None, &Filters::default())
            .await
            .unwrap();
        assert!(hits.len() >= 2, "expected both captures, got {hits:?}");
    }

    // Subset-match semantics are covered exhaustively in `tag_match.rs`.
    // We don't re-test the matcher here; the integration tests above
    // already exercise it indirectly through the lex and vec paths.

    // ---- T-51c: reserved-tag visibility / retention in hybrid ----------

    #[tokio::test]
    async fn private_capture_is_hidden_under_default_visibility() {
        // visibility=private must drop the hit from a default hybrid
        // search. The capture is otherwise relevant (matches both lex
        // and vec). With Visibility::IncludePrivate it surfaces again,
        // proving the filter is the *only* thing hiding it.
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        let priv_cap = capture_with_tags(
            "I prefer functional Rust for systems work",
            &[("visibility", "private")],
        );
        index_capture(&mut r, &priv_cap).await;

        // Default Filters → exclude private.
        let default_filter = Filters::default();
        let default_hits = r
            .search("functional Rust", 10, None, &default_filter)
            .await
            .unwrap();
        assert!(
            !default_hits
                .iter()
                .any(|h| h.event_id == priv_cap.id.to_string()),
            "private capture must NOT surface under default visibility"
        );

        // IncludePrivate Filters → surface it.
        let audit_filter = Filters {
            visibility: crate::reserved_tags::Visibility::IncludePrivate,
            ..Filters::default()
        };
        let audit_hits = r
            .search("functional Rust", 10, None, &audit_filter)
            .await
            .unwrap();
        assert!(
            audit_hits
                .iter()
                .any(|h| h.event_id == priv_cap.id.to_string()),
            "private capture must surface under IncludePrivate"
        );
    }

    #[tokio::test]
    async fn ephemeral_capture_expires_at_query_time() {
        // A capture tagged retention=ephemeral:1h surfaces while the
        // query's `now` is within the TTL, then disappears when `now`
        // moves past the TTL. We control `now` via Filters so the test
        // is deterministic; production code passes Utc::now().
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        let mut eph = capture_with_tags(
            "ephemeral note about functional Rust patterns",
            &[("retention", "ephemeral:1h")],
        );
        // Pin the capture ts to a known instant so the math is obvious.
        eph.ts = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        index_capture(&mut r, &eph).await;

        // Within TTL: still surfaces.
        let inside = Filters {
            now: eph.ts + chrono::Duration::minutes(30),
            ..Filters::default()
        };
        let inside_hits = r.search("ephemeral note", 10, None, &inside).await.unwrap();
        assert!(
            inside_hits.iter().any(|h| h.event_id == eph.id.to_string()),
            "ephemeral capture must surface within its TTL"
        );

        // Past TTL: dropped.
        let after = Filters {
            now: eph.ts + chrono::Duration::hours(2),
            ..Filters::default()
        };
        let after_hits = r.search("ephemeral note", 10, None, &after).await.unwrap();
        assert!(
            !after_hits.iter().any(|h| h.event_id == eph.id.to_string()),
            "ephemeral capture must drop after its TTL"
        );
    }

    // Lightweight sanity tests that do not need the BGE model.

    #[test]
    fn rrf_score_decreases_with_rank() {
        let s0 = rrf_score(0);
        let s1 = rrf_score(1);
        let s2 = rrf_score(2);
        assert!(s0 > s1 && s1 > s2, "RRF must decrease monotonically");
        // Sanity: rank-0 is 1 / (60 + 1) ≈ 0.0163934.
        assert!((s0 - 1.0 / 61.0).abs() < 1e-9);
    }

    #[test]
    fn fact_payload_helper_compiles() {
        // Touch the unused helper so `cargo clippy --all-targets` does not
        // flag it when the heavy integration tests are skipped on a host
        // without network access to HuggingFace.
        let payload = FactPayload {
            subject: "user".into(),
            predicate: "prefers".into(),
            object: "rust".into(),
            confidence: 0.5,
            valid_from: Utc::now(),
            valid_to: None,
            derived_from: vec![EventId::new()],
            kind: Default::default(),
            tags: Default::default(),
            extra: Map::new(),
        };
        let f = from_event_payload(&payload, EventId::new());
        assert_eq!(f.subject, "user");
    }

    // ---- T-57: recency bias ------------------------------------------

    #[test]
    fn recency_bonus_is_zero_when_weight_is_zero() {
        let now = Utc::now();
        let ts = now - chrono::Duration::days(1);
        assert_eq!(apply_recency_bonus(0.5, ts, now, 0.0), 0.5);
    }

    #[test]
    fn recency_bonus_decays_with_age() {
        // Fresh capture: full weight bonus.
        let now = Utc::now();
        let fresh = apply_recency_bonus(0.0, now, now, 0.10);
        // Old capture (30 days): 1/e of the bonus.
        let old = apply_recency_bonus(0.0, now - chrono::Duration::days(30), now, 0.10);
        // Older capture (90 days): 1/e^3.
        let older = apply_recency_bonus(0.0, now - chrono::Duration::days(90), now, 0.10);
        assert!(fresh > old);
        assert!(old > older);
        // Fresh ≈ 0.10; check within float tolerance.
        assert!((fresh - 0.10).abs() < 1e-6, "fresh = {fresh}");
        // 30 days = 1/e.
        let expected_30 = 0.10 * std::f32::consts::E.recip();
        assert!(
            (old - expected_30).abs() < 1e-6,
            "old = {old}, expected ≈ {expected_30}"
        );
    }

    #[test]
    fn recency_bonus_clamps_future_timestamps_at_zero_age() {
        // Clock skew: a capture's ts is slightly in the future. We must
        // not return a NEGATIVE age, which would amplify the bonus
        // exponentially. The clamp keeps the bonus at most `weight`.
        let now = Utc::now();
        let future_ts = now + chrono::Duration::hours(1);
        let bonus = apply_recency_bonus(0.0, future_ts, now, 0.10);
        assert!(
            (bonus - 0.10).abs() < 1e-6,
            "future ts must not amplify bonus"
        );
    }

    // ---- T-73: per-kind half-life ----------------------------------------

    #[test]
    fn kind_aware_decay_with_half_life_matches_half_at_age_equal_h() {
        // 0.5^(H/H) == 0.5 → bonus is exactly half of weight at age=H.
        let now = Utc::now();
        let ts = now - chrono::Duration::days(30);
        let bonus = apply_recency_bonus_kind(0.0, ts, now, 0.10, Some(30.0));
        assert!(
            (bonus - 0.05).abs() < 1e-6,
            "expected ~0.05 at age=H, got {bonus}"
        );
    }

    #[test]
    fn kind_aware_decay_falls_back_to_uniform_when_half_life_is_none() {
        // No half-life entry: falls back to `exp(-age/30)`. Same as
        // the legacy uniform helper.
        let now = Utc::now();
        let ts = now - chrono::Duration::days(45);
        let kind_bonus = apply_recency_bonus_kind(0.0, ts, now, 0.10, None);
        let uniform_bonus = apply_recency_bonus(0.0, ts, now, 0.10);
        assert!(
            (kind_bonus - uniform_bonus).abs() < 1e-6,
            "fallback path must agree with apply_recency_bonus"
        );
    }

    #[test]
    fn sixty_day_old_todo_decays_faster_than_sixty_day_old_decision() {
        // T-73 acceptance criterion: with the default half-lives
        // (todo=14d, decision=365d), a 60-day-old todo's bonus is
        // dramatically smaller than a 60-day-old decision's bonus.
        let now = Utc::now();
        let ts = now - chrono::Duration::days(60);
        let todo_bonus = apply_recency_bonus_kind(0.0, ts, now, 1.0, Some(14.0));
        let decision_bonus = apply_recency_bonus_kind(0.0, ts, now, 1.0, Some(365.0));
        assert!(
            decision_bonus > todo_bonus,
            "decision bonus {decision_bonus} must exceed todo bonus {todo_bonus} at age=60d"
        );
        // Decisions retain most of their bonus: 0.5^(60/365) ≈ 0.892.
        assert!(
            decision_bonus > 0.85,
            "60d decision should keep >85% of its bonus, got {decision_bonus}"
        );
        // Todos lose nearly all of it: 0.5^(60/14) ≈ 0.051.
        assert!(
            todo_bonus < 0.10,
            "60d todo should keep <10% of its bonus, got {todo_bonus}"
        );
    }

    #[test]
    fn kind_aware_decay_zero_weight_short_circuits() {
        let now = Utc::now();
        let ts = now - chrono::Duration::days(30);
        let out = apply_recency_bonus_kind(0.5, ts, now, 0.0, Some(14.0));
        assert_eq!(out, 0.5, "weight=0 must short-circuit");
    }

    #[tokio::test]
    async fn recent_capture_ranks_above_older_one_with_recency_bias() {
        // End-to-end: two captures with identical content (so RRF
        // ranks them by their natural insertion order from each
        // retriever). The newer capture must rank above the older
        // one once the recency bias is applied.
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        // The default recency weight is non-zero, so a fresh `setup`
        // already exercises the bias. We just need to seed two
        // captures with different ts values.
        let old_ts = chrono::DateTime::<chrono::Utc>::from_timestamp(1_600_000_000, 0).unwrap();
        let new_ts = chrono::DateTime::<chrono::Utc>::from_timestamp(1_800_000_000, 0).unwrap();
        let mut old = capture("I prefer functional Rust and avoid macros");
        old.ts = old_ts;
        let mut new = capture("I prefer functional Rust and avoid macros");
        new.ts = new_ts;
        index_capture(&mut r, &old).await;
        index_capture(&mut r, &new).await;
        let filters = Filters {
            now: new_ts + chrono::Duration::days(1),
            ..Filters::default()
        };
        let hits = r
            .search("functional Rust", 10, None, &filters)
            .await
            .unwrap();
        // Both should surface. The newer should rank first when
        // recency bias breaks the tie.
        let new_pos = hits.iter().position(|h| h.event_id == new.id.to_string());
        let old_pos = hits.iter().position(|h| h.event_id == old.id.to_string());
        assert!(
            new_pos.is_some() && old_pos.is_some(),
            "both captures should appear in hits"
        );
        assert!(
            new_pos.unwrap() < old_pos.unwrap(),
            "recency bias should rank the newer capture above the older one. hits={:?}",
            hits.iter()
                .map(|h| (h.event_id.clone(), h.score))
                .collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn zero_recency_weight_disables_the_bias() {
        // Toggle the weight 0 → nonzero on the SAME corpus and assert
        // each capture's score gains the expected `weight * exp(...)`
        // delta. Avoids comparing two captures' scores directly: when
        // they share identical content, BM25/ANN tie-breaking for
        // their RRF rank is implementation-defined and not stable
        // across test orderings — an earlier form of this test
        // asserted equality and flaked under suite-wide pressure.
        //
        // The new shape isolates ONE behaviour (bias on vs off on the
        // same corpus) which is what the contract actually promises.
        let tmp = tempdir().unwrap();
        let Some(mut r) = setup(tmp.path()).await else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        // Two captures, deliberately different content so each has a
        // stable lex+vec rank. Ages set explicitly.
        let new_ts = chrono::DateTime::<chrono::Utc>::from_timestamp(1_800_000_000, 0).unwrap();
        let old_ts = new_ts - chrono::Duration::days(60);
        let mut fresh = capture("I prefer functional Rust and avoid macros");
        fresh.ts = new_ts;
        let mut stale =
            capture("Vim handles split panes with C-w and avoids macros for indentation");
        stale.ts = old_ts;
        index_capture(&mut r, &fresh).await;
        index_capture(&mut r, &stale).await;
        let filters = Filters {
            now: new_ts + chrono::Duration::days(1),
            ..Filters::default()
        };

        // Pass 1: bias OFF. Record each hit's score.
        r = r.with_recency_weight(0.0);
        let hits_off = r.search("avoid macros", 10, None, &filters).await.unwrap();
        assert!(
            hits_off.iter().any(|h| h.event_id == fresh.id.to_string()),
            "fresh capture must surface",
        );
        assert!(
            hits_off.iter().any(|h| h.event_id == stale.id.to_string()),
            "stale capture must surface",
        );
        let fresh_off = hits_off
            .iter()
            .find(|h| h.event_id == fresh.id.to_string())
            .unwrap()
            .score;
        let stale_off = hits_off
            .iter()
            .find(|h| h.event_id == stale.id.to_string())
            .unwrap()
            .score;

        // Pass 2: bias ON. Same retriever, just flip the knob.
        let weight = 0.10f32;
        r = r.with_recency_weight(weight);
        let hits_on = r.search("avoid macros", 10, None, &filters).await.unwrap();
        let fresh_on = hits_on
            .iter()
            .find(|h| h.event_id == fresh.id.to_string())
            .unwrap()
            .score;
        let stale_on = hits_on
            .iter()
            .find(|h| h.event_id == stale.id.to_string())
            .unwrap()
            .score;

        // Each capture's delta == apply_recency_bonus(0, ts, now, w).
        // We compute the expected bonus and compare. Avoids any
        // dependency on the underlying RRF score for either capture.
        let now = filters.now;
        let expected_fresh_delta = apply_recency_bonus(0.0, fresh.ts, now, weight);
        let expected_stale_delta = apply_recency_bonus(0.0, stale.ts, now, weight);
        let actual_fresh_delta = fresh_on - fresh_off;
        let actual_stale_delta = stale_on - stale_off;
        assert!(
            (actual_fresh_delta - expected_fresh_delta).abs() < 1e-5,
            "fresh delta: actual {actual_fresh_delta}, expected {expected_fresh_delta}",
        );
        assert!(
            (actual_stale_delta - expected_stale_delta).abs() < 1e-5,
            "stale delta: actual {actual_stale_delta}, expected {expected_stale_delta}",
        );
        // Sanity: the fresh capture's bonus is ALSO larger than the
        // stale one's, because it's younger.
        assert!(
            actual_fresh_delta > actual_stale_delta,
            "younger capture must gain more bonus. fresh={actual_fresh_delta} stale={actual_stale_delta}",
        );
    }
}
