//! HTTP route handlers.
//!
//! One handler per MCP tool, plus /health and /version for liveness. See
//! SPEC.md ("MCP tool surface") for the exact request/response shape every
//! endpoint conforms to.
//!
//! T-43 (this commit) finishes /write so it runs the full pipeline
//! (policy + journal + lex + vec + facts). T-44 (this commit) wires
//! /recall, /profile, /forget, /journal to their real underlying stores
//! via the new `AppState` handles. /search routes through the hybrid
//! retriever when the embedder is loaded, falling back to lex-only
//! otherwise.

use crate::event::{
    CapturePayload, Event, EventKind, FactPayload, ForgetPayload, PolicyAction, Source,
};
use crate::event_id::EventId;
use crate::facts::Fact;
use crate::journal::JournalEntry as DerivedJournalEntry;
use crate::policy::EvalContext;
use crate::server::AppState;
use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{error, info};

// ---------------------------------------------------------------------------
// Response envelope helpers
// ---------------------------------------------------------------------------

/// Error response body. Matches SPEC.md:
/// `{ ok: false, error: { code, message } }`.
#[derive(Debug, Serialize)]
struct ErrorBody {
    ok: bool,
    error: ErrorPayload,
}

#[derive(Debug, Serialize)]
struct ErrorPayload {
    code: String,
    message: String,
}

/// Uniform error type for handler results. Carries an HTTP status, a stable
/// machine-readable `code`, and a human message.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: message.into(),
        }
    }

    pub fn internal(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            ok: false,
            error: ErrorPayload {
                code: self.code.to_string(),
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

/// Wrap any anyhow failure as a 500 with the supplied stable code. The error
/// message is included verbatim. Per SPEC.md, error messages never contain
/// user content; handlers must only pass infrastructure errors here.
fn internal(code: &'static str, ctx: &'static str) -> impl FnOnce(anyhow::Error) -> ApiError {
    move |e| {
        error!(error = ?e, ctx = %ctx, "internal error");
        ApiError::internal(code, format!("{e:#}"))
    }
}

// ---------------------------------------------------------------------------
// /health and /version (T-19)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct HealthBody {
    pub ok: bool,
    pub status: &'static str,
}

pub async fn health() -> Json<HealthBody> {
    Json(HealthBody {
        ok: true,
        status: "healthy",
    })
}

#[derive(Debug, Serialize)]
pub struct VersionBody {
    pub ok: bool,
    pub version: &'static str,
}

pub async fn version() -> Json<VersionBody> {
    Json(VersionBody {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
    })
}

// ---------------------------------------------------------------------------
// /write (T-20 + T-43)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct WriteRequest {
    pub content: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    /// Container tags (v0.2). Optional; absent or empty preserves v0.1
    /// shape on the wire.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct WriteResponse {
    pub ok: bool,
    pub event_id: String,
    pub action: &'static str,
    pub facts_extracted: u32,
    /// T-55: surfaced only when the rewriter produced a different
    /// version. Absent otherwise so the JSON shape stays
    /// indistinguishable from v0.1 for non-rewriting writes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewritten_text: Option<String>,
}

const POLICY_RECENT_WINDOW: usize = 100;

pub async fn write(
    State(state): State<Arc<AppState>>,
    Json(req): Json<WriteRequest>,
) -> Result<Json<WriteResponse>, ApiError> {
    if req.content.trim().is_empty() {
        return Err(ApiError::bad_request(
            "empty_content",
            "content must not be empty",
        ));
    }

    // T-52: `kind` in the MCP request now maps to a typed
    // [`crate::kind::Kind`] field on the capture instead of being
    // stuffed into `extra`. Unknown strings round-trip as
    // `Kind::Other(_)` so a forward-looking client can send a
    // future kind without breaking us. Absent → defaults to
    // `Kind::Note`.
    let kind = req.kind.map(crate::kind::Kind::from).unwrap_or_default();
    let extra = Map::new();

    // T-55: apply the configured rewriter before persisting. The
    // server reads the config fresh per write so a `[rewriter].mode`
    // change picked up by `localmem serve` reload (or a process
    // restart) takes effect without code changes.
    let cfg = crate::config::Config::load(&state.home)
        .map_err(internal("config_load_failed", "load config for rewriter"))?;
    let rewritten_text = apply_server_rewriter(&cfg, &req.content);

    let payload = CapturePayload {
        text: req.content,
        kind,
        rewritten_text,
        mime: None,
        attachments: vec![],
        tags: req.tags,
        extra,
    };
    let source = Source {
        app: req.source.unwrap_or_else(|| "mcp".into()),
        host: hostname(),
        user: None,
    };
    let capture = Event::new(EventKind::Capture(payload), source);
    let event_id = capture.id.to_string();

    state
        .event_log
        .append(&capture)
        .map_err(internal("append_failed", "append capture to event log"))?;

    // Policy decision.
    let recent = load_recent_events(&state, POLICY_RECENT_WINDOW, capture.id)
        .map_err(internal("policy_context_failed", "read recent events"))?;
    let decision = state
        .policy
        .evaluate(&capture, &EvalContext { recent: &recent })
        .map_err(internal("policy_evaluate_failed", "evaluate policy"))?;
    state
        .journal
        .append(&DerivedJournalEntry::from_decision(
            &decision, capture.id, capture.ts,
        ))
        .map_err(internal("journal_append_failed", "append journal entry"))?;

    let action_label: &'static str = match decision.action {
        PolicyAction::Commit => "COMMIT",
        PolicyAction::Update => "UPDATE",
        PolicyAction::Dedup => "DEDUP",
        PolicyAction::Skip => "SKIP",
        PolicyAction::Forget => "FORGET",
    };

    let mut facts_count = 0u32;
    if decision.action == PolicyAction::Commit {
        // Lexical index: always update on commit.
        {
            let mut lex = state.lexical.lock().await;
            lex.index_event(&capture)
                .map_err(internal("index_failed", "index capture in lexical store"))?;
            lex.commit()
                .map_err(internal("commit_failed", "commit lexical writer"))?;
        }

        // Vector store: only when embedder + vectors are available.
        // T-55: embed the indexable text (rewritten when present,
        // else original) so vec hits + lex hits return the same
        // snippet text.
        if let (Some(vectors), payload) =
            (state.vectors.as_ref().as_ref(), &capture_payload(&capture))
        {
            let to_index = payload.indexable_text();
            let mut embedder_guard = state.embedder.lock().await;
            if let Some(emb) = embedder_guard.as_mut() {
                let v = emb
                    .embed(to_index)
                    .map_err(internal("embed_failed", "embed capture"))?;
                vectors
                    .add(&capture.id.to_string(), &v, to_index, capture.ts)
                    .await
                    .map_err(internal("vector_add_failed", "write embedding"))?;
            }
        }

        // Fact extraction: always run on commit. Each extracted
        // fact runs through T-56 contradiction resolution; if a
        // prior live fact with the same (subject, predicate) gets
        // retired, we emit an `Update` event (carrying the new
        // payload) instead of a plain `fact` event so `replay`
        // rebuilds state correctly. See the CLI write pipeline for
        // the mirror; the logic is duplicated rather than shared
        // because the server holds its handles behind Arc/Mutex
        // and lifting it into a shared module would force the CLI
        // path through that synchronisation overhead.
        let payload = capture_payload(&capture);
        // T-58: registry runs every configured extractor in parallel
        // and dedups by (subject, predicate, object). The kind hint
        // lets future LLM extractors bias their prompt to the
        // capture's declared kind.
        let extracted = state
            .extractor
            .extract(&payload.text, Some(&payload.kind))
            .await
            .map_err(internal("extractor_failed", "registry extract"))?;
        for ef in &extracted {
            let new_payload = server_build_fact_payload(&capture, ef);
            let new_event_id = crate::event_id::EventId::new();
            let new_fact = Fact::from_event(
                new_event_id,
                &new_payload,
                Utc::now(),
                Some(decision.rule_id.clone()),
            );

            // T-56: check contradictions against prior live facts.
            // The DB UPDATE happens inside resolve_contradiction; we
            // only need to emit the matching `Update` event so
            // replay rebuilds the same state.
            let retired_ids = {
                let facts_guard = state.facts.lock().await;
                facts_guard
                    .resolve_contradiction(&new_fact)
                    .map_err(internal("smart_forgetting_failed", "resolve_contradiction"))?
            };

            let source = server_fact_source(&capture);
            let event = if let Some(supersedes_id) = retired_ids.first().copied() {
                Event::with_id(
                    new_event_id,
                    EventKind::Update(crate::event::UpdatePayload {
                        supersedes_id,
                        new_fact: new_payload.clone(),
                        extra: Map::new(),
                    }),
                    source,
                )
            } else {
                Event::with_id(new_event_id, EventKind::Fact(new_payload.clone()), source)
            };
            state.event_log.append(&event).map_err(internal(
                "append_failed",
                "append derived fact / update event",
            ))?;
            {
                let facts_guard = state.facts.lock().await;
                facts_guard
                    .insert(&new_fact)
                    .map_err(internal("facts_insert_failed", "insert fact row"))?;
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
                state
                    .journal
                    .append(&entry)
                    .map_err(internal("journal_append_failed", "journal contradiction"))?;
            }

            facts_count += 1;
        }
    }

    info!(event_id = %event_id, action = action_label, "wrote capture");

    // Pull the rewritten text from the persisted capture (rather than
    // the pre-rewrite variable above) so the response and the event
    // log stay in sync even if the wiring drifts.
    let rewritten_text = match &capture.kind {
        EventKind::Capture(p) => p.rewritten_text.clone(),
        _ => None,
    };

    Ok(Json(WriteResponse {
        ok: true,
        event_id,
        action: action_label,
        facts_extracted: facts_count,
        rewritten_text,
    }))
}

/// T-55 mirror of the CLI rewriter helper. Same fallback discipline:
/// invalid mode or failed call logs a warning and degrades to "no
/// rewrite" rather than dropping the user's write. The CLI keeps
/// its own copy because pulling this into a shared module would
/// drag `tracing` into the rewriter crate, which we want to stay
/// dependency-light. The duplication is small (12 lines) and easy
/// to audit.
fn apply_server_rewriter(cfg: &crate::config::Config, text: &str) -> Option<String> {
    let rewriter = match crate::rewriter::build(&cfg.rewriter.mode) {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, mode = %cfg.rewriter.mode,
                   "rewriter config invalid; falling back to no-rewrite");
            return None;
        }
    };
    let user_name = crate::rewriter::resolve_user_name(&cfg.home.user_name);
    match rewriter.rewrite(text, &user_name) {
        Ok(out) if out != text => Some(out),
        Ok(_) => None,
        Err(e) => {
            error!(error = %e, mode = %cfg.rewriter.mode,
                   "rewriter call failed; falling back to no-rewrite");
            None
        }
    }
}

fn capture_payload(event: &Event) -> &CapturePayload {
    match &event.kind {
        EventKind::Capture(p) => p,
        _ => panic!("capture_payload invoked on non-capture event"),
    }
}

/// Build the `FactPayload` for a single extractor hit. Server-side
/// mirror of [`crate::cli::write::build_fact_payload`]; same
/// kind + tag inheritance, kept as its own copy here because the
/// CLI helper is private to its module.
fn server_build_fact_payload(capture: &Event, ef: &crate::extractor::ExtractedFact) -> FactPayload {
    // T-51b: tags / T-52: kind both inherit from the source
    // capture, so the facts table can be filtered + grouped without
    // a join back to events.jsonl.
    let (inherited_tags, inherited_kind) = match &capture.kind {
        EventKind::Capture(p) => (p.tags.clone(), p.kind.clone()),
        _ => (BTreeMap::new(), crate::kind::Kind::default()),
    };
    FactPayload {
        subject: ef.subject.clone(),
        predicate: ef.predicate.clone(),
        object: ef.object.clone(),
        confidence: ef.confidence,
        valid_from: capture.ts,
        valid_to: None,
        derived_from: vec![capture.id],
        kind: inherited_kind,
        tags: inherited_tags,
        extra: Map::new(),
    }
}

/// Source for a derived fact/update event: inherits the capture's
/// app/host/user so the audit trail surfaces which tool led to the
/// fact even after T-56 retires it via an `Update`.
fn server_fact_source(capture: &Event) -> Source {
    Source {
        app: capture.source.app.clone(),
        host: capture.source.host.clone(),
        user: capture.source.user.clone(),
    }
}

fn load_recent_events(state: &AppState, n: usize, candidate: EventId) -> Result<Vec<Event>> {
    let mut buf: Vec<Event> = Vec::with_capacity(n.saturating_add(1));
    for ev in state.event_log.iter()? {
        let ev = ev?;
        if ev.id == candidate {
            continue;
        }
        if buf.len() == n {
            buf.remove(0);
        }
        buf.push(ev);
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------
// /search (T-21 + hybrid)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub k: Option<usize>,
    #[serde(default)]
    pub at_time: Option<String>,
    /// Container tag filter (T-51). Subset match on capture tags.
    /// Empty or absent skips filtering, preserving v0.1 shape.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub ok: bool,
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub fact: String,
    pub score: f32,
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
}

/// Default `k` for `/search`. SPEC.md: "Default k: 10. Max 100."
const SEARCH_K_DEFAULT: usize = 10;
const SEARCH_K_MAX: usize = 100;

pub async fn search(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let query = req.query.trim();
    if query.is_empty() {
        return Err(ApiError::bad_request(
            "empty_query",
            "query must not be empty",
        ));
    }
    let k = req.k.unwrap_or(SEARCH_K_DEFAULT).clamp(1, SEARCH_K_MAX);

    let at_time = req
        .at_time
        .as_deref()
        .map(parse_rfc3339)
        .transpose()
        .map_err(|e: anyhow::Error| {
            ApiError::bad_request("invalid_at_time", format!("at_time must be RFC3339: {e:#}"))
        })?;
    let tag_filter = if req.tags.is_empty() {
        None
    } else {
        Some(&req.tags)
    };
    // /search is not the audit path; `visibility=private` captures
    // stay hidden, and retention TTL applies (T-51c). The hybrid
    // path threads these through `hybrid_search`; the lex-only
    // fallback post-filters here since `lex.search` does not know
    // about reserved tags.
    let visibility = crate::reserved_tags::Visibility::Default;
    let now = Utc::now();

    // Hybrid path only available when embedder + vectors are both loaded.
    // Otherwise fall back to lex-only — that path matches the BM25 hits
    // shape and still satisfies SPEC.md memory_search.
    let embedder_present = state.embedder.lock().await.is_some();
    let vectors_present = state.vectors.is_some();
    if embedder_present && vectors_present {
        let hits = hybrid_search(&state, query, k, at_time, tag_filter, visibility, now).await?;
        let results = hits
            .into_iter()
            .map(|h| SearchResult {
                fact: h.content,
                score: h.score,
                sources: vec![h.event_id],
                valid_from: None,
                valid_to: None,
            })
            .collect();
        return Ok(Json(SearchResponse { ok: true, results }));
    }

    let hits = {
        let lex = state.lexical.lock().await;
        lex.search(query, k, tag_filter)
            .map_err(internal("search_failed", "lexical search"))?
    };
    let hits: Vec<crate::lexical::LexicalHit> = hits
        .into_iter()
        .filter(|h| crate::reserved_tags::is_visible(&h.tags, h.ts, now, visibility))
        .collect();
    let results = hits
        .into_iter()
        .map(|h| SearchResult {
            fact: h.snippet,
            score: h.score,
            sources: vec![h.event_id],
            valid_from: None,
            valid_to: None,
        })
        .collect();
    Ok(Json(SearchResponse { ok: true, results }))
}

/// Run a hybrid search using the server's already-open stores. The
/// retriever borrows mutable references, so we have to construct it
/// from owned handles — done by cloning the per-store `Arc`s and
/// taking the contents out of the embedder mutex briefly.
async fn hybrid_search(
    state: &AppState,
    query: &str,
    k: usize,
    at_time: Option<DateTime<Utc>>,
    tag_filter: Option<&BTreeMap<String, String>>,
    visibility: crate::reserved_tags::Visibility,
    now: DateTime<Utc>,
) -> Result<Vec<crate::retriever::HybridHit>, ApiError> {
    // The HybridRetriever wants owned stores. We have an &VectorStore via
    // `state.vectors`; LanceDB's APIs only need &self, so we can
    // re-derive a borrowed retrieval path manually rather than moving
    // ownership out of AppState.
    use crate::retriever::source as src;
    use crate::retriever::HybridHit;
    use std::collections::HashMap;

    const RRF_K: f32 = 60.0;
    const OVERFETCH: usize = 3;
    let fetch = k.saturating_mul(OVERFETCH);

    // Lexical pass — filters tags at the lex layer, then strips hits
    // that violate the reserved-tag rules (T-51c). The lex hit
    // carries `ts` and `tags`, so no second lookup is needed.
    let lex_hits = {
        let lex = state.lexical.lock().await;
        let raw = lex
            .search(query, fetch, tag_filter)
            .map_err(internal("search_failed", "lexical search"))?;
        raw.into_iter()
            .filter(|h| crate::reserved_tags::is_visible(&h.tags, h.ts, now, visibility))
            .collect::<Vec<_>>()
    };

    // Vector pass.
    let query_vec = {
        let mut emb_guard = state.embedder.lock().await;
        let emb = emb_guard
            .as_mut()
            .expect("hybrid_search called without embedder");
        emb.embed(query)
            .map_err(internal("embed_failed", "embed query"))?
    };
    let vectors = state
        .vectors
        .as_ref()
        .as_ref()
        .expect("vectors must be Some");
    let vec_hits_raw = vectors
        .search(&query_vec, fetch)
        .await
        .map_err(internal("vector_search_failed", "vector search"))?;

    // Vec hits carry no tag metadata; look up via the lex index for
    // both the tag subset filter (T-51) and the reserved-tag rules
    // (T-51c). One `meta_for` lookup per vec hit covers both checks
    // AND seeds the ts side map T-57 uses for the recency bonus.
    // T-57 + T-73: side map from event_id → (ts, kind). The kind is
    // used by the per-kind half-life lookup; legacy docs (empty kind)
    // fall back to the uniform tau path inside
    // `apply_recency_bonus_kind`.
    let (vec_hits, mut meta_by_id) = {
        let lex = state.lexical.lock().await;
        let mut keep = Vec::with_capacity(vec_hits_raw.len());
        let mut meta_by_id: HashMap<String, (DateTime<Utc>, String)> = HashMap::new();
        for vh in vec_hits_raw {
            let meta = lex
                .meta_for(&vh.event_id)
                .map_err(internal("tag_lookup_failed", "meta_for on vec hit"))?;
            if let Some(filter) = tag_filter {
                if !crate::tag_match::matches(&meta.tags, filter) {
                    continue;
                }
            }
            if !crate::reserved_tags::is_visible(&meta.tags, meta.ts, now, visibility) {
                continue;
            }
            meta_by_id.insert(vh.event_id.clone(), (meta.ts, meta.kind));
            keep.push(vh);
        }
        (keep, meta_by_id)
    };

    // RRF merge keyed by event_id.
    let mut merged: HashMap<String, HybridHit> = HashMap::new();
    for (rank, h) in lex_hits.iter().enumerate() {
        let bonus = 1.0 / (RRF_K + rank as f32 + 1.0);
        meta_by_id
            .entry(h.event_id.clone())
            .or_insert_with(|| (h.ts, h.kind.clone()));
        merged
            .entry(h.event_id.clone())
            .and_modify(|m| {
                m.score += bonus;
                if !m.sources.contains(&src::LEX) {
                    m.sources.push(src::LEX);
                }
            })
            .or_insert_with(|| HybridHit {
                event_id: h.event_id.clone(),
                content: h.snippet.clone(),
                score: bonus,
                sources: vec![src::LEX],
            });
    }
    for (rank, h) in vec_hits.iter().enumerate() {
        let bonus = 1.0 / (RRF_K + rank as f32 + 1.0);
        merged
            .entry(h.event_id.clone())
            .and_modify(|m| {
                m.score += bonus;
                if !m.sources.contains(&src::VEC) {
                    m.sources.push(src::VEC);
                }
            })
            .or_insert_with(|| HybridHit {
                event_id: h.event_id.clone(),
                content: h.content.clone(),
                score: bonus,
                sources: vec![src::VEC],
            });
    }

    // Temporal filter via FactsStore::is_event_valid_at.
    if let Some(t) = at_time {
        let mut keep = HashMap::with_capacity(merged.len());
        for (event_id, hit) in merged {
            let valid = {
                let guard = state.facts.lock().await;
                guard
                    .is_event_valid_at(&event_id, t)
                    .map_err(internal("temporal_filter_failed", "is_event_valid_at"))?
            };
            if valid {
                keep.insert(event_id, hit);
            }
        }
        merged = keep;
    }

    let mut out: Vec<HybridHit> = merged.into_values().collect();
    // T-57: apply recency bias before the final sort. Weight read
    // per-request from config so a live edit to `[retriever]
    // .recency_weight` takes effect on the next query. Mirrors the
    // CLI path in `HybridRetriever::search`.
    let (recency_weight, half_lives) = {
        let cfg = crate::config::Config::load(&state.home).unwrap_or_default();
        (
            cfg.retriever.recency_weight,
            cfg.retriever.decay_half_lives_in_days(),
        )
    };
    if recency_weight != 0.0 {
        for hit in out.iter_mut() {
            if let Some((ts, kind)) = meta_by_id.get(&hit.event_id) {
                // T-73: per-kind half-life lookup. Empty kind (legacy
                // docs) or unknown extension kinds resolve to None →
                // uniform tau via the helper's fallback branch.
                let half_life = if kind.is_empty() {
                    None
                } else {
                    half_lives.get(kind).copied()
                };
                hit.score = crate::retriever::apply_recency_bonus_kind(
                    hit.score,
                    *ts,
                    now,
                    recency_weight,
                    half_life,
                );
            }
        }
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(k);
    Ok(out)
}

fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>> {
    let parsed =
        DateTime::parse_from_rfc3339(s).map_err(|e| anyhow::anyhow!("parse RFC3339 {s:?}: {e}"))?;
    Ok(parsed.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// /recall (T-22 + T-44)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RecallRequest {
    pub entity: String,
    #[serde(default)]
    pub at_time: Option<String>,
    /// Container tag filter (T-51b). Subset match on the facts'
    /// inherited tags. Empty or absent skips filtering.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct RecallResponse {
    pub ok: bool,
    pub facts: Vec<RecallFact>,
}

#[derive(Debug, Serialize)]
pub struct RecallFact {
    pub predicate: String,
    pub object: String,
    pub valid_from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    pub sources: Vec<String>,
}

pub async fn recall(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RecallRequest>,
) -> Result<Json<RecallResponse>, ApiError> {
    if req.entity.trim().is_empty() {
        return Err(ApiError::bad_request(
            "empty_entity",
            "entity must not be empty",
        ));
    }
    let at_time = req
        .at_time
        .as_deref()
        .map(parse_rfc3339)
        .transpose()
        .map_err(|e| ApiError::bad_request("invalid_at_time", format!("{e:#}")))?;

    let tag_filter = if req.tags.is_empty() {
        None
    } else {
        Some(&req.tags)
    };
    // /recall is the audit-grade entity pull. Per SPEC_V0_2,
    // `visibility=private` captures surface here (T-51c). Retention
    // TTL still applies: an expired ephemeral capture stays hidden
    // everywhere, including audit.
    let visibility = crate::reserved_tags::Visibility::IncludePrivate;
    let now = Utc::now();
    let rows = {
        let facts = state.facts.lock().await;
        match at_time {
            Some(t) => {
                // facts_at_time predates T-51b/T-51c; apply the same
                // filter stack in-process. Result sets are subject-
                // scoped, so the O(n) pass is bounded by the per-
                // entity fact count.
                let raw = facts
                    .facts_at_time(&req.entity, t)
                    .map_err(internal("facts_query_failed", "facts_at_time"))?;
                raw.into_iter()
                    .filter(|fact| {
                        if let Some(f) = tag_filter {
                            if !crate::tag_match::matches(&fact.tags, f) {
                                return false;
                            }
                        }
                        crate::reserved_tags::is_visible(
                            &fact.tags,
                            fact.valid_from,
                            now,
                            visibility,
                        )
                    })
                    .collect()
            }
            None => facts
                .facts_for_subject_filtered(&req.entity, tag_filter, visibility, now)
                .map_err(internal("facts_query_failed", "facts_for_subject_filtered"))?,
        }
    };

    let facts = rows
        .into_iter()
        .map(|f| RecallFact {
            predicate: f.predicate,
            object: f.object,
            valid_from: f.valid_from.to_rfc3339_opts(SecondsFormat::Millis, true),
            valid_to: f
                .valid_to
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true)),
            sources: f.source_events.iter().map(|e| e.to_string()).collect(),
        })
        .collect();

    Ok(Json(RecallResponse { ok: true, facts }))
}

// ---------------------------------------------------------------------------
// /profile (T-22 + T-44)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ProfileRequest {
    #[serde(default)]
    pub scope: Option<String>,
    /// Container tag filter (T-51b). Subset match against each fact's
    /// inherited tags. Empty or absent skips filtering.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct ProfileResponse {
    pub ok: bool,
    pub profile_md: String,
    pub generated_at: String,
    pub fact_count: u32,
}

pub async fn profile(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ProfileRequest>,
) -> Result<Json<ProfileResponse>, ApiError> {
    let scope = req.scope.as_deref();
    let tag_filter = if req.tags.is_empty() {
        None
    } else {
        Some(&req.tags)
    };
    // /profile is a synthesis route. Per SPEC_V0_2, `visibility=
    // private` captures stay hidden here (T-51c); the audit path is
    // /recall.
    let now = Utc::now();
    let facts = {
        let guard = state.facts.lock().await;
        guard
            .all_live_facts_filtered(
                now,
                scope,
                tag_filter,
                crate::reserved_tags::Visibility::Default,
                now,
            )
            .map_err(internal("facts_query_failed", "all_live_facts_filtered"))?
    };
    let md = synthesize_profile_md(&facts, scope);
    Ok(Json(ProfileResponse {
        ok: true,
        profile_md: md,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        fact_count: facts.len() as u32,
    }))
}

fn synthesize_profile_md(facts: &[crate::facts::Fact], scope: Option<&str>) -> String {
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<String, BTreeMap<String, Vec<&crate::facts::Fact>>> = BTreeMap::new();
    for f in facts {
        grouped
            .entry(f.subject.clone())
            .or_default()
            .entry(f.predicate.clone())
            .or_default()
            .push(f);
    }
    let mut md = String::new();
    md.push_str("# localmem profile\n\n");
    if let Some(s) = scope {
        md.push_str(&format!("**Scope:** `{s}`\n\n"));
    }
    md.push_str(&format!("**Facts:** {}\n\n", facts.len()));
    if grouped.is_empty() {
        md.push_str("_No facts to display._\n");
        return md;
    }
    for (subject, predicates) in &grouped {
        md.push_str(&format!("## {subject}\n\n"));
        for (predicate, fs) in predicates {
            md.push_str(&format!("- **{predicate}**\n"));
            for f in fs {
                let valid_from = f.valid_from.to_rfc3339_opts(SecondsFormat::Millis, true);
                md.push_str(&format!(
                    "  - {} _(conf={:.2}, valid_from={})_\n",
                    f.object, f.confidence, valid_from,
                ));
            }
        }
        md.push('\n');
    }
    md
}

// ---------------------------------------------------------------------------
// /forget (T-22 + T-44)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ForgetRequest {
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub criteria: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct ForgetResponse {
    pub ok: bool,
    pub forgotten_event_ids: Vec<String>,
}

pub async fn forget(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ForgetRequest>,
) -> Result<Json<ForgetResponse>, ApiError> {
    if req.target_id.is_some() && req.criteria.is_some() {
        return Err(ApiError::bad_request(
            "both_target_and_criteria",
            "forget accepts target_id OR criteria, not both",
        ));
    }
    if req.target_id.is_none() && req.criteria.is_none() {
        return Err(ApiError::bad_request(
            "target_or_criteria_required",
            "forget requires target_id or criteria",
        ));
    }

    if let Some(target_id_str) = req.target_id {
        let target_id = EventId::from_str(&target_id_str).map_err(|_| {
            ApiError::bad_request(
                "invalid_target_id",
                format!("not a valid ULID: {target_id_str}"),
            )
        })?;
        let forget_event = Event::new(
            EventKind::Forget(ForgetPayload {
                target_id,
                reason: "user requested".into(),
                scope: None,
                extra: Map::new(),
            }),
            Source {
                app: "mcp".into(),
                host: hostname(),
                user: None,
            },
        );
        let forget_event_id = forget_event.id.to_string();
        state
            .event_log
            .append(&forget_event)
            .map_err(internal("append_failed", "append forget event"))?;
        {
            let facts = state.facts.lock().await;
            facts
                .retire_facts_for_target(&target_id.to_string(), forget_event.ts)
                .map_err(internal("retire_failed", "retire facts on forget"))?;
        }
        info!(target = %target_id_str, forget = %forget_event_id, "appended forget event");
        return Ok(Json(ForgetResponse {
            ok: true,
            forgotten_event_ids: vec![forget_event_id],
        }));
    }

    // Criteria path: v0.1 supports {subject, predicate} equality.
    let criteria = req.criteria.expect("criteria is Some here");
    let subject = criteria
        .get("subject")
        .and_then(Value::as_str)
        .map(str::to_string);
    let predicate = criteria
        .get("predicate")
        .and_then(Value::as_str)
        .map(str::to_string);
    if subject.is_none() && predicate.is_none() {
        return Err(ApiError::bad_request(
            "criteria_empty",
            "criteria requires at least `subject` or `predicate`",
        ));
    }
    let candidates = {
        let facts = state.facts.lock().await;
        match subject.as_deref() {
            Some(s) => facts
                .facts_for_subject(s)
                .map_err(internal("facts_query_failed", "facts_for_subject"))?,
            None => facts
                .all_live_facts(Utc::now(), None)
                .map_err(internal("facts_query_failed", "all_live_facts"))?,
        }
    };
    let matched: Vec<_> = candidates
        .into_iter()
        .filter(|f| f.retired_at.is_none())
        .filter(|f| match &predicate {
            Some(p) => &f.predicate == p,
            None => true,
        })
        .collect();
    let mut ids = Vec::with_capacity(matched.len());
    for fact in &matched {
        let forget_event = Event::new(
            EventKind::Forget(ForgetPayload {
                target_id: fact.id,
                reason: "user requested (criteria)".into(),
                scope: Some("criteria".into()),
                extra: Map::new(),
            }),
            Source {
                app: "mcp".into(),
                host: hostname(),
                user: None,
            },
        );
        state
            .event_log
            .append(&forget_event)
            .map_err(internal("append_failed", "append forget event"))?;
        {
            let facts = state.facts.lock().await;
            facts
                .retire_facts_for_target(&fact.id.to_string(), forget_event.ts)
                .map_err(internal("retire_failed", "retire fact on criteria forget"))?;
        }
        ids.push(forget_event.id.to_string());
    }
    Ok(Json(ForgetResponse {
        ok: true,
        forgotten_event_ids: ids,
    }))
}

// ---------------------------------------------------------------------------
// /journal (T-22 + T-44)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct JournalRequest {
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct JournalResponse {
    pub ok: bool,
    pub entries: Vec<JournalEntry>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct JournalEntry {
    pub ts: String,
    pub action: String,
    pub rule: String,
    pub input_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

pub async fn journal(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JournalRequest>,
) -> Result<Json<JournalResponse>, ApiError> {
    let since = req
        .since
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|e: anyhow::Error| {
            ApiError::bad_request("invalid_since", format!("since: {e:#}"))
        })?
        .unwrap_or_else(|| Duration::days(1));
    let cutoff = Utc::now() - since;
    let action_filter = req.action.as_deref().map(str::to_ascii_uppercase);

    let mut entries: Vec<JournalEntry> = Vec::new();
    for ev in state
        .journal
        .iter()
        .map_err(internal("journal_iter_failed", "open journal iter"))?
    {
        let entry = ev.map_err(internal("journal_parse_failed", "parse journal line"))?;
        if entry.ts < cutoff {
            continue;
        }
        let action = match entry.action {
            PolicyAction::Commit => "COMMIT",
            PolicyAction::Update => "UPDATE",
            PolicyAction::Dedup => "DEDUP",
            PolicyAction::Skip => "SKIP",
            PolicyAction::Forget => "FORGET",
        };
        if let Some(f) = action_filter.as_deref() {
            if !action.eq_ignore_ascii_case(f) {
                continue;
            }
        }
        entries.push(JournalEntry {
            ts: entry.ts.to_rfc3339_opts(SecondsFormat::Millis, true),
            action: action.to_string(),
            rule: entry.rule,
            input_id: entry.input_id.to_string(),
            reasoning: entry.reasoning,
        });
    }
    Ok(Json(JournalResponse { ok: true, entries }))
}

fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration");
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: i64 = num
        .parse()
        .map_err(|e| anyhow::anyhow!("parse number in duration {s:?}: {e}"))?;
    Ok(match unit {
        "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        other => anyhow::bail!("unknown duration unit {other:?} in {s:?}"),
    })
}

// ---------------------------------------------------------------------------
// /resource/* (T-54) — Read-only discovery primitives backing MCP Resources.
//
// Four GET endpoints feed the four MCP Resource URIs registered in the
// TS server: profile, subjects, tags, recent. Body shape mirrors the CLI
// JSON output for each command so the MCP server can pass the result
// through with a thin schema check.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ResourceProfileResponse {
    pub ok: bool,
    pub profile_md: String,
    pub generated_at: String,
    pub fact_count: u32,
}

pub async fn resource_profile(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ResourceProfileResponse>, ApiError> {
    let now = Utc::now();
    let facts = {
        let guard = state.facts.lock().await;
        guard
            .all_live_facts_filtered(
                now,
                None,
                None,
                crate::reserved_tags::Visibility::Default,
                now,
            )
            .map_err(internal("facts_query_failed", "all_live_facts_filtered"))?
    };
    let md = synthesize_profile_md(&facts, None);
    Ok(Json(ResourceProfileResponse {
        ok: true,
        profile_md: md,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        fact_count: facts.len() as u32,
    }))
}

#[derive(Debug, Serialize)]
pub struct ResourceSubjectsResponse {
    pub ok: bool,
    pub subjects: Vec<SubjectRow>,
}

#[derive(Debug, Serialize)]
pub struct SubjectRow {
    pub subject: String,
    pub count: u64,
}

pub async fn resource_subjects(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ResourceSubjectsResponse>, ApiError> {
    let rows = {
        let guard = state.facts.lock().await;
        guard
            .subjects()
            .map_err(internal("facts_query_failed", "subjects"))?
    };
    Ok(Json(ResourceSubjectsResponse {
        ok: true,
        subjects: rows
            .into_iter()
            .map(|(subject, count)| SubjectRow { subject, count })
            .collect(),
    }))
}

#[derive(Debug, Serialize)]
pub struct ResourceTagsResponse {
    pub ok: bool,
    pub tags: Vec<TagRow>,
}

#[derive(Debug, Serialize)]
pub struct TagRow {
    pub key: String,
    pub value: String,
    pub count: u64,
}

pub async fn resource_tags(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ResourceTagsResponse>, ApiError> {
    let rows =
        aggregate_capture_tags(&state).map_err(internal("tag_aggregate_failed", "tags walk"))?;
    Ok(Json(ResourceTagsResponse { ok: true, tags: rows }))
}

/// Walk the event log to count `key=value` tag pairs across all
/// committed (non-forgotten) captures. Mirrors `cli::tags`
/// aggregation; kept duplicated to avoid a CLI -> server dependency
/// cycle. See SPEC_V0_2 "Discovery API" and the CLI module for the
/// rationale on event-log-as-source-of-truth.
fn aggregate_capture_tags(state: &AppState) -> Result<Vec<TagRow>> {
    use crate::event::ForgetPayload;
    let mut forgotten: std::collections::HashSet<EventId> = std::collections::HashSet::new();
    for ev in state.event_log.iter()? {
        let ev = ev?;
        if let EventKind::Forget(ForgetPayload { target_id, .. }) = ev.kind {
            forgotten.insert(target_id);
        }
    }
    let mut counts: BTreeMap<(String, String), u64> = BTreeMap::new();
    for ev in state.event_log.iter()? {
        let ev = ev?;
        if let EventKind::Capture(p) = &ev.kind {
            if forgotten.contains(&ev.id) {
                continue;
            }
            for (k, v) in &p.tags {
                *counts.entry((k.clone(), v.clone())).or_default() += 1;
            }
        }
    }
    let mut rows: Vec<TagRow> = counts
        .into_iter()
        .map(|((k, v), c)| TagRow {
            key: k,
            value: v,
            count: c,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.value.cmp(&b.value))
    });
    Ok(rows)
}

#[derive(Debug, Deserialize)]
pub struct RecentQuery {
    /// Optional cap. Defaults to [`crate::cli::recent::DEFAULT_LIMIT`].
    /// Capped at [`RECENT_LIMIT_MAX`] to keep responses bounded.
    #[serde(default)]
    pub limit: Option<usize>,
}

const RECENT_LIMIT_MAX: usize = 200;

#[derive(Debug, Serialize)]
pub struct ResourceRecentResponse {
    pub ok: bool,
    pub captures: Vec<crate::cli::recent::RecentCapture>,
}

pub async fn resource_recent(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<RecentQuery>,
) -> Result<Json<ResourceRecentResponse>, ApiError> {
    let limit = q
        .limit
        .unwrap_or(crate::cli::recent::DEFAULT_LIMIT)
        .min(RECENT_LIMIT_MAX);
    let rows = crate::cli::recent::load_recent(state.event_log.as_ref(), limit)
        .map_err(internal("recent_walk_failed", "load_recent"))?;
    Ok(Json(ResourceRecentResponse {
        ok: true,
        captures: rows,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort hostname for the `Source.host` field. Falls back to a stable
/// constant in CI-style environments where neither `HOST` nor `HOSTNAME` is
/// set. Capture content never depends on this value, only the audit trail.
fn hostname() -> String {
    std::env::var("HOST")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".into())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{router, AppState};
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Method, Request, StatusCode};
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tower::ServiceExt;

    const BODY_LIMIT: usize = 64 * 1024;

    fn force_no_embedder() {
        std::env::set_var("LOCALMEM_MODEL_DIR", "/this/path/does/not/exist");
    }
    fn restore_embedder_env() {
        std::env::remove_var("LOCALMEM_MODEL_DIR");
    }

    async fn new_state(home: &Path) -> Arc<AppState> {
        force_no_embedder();
        let state = AppState::open(home).await.unwrap();
        restore_embedder_env();
        state
    }

    async fn post(state: Arc<AppState>, uri: &str, body: Value) -> (StatusCode, Value) {
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), BODY_LIMIT).await.unwrap();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, parsed)
    }

    // ---- /write ----------------------------------------------------------

    #[tokio::test]
    async fn write_appends_event_and_returns_commit_for_long_content() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, body) = post(
            state.clone(),
            "/write",
            json!({
                "content": "I prefer functional Rust and avoid macros where possible.",
                "source": "claude-code"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["action"], "COMMIT");
        let event_id = body["event_id"].as_str().expect("event_id is a string");
        assert_eq!(event_id.len(), 26);
        // Real extractor should produce at least one fact from this content.
        assert!(
            body["facts_extracted"].as_u64().unwrap() >= 1,
            "expected fact extraction on COMMIT, got: {body:?}"
        );
    }

    #[tokio::test]
    async fn write_short_content_is_skipped_by_default_policy() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, body) = post(
            state.clone(),
            "/write",
            json!({"content": "ok", "source": "test"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["action"], "SKIP");
        assert_eq!(body["facts_extracted"], 0);
    }

    #[tokio::test]
    async fn write_empty_content_is_400() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, body) = post(state, "/write", json!({"content": ""})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "empty_content");
    }

    #[tokio::test]
    async fn server_write_runs_rewriter_when_configured_and_surfaces_rewrite() {
        // T-55: with [rewriter].mode = "regex" in the home's
        // config.toml, /write must return `rewritten_text` AND lex
        // must index the rewritten string so subsequent searches
        // surface a snippet containing the user's name.
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
[home]
user_name = "Vijay"

[rewriter]
mode = "regex"
"#,
        )
        .unwrap();
        let state = new_state(tmp.path()).await;
        let (status, body) = post(
            state.clone(),
            "/write",
            json!({
                "content": "I prefer functional Rust over OO ceremony any day."
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rewritten = body["rewritten_text"]
            .as_str()
            .expect("regex rewrite must return rewritten_text");
        assert!(rewritten.contains("Vijay"));

        // Lex returns the rewritten snippet.
        let idx = crate::lexical::LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let hits = idx.search("functional Rust", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("Vijay"));
    }

    #[tokio::test]
    async fn server_write_in_default_mode_omits_rewritten_text() {
        // Default config = none mode. /write must NOT include a
        // `rewritten_text` key in the response (wire-shape
        // backward-compat).
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, body) = post(
            state,
            "/write",
            json!({"content": "I prefer functional Rust over OO."}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.get("rewritten_text").is_none(),
            "default (none) mode must NOT emit rewritten_text, got: {body}"
        );
    }

    #[tokio::test]
    async fn write_persists_tags_for_filtered_lex_lookup() {
        // T-51: tags supplied in /write must round-trip into the
        // capture's payload and become queryable through the lex tag
        // filter we built. We assert via a direct LexicalIndex search
        // (post-write) because /search does not yet expose the tags
        // filter — that's the next slice (T-51 search CLI wiring).
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, _) = post(
            state,
            "/write",
            json!({
                "content": "rust async runtime notes for localmem",
                "tags": {"project": "localmem", "topic": "async"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let idx = crate::lexical::LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let mut filter = BTreeMap::new();
        filter.insert("project".into(), "localmem".into());
        let hits = idx.search("rust async", 10, Some(&filter)).unwrap();
        assert_eq!(hits.len(), 1, "tagged write should be filter-visible");
    }

    #[tokio::test]
    async fn search_hides_private_capture_but_recall_surfaces_it() {
        // T-51c: visibility=private is the canonical default-hide /
        // audit-only-surface case. Search must not return the private
        // capture; entity-only recall must.
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        // Phrasing chosen to match the rule extractor ("I prefer X")
        // so the capture commits a derived fact about `user`.
        post(
            state.clone(),
            "/write",
            json!({
                "content": "I prefer functional Rust patterns for systems work.",
                "tags": {"visibility": "private"}
            }),
        )
        .await;
        post(
            state.clone(),
            "/write",
            json!({
                "content": "I prefer object-oriented Go for distributed systems."
            }),
        )
        .await;

        // Default search drops the private capture: "functional"
        // would match only the private capture, so 0 results proves
        // the filter is working.
        let (status, body) = post(
            state.clone(),
            "/search",
            json!({"query": "functional", "k": 10}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let results = body["results"].as_array().unwrap();
        for r in results {
            let fact = r["fact"].as_str().unwrap().to_lowercase();
            assert!(
                !fact.contains("functional"),
                "search must not surface private capture's text, got: {fact}"
            );
        }

        // Recall on `user` surfaces both private + public derived facts
        // (entity-only audit path includes private). The two writes
        // each commit one preference fact about user.
        let (status, body) = post(state, "/recall", json!({"entity": "user"})).await;
        assert_eq!(status, StatusCode::OK);
        let facts = body["facts"].as_array().unwrap();
        assert_eq!(
            facts.len(),
            2,
            "recall on user must surface both public AND private facts, got: {body}"
        );
    }

    #[tokio::test]
    async fn write_kind_round_trips_via_extra() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, _) = post(
            state,
            "/write",
            json!({
                "content": "I prefer functional Rust and avoid macros where possible.",
                "kind": "preference"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn write_then_search_round_trip() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        post(
            state.clone(),
            "/write",
            json!({"content": "stripe webhook signature verification failure"}),
        )
        .await;
        let (status, body) =
            post(state, "/search", json!({"query": "stripe webhook", "k": 5})).await;
        assert_eq!(status, StatusCode::OK);
        let results = body["results"].as_array().unwrap();
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn search_empty_query_is_400() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, _) = post(state, "/search", json!({"query": ""})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_clamps_k_to_max_100() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, _) = post(state, "/search", json!({"query": "hello", "k": 9999})).await;
        assert_eq!(status, StatusCode::OK);
    }

    // ---- /recall ---------------------------------------------------------

    #[tokio::test]
    async fn recall_empty_entity_is_400() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, _) = post(state, "/recall", json!({"entity": ""})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn recall_returns_facts_after_write() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        post(
            state.clone(),
            "/write",
            json!({"content": "I prefer functional Rust and avoid macros where possible."}),
        )
        .await;
        let (status, body) = post(state, "/recall", json!({"entity": "user"})).await;
        assert_eq!(status, StatusCode::OK);
        let facts = body["facts"].as_array().unwrap();
        assert!(
            !facts.is_empty(),
            "expected facts for `user`, got: {body:?}"
        );
    }

    // ---- /profile --------------------------------------------------------

    #[tokio::test]
    async fn recall_with_tags_filters_to_matching_capture() {
        // T-51b: facts inherit capture tags; /recall with a tag filter
        // returns only facts whose source capture carried it.
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        post(
            state.clone(),
            "/write",
            json!({
                "content": "I prefer functional Rust and avoid macros where possible.",
                "tags": {"project": "localmem"}
            }),
        )
        .await;
        post(
            state.clone(),
            "/write",
            json!({
                "content": "I prefer object-oriented Go and use generics everywhere.",
                "tags": {"project": "other"}
            }),
        )
        .await;
        let (status, body) = post(
            state,
            "/recall",
            json!({"entity": "user", "tags": {"project": "localmem"}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let facts = body["facts"].as_array().unwrap();
        for f in facts {
            // We can't observe tags directly on the response (the
            // RecallFact shape doesn't expose them yet), but every
            // surviving fact should reference the localmem capture's
            // text. Negative assertion would be cleaner but the
            // RecallFact projection drops the source text.
            assert!(
                !f["object"].as_str().unwrap().to_lowercase().contains("go"),
                "Go-tagged fact should have been filtered out, got: {f}"
            );
        }
    }

    #[tokio::test]
    async fn profile_returns_markdown_for_indexed_facts() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        post(
            state.clone(),
            "/write",
            json!({"content": "I prefer functional Rust and avoid macros where possible."}),
        )
        .await;
        let (status, body) = post(state, "/profile", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let md = body["profile_md"].as_str().unwrap();
        assert!(md.contains("user"), "profile should mention `user` subject");
    }

    #[tokio::test]
    async fn profile_with_tags_filters_to_matching_facts() {
        // T-51b: /profile applies the same tag filter to its rendered
        // facts. We assert via fact_count (any non-matching capture
        // would inflate the count) since the rendered markdown is
        // dependent on extractor output ordering.
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        post(
            state.clone(),
            "/write",
            json!({
                "content": "I prefer functional Rust and avoid macros where possible.",
                "tags": {"project": "localmem"}
            }),
        )
        .await;
        post(
            state.clone(),
            "/write",
            json!({
                "content": "I prefer object-oriented Go and use generics everywhere.",
                "tags": {"project": "other"}
            }),
        )
        .await;
        let (status, body) = post(
            state.clone(),
            "/profile",
            json!({"tags": {"project": "localmem"}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let filtered_count = body["fact_count"].as_u64().unwrap();

        // Compare against no-filter call to confirm we dropped rows.
        let (_, all_body) = post(state, "/profile", json!({})).await;
        let total_count = all_body["fact_count"].as_u64().unwrap();
        assert!(
            filtered_count < total_count,
            "filter should shrink fact_count: filtered={filtered_count} total={total_count}"
        );
    }

    // ---- /forget ---------------------------------------------------------

    #[tokio::test]
    async fn forget_target_appends_forget_event() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let bogus_id = "01HXY00000000000000000000Z";
        let (status, body) = post(state, "/forget", json!({"target_id": bogus_id})).await;
        assert_eq!(status, StatusCode::OK);
        let ids = body["forgotten_event_ids"].as_array().unwrap();
        assert_eq!(ids.len(), 1);
    }

    #[tokio::test]
    async fn forget_invalid_target_is_400() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, body) = post(state, "/forget", json!({"target_id": "not-a-ulid"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_target_id");
    }

    #[tokio::test]
    async fn forget_without_target_or_criteria_is_400() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, body) = post(state, "/forget", json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "target_or_criteria_required");
    }

    // ---- /journal --------------------------------------------------------

    #[tokio::test]
    async fn journal_returns_entries_after_write() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        post(
            state.clone(),
            "/write",
            json!({"content": "I prefer functional Rust and avoid macros where possible."}),
        )
        .await;
        let (status, body) = post(state, "/journal", json!({"since": "1h"})).await;
        assert_eq!(status, StatusCode::OK);
        let entries = body["entries"].as_array().unwrap();
        assert!(
            !entries.is_empty(),
            "journal should have at least one entry"
        );
        assert_eq!(entries[0]["action"], "COMMIT");
    }

    #[tokio::test]
    async fn journal_action_filter_keeps_matching_only() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        post(
            state.clone(),
            "/write",
            json!({"content": "I prefer functional Rust and avoid macros where possible."}),
        )
        .await;
        let (status, body) =
            post(state, "/journal", json!({"since": "1h", "action": "SKIP"})).await;
        assert_eq!(status, StatusCode::OK);
        let entries = body["entries"].as_array().unwrap();
        assert!(entries.is_empty(), "no SKIP entries expected");
    }

    // ---- /resource/* (T-54) --------------------------------------------

    async fn get(state: Arc<AppState>, uri: &str) -> (StatusCode, Value) {
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), BODY_LIMIT).await.unwrap();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, parsed)
    }

    #[tokio::test]
    async fn resource_profile_returns_md_and_count() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        post(
            state.clone(),
            "/write",
            json!({"content": "I prefer functional Rust and avoid macros where possible."}),
        )
        .await;
        let (status, body) = get(state, "/resource/profile").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert!(body["profile_md"].as_str().unwrap().starts_with("# localmem profile"));
        assert!(body["fact_count"].as_u64().unwrap() >= 1);
        assert!(body["generated_at"].as_str().unwrap().contains('T'));
    }

    #[tokio::test]
    async fn resource_subjects_returns_distinct_rows() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        post(
            state.clone(),
            "/write",
            json!({"content": "I prefer functional Rust and avoid macros."}),
        )
        .await;
        let (status, body) = get(state, "/resource/subjects").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        let arr = body["subjects"].as_array().unwrap();
        assert!(!arr.is_empty());
        assert!(arr[0]["subject"].is_string());
        assert!(arr[0]["count"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn resource_tags_aggregates_capture_tags() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        post(
            state.clone(),
            "/write",
            json!({
                "content": "I prefer functional Rust and avoid macros.",
                "tags": {"project": "localmem", "topic": "lang"}
            }),
        )
        .await;
        let (status, body) = get(state, "/resource/tags").await;
        assert_eq!(status, StatusCode::OK);
        let arr = body["tags"].as_array().unwrap();
        assert!(arr.iter().any(|t| t["key"] == "project" && t["value"] == "localmem"));
        assert!(arr.iter().any(|t| t["key"] == "topic" && t["value"] == "lang"));
    }

    #[tokio::test]
    async fn resource_recent_returns_newest_first_with_default_limit() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        for i in 0..3 {
            post(
                state.clone(),
                "/write",
                json!({"content": format!("memory item {i} that is long enough to commit")}),
            )
            .await;
        }
        let (status, body) = get(state, "/resource/recent").await;
        assert_eq!(status, StatusCode::OK);
        let captures = body["captures"].as_array().unwrap();
        assert_eq!(captures.len(), 3);
        // Newest first: "memory item 2" leads.
        assert!(captures[0]["text"]
            .as_str()
            .unwrap()
            .contains("memory item 2"));
    }

    #[tokio::test]
    async fn resource_recent_respects_limit_query_param() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        for i in 0..5 {
            post(
                state.clone(),
                "/write",
                json!({"content": format!("memory item {i} that is long enough to commit")}),
            )
            .await;
        }
        let (status, body) = get(state, "/resource/recent?limit=2").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["captures"].as_array().unwrap().len(), 2);
    }
}
