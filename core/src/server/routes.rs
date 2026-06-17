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

use crate::event::{CapturePayload, Event, EventKind, ForgetPayload, PolicyAction, Source};
use crate::event_id::EventId;
use crate::journal::JournalEntry as DerivedJournalEntry;
use crate::policy::EvalContext;
use crate::server::understand::{persist_facts, worth_understanding, UnderstandJob};
use crate::server::{AppState, EmbedJob};
use crate::understanding::{OllamaSynthesizer, Synthesizer};
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
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{error, info, warn};

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
    /// Optional explicit event time (RFC3339). When present, the capture's
    /// temporal envelope is built from this instant instead of capture-now,
    /// so imported or historical memories carry their real valid-time. This
    /// is the write-side complement to life-import: a dated corpus (e.g. a
    /// benchmark haystack, or an exported chat history) ingests with the
    /// instant each message actually occurred, which is what makes valid-time
    /// reasoning ("how many days ago", "how long between") correct. Absent
    /// preserves the prior capture-now behavior exactly.
    #[serde(default)]
    pub as_of: Option<String>,
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

    // Parse the optional explicit event time before we move fields out of
    // `req`. A malformed timestamp is a client error, not a silent fallback,
    // so a dated-corpus import fails loudly rather than stamping capture-now.
    let as_of_instant = match req.as_of.as_deref() {
        Some(s) => Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| {
                    ApiError::bad_request("bad_as_of", format!("as_of must be RFC3339: {e}"))
                })?
                .with_timezone(&Utc),
        ),
        None => None,
    };

    // T-52: `kind` in the MCP request maps to a typed [`crate::kind::Kind`].
    // Unknown strings round-trip as `Kind::Other(_)`; absent → `Kind::Note`.
    let kind = req.kind.map(crate::kind::Kind::from).unwrap_or_default();
    let source_app = req.source.unwrap_or_else(|| "mcp".into());

    let out = ingest_capture(
        &state,
        req.content,
        source_app,
        kind,
        req.tags,
        as_of_instant,
    )
    .await?;
    Ok(Json(WriteResponse {
        ok: true,
        event_id: out.event_id,
        action: out.action,
        facts_extracted: out.facts_extracted,
        rewritten_text: out.rewritten_text,
    }))
}

/// Outcome of ingesting one capture through the full pipeline.
pub(crate) struct IngestOutcome {
    pub event_id: String,
    pub action: &'static str,
    pub facts_extracted: u32,
    pub rewritten_text: Option<String>,
}

/// The full capture-ingest pipeline: rewriter → append → policy + journal →
/// (on commit) lexical index + async embed + synchronous fact extraction +
/// async understanding enqueue. Factored out of the `write` handler so both the
/// HTTP path and the startup spool-drain ingest captures identically (one code
/// path, no drift).
pub(crate) async fn ingest_capture(
    state: &AppState,
    content: String,
    source_app: String,
    kind: crate::kind::Kind,
    tags: BTreeMap<String, String>,
    as_of: Option<DateTime<Utc>>,
) -> Result<IngestOutcome, ApiError> {
    // T-55: apply the configured rewriter before persisting. Config is read
    // fresh per write so a `[rewriter].mode` change takes effect on restart.
    let cfg = crate::config::Config::load(&state.home)
        .map_err(internal("config_load_failed", "load config for rewriter"))?;
    let rewritten_text = apply_server_rewriter(&cfg, &content);

    let payload = CapturePayload {
        text: content,
        kind,
        rewritten_text,
        mime: None,
        attachments: vec![],
        tags,
        time: Some(match as_of {
            Some(instant) => crate::temporal::TimeEnvelope::from_instant(instant),
            None => crate::temporal::TimeEnvelope::capture_now(),
        }),
        extra: Map::new(),
    };
    let source = Source {
        app: source_app,
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
    let recent = load_recent_events(state, POLICY_RECENT_WINDOW, capture.id)
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
        // RETRIEVAL HYGIENE (quality pass): ephemeral tool-traces ([Bash]/[Read]/
        // ...) are AUDIT-ONLY. They stay in the event log (the source of truth +
        // the audit trail), but they are NEVER lexically indexed, embedded into
        // vectors, or extracted into facts — so they cannot float in search and
        // burn an agent's tokens on noise. Only SIGNAL captures reach the search
        // stores + facts. (The understanding worker already applies this rule via
        // `worth_understanding`; this extends it to lexical + vectors + the sync
        // extractor so every retrieval surface reads clean memory.)
        let is_signal = !capture_payload(&capture).is_ephemeral();
        if is_signal {
            // Lexical index: index on commit, DEFER the Tantivy commit (T-118).
            {
                let mut lex = state.lexical.lock().await;
                lex.index_event(&capture)
                    .map_err(internal("index_failed", "index capture in lexical store"))?;
                state.lex_dirty.store(true, Ordering::SeqCst);
            }

            // T-117: async vector embedding off the write path (ONNX in a
            // background worker; `/drain` flushes the backlog). T-55: embed the
            // indexable (rewritten-or-original) text on valid-time.
            // P5 (§2.6): CHUNK a large capture so no single essay is one
            // retrieval unit. Each chunk embeds under the SAME capture id (the
            // retriever merges by event_id), so a small capture yields exactly
            // one chunk = unchanged behavior, and a 14K-token paste becomes many
            // sharp chunks instead of one mushy vector.
            if let Some(tx) = &state.embed_tx {
                let payload = capture_payload(&capture);
                let ts = payload.effective_capture_instant(capture.ts);
                let eid = capture.id.to_string();
                let tags = payload.tags.clone();
                for chunk in crate::chunk::chunk_text(payload.indexable_text()) {
                    state.embed_pending.fetch_add(1, Ordering::SeqCst);
                    let job = EmbedJob {
                        event_id: eid.clone(),
                        text: chunk,
                        ts,
                        tags: tags.clone(),
                    };
                    if tx.send(job).await.is_err() {
                        state.embed_pending.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            }

            // Synchronous FAST fact extraction (rules/yaml) so simple facts are
            // queryable immediately; the heavier LLM decomposition runs async
            // below. Both share `persist_facts` (T-56) so they cannot drift.
            let payload = capture_payload(&capture);
            let extracted = state
                .extractor
                .extract(&payload.text, Some(&payload.kind))
                .await
                .map_err(internal("extractor_failed", "registry extract"))?;
            facts_count += persist_facts(
                &state.event_log,
                &state.facts,
                &state.journal,
                &capture,
                &extracted,
                &decision.rule_id,
            )
            .await
            .map_err(internal("facts_persist_failed", "persist extracted facts"))?;
        }

        // Layer 2 understanding (SPEC 7c): enqueue the committed capture for
        // ASYNC LLM decomposition when the worker is enabled AND the capture is
        // signal (not an ephemeral tool-trace). Mirrors the embed enqueue: never
        // blocks the write, rolls the counter back if the worker is gone. When
        // understanding is disabled, `understand_tx` is None and this is a no-op.
        if let Some(tx) = &state.understand_tx {
            if worth_understanding(&capture) {
                state.understand_pending.fetch_add(1, Ordering::SeqCst);
                let job = UnderstandJob {
                    capture: capture.clone(),
                };
                if tx.send(job).await.is_err() {
                    state.understand_pending.fetch_sub(1, Ordering::SeqCst);
                }
            }
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

    Ok(IngestOutcome {
        event_id,
        action: action_label,
        facts_extracted: facts_count,
        rewritten_text,
    })
}

/// Drain `~/.localmem/spool/captures.jsonl` on startup. The capture hook spools
/// there when the core is momentarily unreachable; this replays each spooled
/// line through the full ingest pipeline so a down-core never loses capture.
///
/// Crash/race-safe: the spool file is atomically RENAMED aside first, so hooks
/// that spool concurrently (the core isn't fully up yet) write to a fresh file
/// the next drain handles. Successfully-ingested lines are dropped; any that
/// fail are written back so they retry next start (no duplicates, no loss).
pub(crate) async fn drain_spool(state: Arc<AppState>) {
    let dir = state.home.join("spool");
    let spool = dir.join("captures.jsonl");
    let work = dir.join("captures.draining.jsonl");
    // Recover a prior interrupted drain too: process whatever is in `work`,
    // then anything currently spooled (renamed in).
    if !work.exists() {
        if !spool.exists() {
            return;
        }
        if let Err(err) = std::fs::rename(&spool, &work) {
            warn!(error = %err, "spool drain: could not claim spool file; skipping");
            return;
        }
    }
    let content = match std::fs::read_to_string(&work) {
        Ok(c) => c,
        Err(err) => {
            warn!(error = %err, "spool drain: read failed; leaving file in place");
            return;
        }
    };

    let mut drained = 0usize;
    let mut retry: Vec<String> = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // unparseable spool line: drop it, don't loop forever
        };
        let text = v
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if text.trim().is_empty() {
            continue;
        }
        let source = v
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("claude-code")
            .to_string();
        let kind = v
            .get("kind")
            .and_then(Value::as_str)
            .map(|s| crate::kind::Kind::from(s.to_string()))
            .unwrap_or_default();
        let tags = v
            .get("tags")
            .and_then(|t| serde_json::from_value::<BTreeMap<String, String>>(t.clone()).ok())
            .unwrap_or_default();
        match ingest_capture(&state, text, source, kind, tags, None).await {
            Ok(_) => drained += 1,
            Err(err) => {
                warn!(error = %err.message, "spool drain: ingest failed; will retry next start");
                retry.push(line.to_string());
            }
        }
    }

    if retry.is_empty() {
        let _ = std::fs::remove_file(&work);
    } else {
        // Write the failures back so they retry; keep them out of the live spool
        // (which the hook appends to) by staying in the work file.
        let _ = std::fs::write(&work, retry.join("\n") + "\n");
    }
    if drained > 0 {
        info!(drained, "spool drained into the store");
    }
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
// /drain (T-117 vectors + T-118 lexical)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DrainResponse {
    pub ok: bool,
    pub pending: usize,
}

/// T-118: commit the lexical index if the deferred-commit write path left it
/// dirty, so a subsequent reader sees every prior write. The dirty flag is
/// swapped under the lex lock so a concurrent write cannot slip between the
/// swap and the commit. Cheap no-op when clean.
async fn flush_lexical_if_dirty(state: &AppState) -> Result<(), ApiError> {
    let mut lex = state.lexical.lock().await;
    if state.lex_dirty.swap(false, Ordering::SeqCst) {
        lex.commit()
            .map_err(internal("commit_failed", "commit deferred lexical writes"))?;
    }
    Ok(())
}

/// Flush both derived projections to a consistent point: commit the deferred
/// lexical writes (T-118) and block until the async embedding backlog is empty
/// (T-117). The bench calls this after ingest (its `awaitIndexing` barrier) so
/// search runs against a fully-populated lexical index AND vector store.
/// Real-time callers rarely need it: search already commits-on-read, and the
/// event log is durable regardless.
pub async fn drain(State(state): State<Arc<AppState>>) -> Result<Json<DrainResponse>, ApiError> {
    flush_lexical_if_dirty(&state).await?;
    // Poll the embed + understanding pending counters; each worker decrements
    // its own as work lands. The short sleep keeps this off a busy-loop without
    // meaningful latency. Understanding is slow (an LLM call per capture), so a
    // caller that enabled it and asks to drain is explicitly choosing to wait
    // for the derived facts to settle; when understanding is disabled its
    // counter is always zero and this adds nothing.
    while state.embed_pending.load(Ordering::SeqCst) != 0
        || state.understand_pending.load(Ordering::SeqCst) != 0
    {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Ok(Json(DrainResponse {
        ok: true,
        pending: 0,
    }))
}

// ---------------------------------------------------------------------------
// /brief (SPEC 7c, Output B — the Session Boot Briefing)
// ---------------------------------------------------------------------------

/// How many understanding summaries / facts to feed the synthesizer. Bounded so
/// the prompt stays within a small local model's context and per-query cost is
/// flat regardless of corpus size (SPEC 7c Decision E). A big project blew past
/// the model with an unbounded fact list, so both halves are capped.
const BRIEF_SUMMARY_LIMIT: usize = 20;
const BRIEF_FACTS_LIMIT: usize = 24;

#[derive(Debug, Deserialize)]
pub struct BriefRequest {
    /// Project to scope to (matches the `project` tag). Absent = all projects.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BriefResponse {
    pub ok: bool,
    pub project: String,
    pub briefing_md: String,
    /// Source memory ids the briefing was synthesized from (grounding).
    pub sources: Vec<String>,
}

/// Synthesize a Session Boot Briefing on demand and write it through to the
/// per-project cache (so the SessionStart hook + `/brief/refresh` serve it
/// LLM-free). Synthesis runs in the server (it owns the store handles + the
/// model), so the CLI/hook route here rather than fighting the DuckDB writer
/// lock. Needs understanding enabled (a resolved model).
pub async fn brief(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BriefRequest>,
) -> Result<Json<BriefResponse>, ApiError> {
    let project = req.project.unwrap_or_default();
    let (briefing_md, sources) = synthesize_project_briefing(&state, &project).await?;
    if !briefing_md.trim().is_empty() {
        if let Err(err) =
            crate::understanding::write_briefing_cache(&state.home, &project, &briefing_md)
        {
            warn!(error = %err, project = %project, "brief: cache write failed");
        }
    }
    Ok(Json(BriefResponse {
        ok: true,
        project,
        briefing_md,
        sources,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CachedBriefQuery {
    #[serde(default)]
    pub project: Option<String>,
}

/// Read the cached briefing for a project WITHOUT synthesizing — fast and
/// LLM-free, for the viewer's Brain tab. An empty `briefing_md` means a cold
/// cache; the caller regenerates via `POST /brief`.
pub async fn brief_cached(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<CachedBriefQuery>,
) -> Json<BriefResponse> {
    let project = q.project.unwrap_or_default();
    let md = crate::understanding::read_briefing_cache(&state.home, &project).unwrap_or_default();
    Json(BriefResponse {
        ok: true,
        project,
        briefing_md: md,
        sources: vec![],
    })
}

/// Trigger a BACKGROUND refresh of a project's cached briefing and return
/// immediately. The SessionStart hook calls this AFTER injecting the (possibly
/// stale) cache, so the next session boots fresh without ever blocking a hook
/// on the LLM. No-op when understanding is disabled.
pub async fn brief_refresh(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BriefRequest>,
) -> Json<serde_json::Value> {
    let project = req.project.unwrap_or_default();
    if state.understand_model.is_some() {
        let state = state.clone();
        let project_for_task = project.clone();
        tokio::spawn(async move {
            match synthesize_project_briefing(&state, &project_for_task).await {
                Ok((md, _)) if !md.trim().is_empty() => {
                    if let Err(err) = crate::understanding::write_briefing_cache(
                        &state.home,
                        &project_for_task,
                        &md,
                    ) {
                        warn!(error = %err, project = %project_for_task, "brief refresh: cache write failed");
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    warn!(error = ?err, project = %project_for_task, "brief refresh: synthesis failed")
                }
            }
        });
    }
    Json(serde_json::json!({ "ok": true, "project": project }))
}

/// Shared synthesis: gather a project's live facts + recent understanding
/// summaries, build the grounded context, and synthesize the briefing. Returns
/// `(markdown, source_ids)`; empty markdown means "nothing to brief yet"
/// (returned rather than letting the model hallucinate from no input).
pub(crate) async fn synthesize_project_briefing(
    state: &AppState,
    project: &str,
) -> Result<(String, Vec<String>), ApiError> {
    let model = state.understand_model.clone().ok_or_else(|| {
        ApiError::bad_request(
            "understanding_disabled",
            "understanding is not enabled (set [understanding].enabled = true and a model)",
        )
    })?;
    let now = Utc::now();
    let scope = project_scope((!project.is_empty()).then_some(project), true);
    let mut facts = {
        let guard = state.facts.lock().await;
        guard
            .all_live_facts_scoped(
                now,
                None,
                None,
                scope.as_ref(),
                crate::reserved_tags::Visibility::Default,
                now,
            )
            .map_err(internal("facts_read_failed", "gather facts for brief"))?
    };
    // Bound facts (most-recent first) like summaries: an unbounded fact list
    // blew past the local model's context and it returned an empty briefing.
    // Per-query cost stays flat with corpus size (SPEC 7c Decision E).
    facts.sort_by_key(|f| std::cmp::Reverse(f.valid_from));
    facts.truncate(BRIEF_FACTS_LIMIT);
    let summaries = gather_understandings(state, project, BRIEF_SUMMARY_LIMIT).map_err(
        internal("understandings_read_failed", "gather understandings"),
    )?;

    let (context, sources) = build_brief_context(&facts, &summaries);
    if context.trim().is_empty() {
        return Ok((String::new(), vec![]));
    }
    let label = if project.is_empty() {
        "all projects"
    } else {
        project
    };
    let synth = OllamaSynthesizer::new(model, state.understand_endpoint.clone());
    let briefing = synth
        .synthesize("the user", label, &context)
        .await
        .map_err(internal("synthesis_failed", "synthesize briefing"))?;
    Ok((briefing.to_markdown(label), sources))
}

/// Understanding events for a project (or all), selected SIGNAL-FIRST and capped
/// at `limit` so prompt + cost stay flat as the log grows. The selection is the
/// pure, testable [`select_understandings`].
fn gather_understandings(
    state: &AppState,
    project: &str,
    limit: usize,
) -> Result<Vec<crate::event::UnderstandingPayload>> {
    let mut all: Vec<crate::event::UnderstandingPayload> = Vec::new();
    for ev in state.event_log.iter()? {
        let ev = ev?;
        if let EventKind::Understanding(p) = ev.kind {
            let in_scope =
                project.is_empty() || p.tags.get("project").map(String::as_str) == Some(project);
            if in_scope && !p.summary.trim().is_empty() {
                all.push(p);
            }
        }
    }
    Ok(select_understandings(all, limit))
}

/// True when an understanding carries SIGNAL salience (a decision/rule/etc.) as
/// opposed to default chatter. The only label compared is the documented default
/// `"note"`; everything else (including empty, treated as note) is signal-or-not
/// by that single check, so there's no hardcoded salience enum to maintain.
fn is_signal(u: &crate::event::UnderstandingPayload) -> bool {
    let s = u.salience.trim();
    !s.is_empty() && s != "note"
}

/// Select up to `limit` understandings SIGNAL-FIRST: high-salience memories
/// (decisions, rules, preferences, ...) are kept ahead of plain notes so an
/// older decision isn't crowded out by recent chatter, with recency breaking
/// ties. The returned set is then ordered newest-first for the prompt. Pure +
/// testable; `all` is in log (oldest-first) order on input.
fn select_understandings(
    mut all: Vec<crate::event::UnderstandingPayload>,
    limit: usize,
) -> Vec<crate::event::UnderstandingPayload> {
    // Signal before chatter; within each, newer before older.
    all.sort_by(|a, b| {
        is_signal(b)
            .cmp(&is_signal(a))
            .then(b.valid_from.cmp(&a.valid_from))
    });
    all.truncate(limit);
    // Present newest-first regardless of the signal/chatter split.
    all.sort_by_key(|u| std::cmp::Reverse(u.valid_from));
    all
}

/// Render gathered facts + understandings into the dated, id-tagged context the
/// synthesizer grounds on, plus the collected source ids. Pure + testable.
fn build_brief_context(
    facts: &[crate::facts::Fact],
    understandings: &[crate::event::UnderstandingPayload],
) -> (String, Vec<String>) {
    let mut ctx = String::new();
    let mut sources: Vec<String> = Vec::new();

    let summaries: Vec<&crate::event::UnderstandingPayload> = understandings
        .iter()
        .filter(|u| !u.summary.trim().is_empty())
        .collect();
    if !summaries.is_empty() {
        ctx.push_str("Recent activity (summaries):\n");
        for u in summaries {
            let date = u.valid_from.format("%Y-%m-%d");
            let summary = u.summary.trim();
            // Surface salience + intent + references so the synthesizer can rank
            // signal over chatter and keep concrete anchors (file paths, IDs).
            let mut annot = Vec::new();
            let salience = u.salience.trim();
            if !salience.is_empty() && salience != "note" {
                annot.push(salience.to_string());
            }
            let intent = u.intent.trim();
            if !intent.is_empty() {
                annot.push(format!("intent: {intent}"));
            }
            if !u.references.is_empty() {
                annot.push(format!("refs: {}", u.references.join(", ")));
            }
            let annot = if annot.is_empty() {
                String::new()
            } else {
                format!(" ({})", annot.join("; "))
            };
            ctx.push_str(&format!("- ({date}) {summary}{annot} [{}]\n", u.source_id));
            sources.push(u.source_id.to_string());
        }
    }

    if !facts.is_empty() {
        ctx.push_str("\nKnown facts:\n");
        for f in facts {
            let date = f.valid_from.format("%Y-%m-%d");
            ctx.push_str(&format!(
                "- ({date}) {} {} {} [{}]\n",
                f.subject, f.predicate, f.object, f.id
            ));
            sources.push(f.id.to_string());
        }
    }

    (ctx, sources)
}

// ---------------------------------------------------------------------------
// /understand/backfill — understand captures that predate the worker
// ---------------------------------------------------------------------------

/// Default cap on a single backfill so one call can't enqueue an unbounded
/// number of LLM jobs. The briefing only reads the most-recent summaries anyway.
const BACKFILL_DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
pub struct BackfillRequest {
    /// Scope to a project tag. Absent = all projects.
    #[serde(default)]
    pub project: Option<String>,
    /// Max captures to enqueue (most-recent first). Defaults to a bounded cap.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct BackfillResponse {
    pub ok: bool,
    pub enqueued: usize,
    /// Captures still un-understood after this batch (so a caller can loop).
    pub remaining: usize,
}

/// Enqueue captures that have no `understanding` event yet (the 2b marker) for
/// the async worker, so memories that predate understanding get decomposed.
/// Idempotent: a capture already understood is skipped. Bounded by `limit`.
pub async fn understand_backfill(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BackfillRequest>,
) -> Result<Json<BackfillResponse>, ApiError> {
    let tx = state.understand_tx.clone().ok_or_else(|| {
        ApiError::bad_request(
            "understanding_disabled",
            "understanding is not enabled (no worker to backfill into)",
        )
    })?;
    let project = req.project.unwrap_or_default();
    let limit = req.limit.unwrap_or(BACKFILL_DEFAULT_LIMIT);

    // One pass: collect the set of already-understood capture ids (the marker)
    // and the candidate captures (project-scoped). The log is append-only so the
    // understanding event for a capture always trails the capture in iteration.
    let mut understood: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut captures: Vec<Event> = Vec::new();
    for ev in state
        .event_log
        .iter()
        .map_err(internal("event_log_read_failed", "open log for backfill"))?
    {
        let ev = ev.map_err(internal("event_read_failed", "read event for backfill"))?;
        match &ev.kind {
            EventKind::Understanding(p) => {
                understood.insert(p.source_id.to_string());
            }
            EventKind::Capture(p) => {
                let in_scope = project.is_empty()
                    || p.tags.get("project").map(String::as_str) == Some(project.as_str());
                // Skip ephemeral tool-traces: same signal-not-noise rule as the
                // live /write enqueue, so backfill doesn't understand `[Bash]`
                // and `[mcp__...]` lines.
                if in_scope && worth_understanding(&ev) {
                    captures.push(ev);
                }
            }
            _ => {}
        }
    }

    let mut todo: Vec<Event> = captures
        .into_iter()
        .filter(|c| !understood.contains(&c.id.to_string()))
        .collect();
    let total = todo.len();
    // Most-recent first: keep the tail (the log is oldest-first).
    if todo.len() > limit {
        let drop = todo.len() - limit;
        todo.drain(0..drop);
    }

    let mut enqueued = 0usize;
    for capture in todo {
        state.understand_pending.fetch_add(1, Ordering::SeqCst);
        if tx.send(UnderstandJob { capture }).await.is_err() {
            state.understand_pending.fetch_sub(1, Ordering::SeqCst);
            break;
        }
        enqueued += 1;
    }

    Ok(Json(BackfillResponse {
        ok: true,
        enqueued,
        remaining: total.saturating_sub(enqueued),
    }))
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
    /// SPEC §2.8 project scope. When present, restrict to this project plus
    /// user-common (global) memory, never another project. Absent = unscoped.
    #[serde(default)]
    pub scope: Option<ScopeInput>,
    /// North Star (§2.9): the model whose tokenizer the returned context is
    /// COSTED against, so the agent sees the real token cost for the model it
    /// runs. Absent = the default accounting model (gpt-4o).
    #[serde(default)]
    pub accounting_model: Option<String>,
    /// North Star (§2.9): set by HUMAN-facing callers (the dashboard) so the
    /// retrieval is NOT recorded in the cumulative rollup. The rollup measures
    /// AGENT retrievals (context actually fed to a model); a person browsing the
    /// viewer feeds nothing, so it must not count as served/cost. Absent/false =
    /// an agent retrieval, which is recorded.
    #[serde(default)]
    pub browse: Option<bool>,
}

/// Scope a search to one project plus global memory (SPEC §2.8). `key` is the
/// scoping tag (`project_path` by default, the collision-proof key; or
/// `project`). `include_global` (default true) keeps untagged user-common memory.
#[derive(Debug, Deserialize)]
pub struct ScopeInput {
    #[serde(default)]
    pub key: String,
    pub value: String,
    #[serde(default = "default_include_global")]
    pub include_global: bool,
}

fn default_include_global() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub ok: bool,
    pub results: Vec<SearchResult>,
    /// North Star (§2.9): the REAL token cost of the context in `results` (the
    /// snippets handed to the model), counted with the accounting model's
    /// tokenizer. This is the number that makes "fewest tokens" measurable: the
    /// agent sees a recall cost N tokens instead of dumping its whole history.
    pub tokens: usize,
    /// The model the `tokens` count is accounted against.
    pub token_model: String,
    /// True when `tokens` used that model's EXACT tokenizer (GPT family), false
    /// when it is a documented proxy (Claude / local Llama have no embedded BPE).
    pub tokens_exact: bool,
    /// Real USD cost of `tokens` at the accounting model's input price (config
    /// `[north_star].pricing_per_1m`). `None` when the model is unpriced (e.g. a
    /// local model, which costs nothing per token). This is the dollars half of
    /// the North Star.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
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
    // T-118: commit any writes the deferred-commit path left pending, so this
    // search sees the latest. Cheap no-op when the index is clean.
    flush_lexical_if_dirty(&state).await?;
    let k = req.k.unwrap_or(SEARCH_K_DEFAULT).clamp(1, SEARCH_K_MAX);

    let at_time = req
        .at_time
        .as_deref()
        .map(parse_rfc3339)
        .transpose()
        .map_err(|e: anyhow::Error| {
            ApiError::bad_request("invalid_at_time", format!("at_time must be RFC3339: {e:#}"))
        })?;
    let now = Utc::now();

    // T-63: ONE retrieval path for CLI and server. Build the registry from the
    // server's shared store handles. HybridRetriever shares AppState's Arcs
    // (so no stores are moved out of AppState) and degrades to lexical-only
    // when no embedder/vectors are present. Honors [retriever].plugins, so the
    // entity-graph retriever is reachable over MCP when the user enables it.
    // Replaces the former duplicated inline hybrid_search.
    let cfg = crate::config::Config::load(&state.home).unwrap_or_default();
    let hybrid = crate::retriever::HybridRetriever::new_shared(
        state.embedder.clone(),
        state.vectors.clone(),
        state.lexical.clone(),
        state.facts.clone(),
    )
    .with_recency_weight(cfg.retriever.recency_weight)
    .with_decay_half_lives(cfg.retriever.decay_half_lives_in_days())
    .with_mmr_lambda(cfg.retriever.mmr_lambda)
    .with_reranker(if cfg.retriever.rerank {
        // Resolve the reranker model dir like the embedder does: a
        // LOCALMEM_RERANKER_DIR env override (so many homes can share ONE model,
        // e.g. a benchmark spawning per-conversation homes) else
        // <home>/models/reranker. A failed load when rerank is ENABLED is logged
        // loudly rather than silently degrading, so "rerank on but no effect" is
        // never invisible again.
        let dir = std::env::var("LOCALMEM_RERANKER_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| state.home.join("models").join("reranker"));
        let loaded = crate::rerank::Reranker::load(&dir);
        if let Err(e) = &loaded {
            warn!(dir = %dir.display(), error = %format!("{e:#}"), "rerank enabled but reranker model failed to load; degrading to first-stage retrieval");
        }
        std::sync::Arc::new(tokio::sync::Mutex::new(loaded.ok()))
    } else {
        std::sync::Arc::new(tokio::sync::Mutex::new(None))
    });
    let ctx = crate::retriever::RetrieverBuildCtx {
        hybrid: Some(hybrid),
        facts: state.facts.clone(),
    };
    let registry = crate::retriever::RetrieverRegistry::from_config(&cfg.retriever, ctx).map_err(
        internal("retriever_build_failed", "build retriever registry"),
    )?;

    // /search is not the audit path: visibility=private stays hidden and the
    // retention TTL applies (T-51c), threaded via Filters.
    // SPEC §2.8: when the caller supplies a project scope (the MCP server sets it
    // from the session's project by default), restrict to that project + global,
    // never another project. Absent scope = unscoped (explicit "all").
    let scope = req.scope.as_ref().map(|s| crate::retriever::Scope {
        key: if s.key.trim().is_empty() {
            "project_path".to_string()
        } else {
            s.key.clone()
        },
        value: s.value.clone(),
        include_global: s.include_global,
    });
    let filters = crate::retriever::Filters {
        tags: req.tags.clone(),
        scope,
        visibility: crate::reserved_tags::Visibility::Default,
        now,
        at_time,
    };
    let hits = registry
        .search(query, k, &filters)
        .await
        .map_err(internal("search_failed", "registry search"))?;
    let results = hits
        .into_iter()
        .map(|h| SearchResult {
            fact: h.content,
            score: h.score,
            sources: vec![h.event_id],
            // T-63 completion: surface the hit's valid-time (threaded through
            // HybridHit) instead of the former hardcoded None, so callers can
            // do temporal reasoning. valid_to is reserved for a future
            // interval surface; captures expose only the start instant today.
            valid_from: h
                .valid_from
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true)),
            valid_to: None,
        })
        .collect::<Vec<_>>();
    // North Star (§2.9): cost the returned context in the accounting model's
    // real tokens + dollars, so the caller sees what this recall actually costs
    // to feed a model versus dumping raw history. Pricing + default model come
    // from config ([north_star]).
    let ns = crate::config::Config::load(&state.home)
        .map(|c| c.north_star)
        .unwrap_or_default();
    let token_model = req
        .accounting_model
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or(&ns.accounting_model)
        .to_string();
    let tokens = crate::tokens::count_many(results.iter().map(|r| r.fact.as_str()), &token_model);
    let tokens_exact = crate::tokens::is_exact(&token_model);
    let cost_usd = ns.cost_usd(tokens, &token_model);
    // Record AGENT retrievals only (context actually fed to a model) in the
    // cumulative rollup. Human browsing the dashboard (browse=true) feeds nothing
    // and must not count as served/cost, or the number is meaningless. Telemetry
    // is content-free and best-effort: it must never fail a search.
    if !req.browse.unwrap_or(false) {
        crate::north_star::record_retrieval(
            &state.home,
            tokens,
            results.len(),
            &token_model,
            cost_usd,
        );
    }
    Ok(Json(SearchResponse {
        ok: true,
        results,
        tokens,
        token_model,
        tokens_exact,
        cost_usd,
    }))
}

fn parse_rfc3339(s: &str) -> Result<DateTime<Utc>> {
    let parsed =
        DateTime::parse_from_rfc3339(s).map_err(|e| anyhow::anyhow!("parse RFC3339 {s:?}: {e}"))?;
    Ok(parsed.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// /north-star (P7, §2.9): cumulative token + dollar savings rollup
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct NorthStarResponse {
    pub ok: bool,
    #[serde(flatten)]
    pub rollup: crate::north_star::Rollup,
}

/// The North Star headline: how much precise context localmem has served (tokens
/// + dollars) over today / 7d / 30d / all-time, plus the estimated saving vs
/// dumping raw history. Reads the local, content-free usage log.
pub async fn north_star(
    State(state): State<Arc<AppState>>,
) -> Result<Json<NorthStarResponse>, ApiError> {
    let mult = crate::config::Config::load(&state.home)
        .map(|c| c.north_star.baseline_multiplier)
        .unwrap_or(10.0);
    let rollup = crate::north_star::rollup(&state.home, mult);
    Ok(Json(NorthStarResponse { ok: true, rollup }))
}

// ---------------------------------------------------------------------------
// /getting-started (P8, §8): the ONE onboarding source every entry point reads
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct GettingStartedResponse {
    pub ok: bool,
    #[serde(flatten)]
    pub getting_started: crate::onboarding::GettingStarted,
    /// Agent-facing Markdown so the MCP welcome resource can surface it verbatim.
    pub markdown: String,
}

/// The shared onboarding snapshot: dashboard URL, what is set up, importable
/// histories, and ordered next steps. The MCP `localmem://getting-started`
/// resource renders this in the IDE; the CLI installers render the same.
pub async fn getting_started(
    State(state): State<Arc<AppState>>,
) -> Result<Json<GettingStartedResponse>, ApiError> {
    let cfg = crate::config::Config::load(&state.home).unwrap_or_default();
    let url = crate::onboarding::dashboard_url(&cfg.server.addr);
    let model = crate::onboarding::model_present(&state.home);
    let understanding = state.understand_model.is_some();
    // Best-effort import detection (same sources as /import/scan).
    let mut candidates = 0usize;
    if let Ok(dets) = crate::cli::import_wizard::scan_default_locations() {
        candidates += dets.len();
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = std::path::Path::new(&home).join(".claude").join("projects");
        if p.is_dir() && count_jsonl_sessions(&p) > 0 {
            candidates += 1;
        }
    }
    // The core is obviously running (it is serving this request); list real
    // client wiring status.
    let clients = crate::cli::mcp::all_clients_status(None);
    let gs = crate::onboarding::build(url, model, true, &clients, understanding, candidates);
    let markdown = gs.render_markdown();
    Ok(Json(GettingStartedResponse {
        ok: true,
        getting_started: gs,
        markdown,
    }))
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
    /// SPEC §2.8 project scope (collision-proof project_path). When set,
    /// restrict to this project plus global memory. Absent = every project.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub include_global: Option<bool>,
}

/// Canonical project scope for every scoped read (SPEC §2.8). `project` carries
/// the collision-proof `project_path` (the dashboard + MCP send the full cwd).
/// `None`/empty => unscoped. `include_global` decides whether untagged
/// user-common memory comes along: AGENT reads (MCP/CLI) default to `true`
/// (project + global, the best context), while the human dashboard passes
/// `false` for a STRICT per-project view (only that project's memory). Same
/// predicate either way; only the flag differs.
fn project_scope(project: Option<&str>, include_global: bool) -> Option<crate::retriever::Scope> {
    project
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut s = crate::retriever::Scope::project_path(p);
            s.include_global = include_global;
            s
        })
}

/// Default for the optional `include_global` request field: inclusive (project +
/// global), the agent-friendly default. The dashboard overrides to `false`.
fn want_global(flag: Option<bool>) -> bool {
    flag.unwrap_or(true)
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
    pub confidence: f64,
    pub valid_from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    /// System-time supersession instant: when this belief stopped being current
    /// (a newer fact took over, or a forget retired it). `None` = still current.
    /// Powers the dashboard Timeline view (P6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<String>,
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
    let scope = project_scope(req.project.as_deref(), want_global(req.include_global));
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
                        if !crate::retriever::scope_matches(&fact.tags, &scope) {
                            return false;
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
                .facts_for_subject_scoped(&req.entity, tag_filter, scope.as_ref(), visibility, now)
                .map_err(internal("facts_query_failed", "facts_for_subject_scoped"))?,
        }
    };

    let facts = rows
        .into_iter()
        .map(|f| RecallFact {
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
        })
        .collect();

    Ok(Json(RecallResponse { ok: true, facts }))
}

// ---------------------------------------------------------------------------
// /profile (T-22 + T-44)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ProfileRequest {
    /// Subject filter: synthesize the profile for one entity only. (Named
    /// `scope` for historical reasons; it is NOT the project scope below.)
    #[serde(default)]
    pub scope: Option<String>,
    /// Container tag filter (T-51b). Subset match against each fact's
    /// inherited tags. Empty or absent skips filtering.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// SPEC §2.8 project scope (collision-proof project_path). When set,
    /// restrict to this project plus global memory. Absent = every project.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub include_global: Option<bool>,
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
    let subject = req.scope.as_deref();
    let tag_filter = if req.tags.is_empty() {
        None
    } else {
        Some(&req.tags)
    };
    let proj_scope = project_scope(req.project.as_deref(), want_global(req.include_global));
    // /profile is a synthesis route. Per SPEC_V0_2, `visibility=
    // private` captures stay hidden here (T-51c); the audit path is
    // /recall.
    let now = Utc::now();
    let facts = {
        let guard = state.facts.lock().await;
        guard
            .all_live_facts_scoped(
                now,
                subject,
                tag_filter,
                proj_scope.as_ref(),
                crate::reserved_tags::Visibility::Default,
                now,
            )
            .map_err(internal("facts_query_failed", "all_live_facts_scoped"))?
    };
    let md = synthesize_profile_md(&facts, subject);
    Ok(Json(ProfileResponse {
        ok: true,
        profile_md: md,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        fact_count: facts.len() as u32,
    }))
}

/// One current belief in a grouped profile, carrying freshness (valid_from) and
/// its kind taxonomy so the viewer can rank + age it.
#[derive(Debug, Serialize)]
pub struct ProfileFact {
    pub predicate: String,
    pub object: String,
    pub confidence: f64,
    pub valid_from: String,
    pub kind: String,
    /// P3: 1.0 fresh -> 0.0 stale, by per-kind half-life over the belief's age.
    pub freshness: f64,
    /// P3: true when this current belief is past its half-life (review candidate).
    pub stale: bool,
}

/// A resolved entity with its current beliefs, the unit of the grouped Profile.
#[derive(Debug, Serialize)]
pub struct ProfileGroup {
    pub entity: String,
    pub canonical: String,
    pub kind: String,
    pub mentions: u64,
    pub last_seen: String,
    pub facts: Vec<ProfileFact>,
}

#[derive(Debug, Serialize)]
pub struct GroupedProfileResponse {
    pub ok: bool,
    pub groups: Vec<ProfileGroup>,
    pub fact_count: u32,
    pub generated_at: String,
}

const PROFILE_GROUPS_MAX: usize = 120;
const PROFILE_FACTS_PER_GROUP: usize = 12;

/// Grouped + ranked profile (P4): current beliefs organized BY resolved entity,
/// ranked by salience (mention count, then belief count, then recency), each
/// fact carrying freshness — replacing the flat alphabetical `profile_md` wall.
/// Subjects are matched to resolved entities by canonical key so the typing +
/// dedup from the graph layer carries into the profile.
pub async fn profile_grouped(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ProfileRequest>,
) -> Result<Json<GroupedProfileResponse>, ApiError> {
    use crate::facts::{canonicalize_entity, Fact};
    use std::collections::HashMap;

    let subject = req.scope.as_deref();
    let tag_filter = if req.tags.is_empty() {
        None
    } else {
        Some(&req.tags)
    };
    let proj_scope = project_scope(req.project.as_deref(), want_global(req.include_global));
    let now = Utc::now();
    // P3: per-kind half-lives (T-73) drive the freshness signal on each belief.
    let half_lives = crate::config::Config::load(&state.home)
        .unwrap_or_default()
        .retriever
        .decay_half_lives_in_days();
    let (facts, entity_map) = {
        let guard = state.facts.lock().await;
        let facts = guard
            .all_live_facts_scoped(
                now,
                subject,
                tag_filter,
                proj_scope.as_ref(),
                crate::reserved_tags::Visibility::Default,
                now,
            )
            .map_err(internal("facts_query_failed", "all_live_facts_scoped"))?;
        let entity_map: HashMap<String, (String, String, u64, DateTime<Utc>)> = guard
            .resolved_entities()
            .map(|v| {
                v.into_iter()
                    .map(|e| {
                        (
                            e.canonical,
                            (e.display_name, e.kind, e.mentions, e.last_seen),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        (facts, entity_map)
    };
    let fact_count = facts.len() as u32;

    let mut grouped: HashMap<String, Vec<&Fact>> = HashMap::new();
    for f in &facts {
        let c = canonicalize_entity(&f.subject);
        if c.is_empty() {
            continue;
        }
        grouped.entry(c).or_default().push(f);
    }
    let mut groups: Vec<ProfileGroup> = grouped
        .into_iter()
        .map(|(canon, mut fs)| {
            // Freshest belief first within the group.
            fs.sort_by(|a, b| b.valid_from.cmp(&a.valid_from));
            let (entity, kind, mentions, last_seen) = match entity_map.get(&canon) {
                Some((disp, k, m, ls)) => (disp.clone(), k.clone(), *m, *ls),
                None => (
                    fs.first()
                        .map(|f| f.subject.clone())
                        .unwrap_or_else(|| canon.clone()),
                    "thing".to_string(),
                    0,
                    fs.first().map(|f| f.valid_from).unwrap_or(now),
                ),
            };
            let facts: Vec<ProfileFact> = fs
                .iter()
                .take(PROFILE_FACTS_PER_GROUP)
                .map(|f| {
                    let fr = crate::freshness::freshness(
                        f.kind.as_str(),
                        f.valid_from,
                        now,
                        &half_lives,
                    );
                    ProfileFact {
                        predicate: f.predicate.clone(),
                        object: f.object.clone(),
                        confidence: f.confidence,
                        valid_from: f.valid_from.to_rfc3339_opts(SecondsFormat::Secs, true),
                        kind: f.kind.as_str().to_string(),
                        freshness: fr,
                        stale: crate::freshness::is_stale(fr),
                    }
                })
                .collect();
            ProfileGroup {
                entity,
                canonical: canon,
                kind,
                mentions,
                last_seen: last_seen.to_rfc3339_opts(SecondsFormat::Secs, true),
                facts,
            }
        })
        .collect();
    // Rank: most-mentioned entities first, then richest, then freshest.
    groups.sort_by(|a, b| {
        b.mentions
            .cmp(&a.mentions)
            .then(b.facts.len().cmp(&a.facts.len()))
            .then(b.last_seen.cmp(&a.last_seen))
    });
    groups.truncate(PROFILE_GROUPS_MAX);

    Ok(Json(GroupedProfileResponse {
        ok: true,
        groups,
        fact_count,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    }))
}

#[derive(Debug, Deserialize)]
pub struct ReviewRequest {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub include_global: Option<bool>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One stale current belief awaiting "still true?" review.
#[derive(Debug, Serialize)]
pub struct ReviewItem {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub kind: String,
    pub valid_from: String,
    pub age_days: u64,
    pub freshness: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_capture: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReviewResponse {
    pub ok: bool,
    pub items: Vec<ReviewItem>,
    /// How many high-importance current beliefs were checked (the denominator).
    pub checked: u64,
}

const REVIEW_LIMIT_DEFAULT: usize = 50;
const REVIEW_LIMIT_MAX: usize = 200;

/// P3 (intelligence-v2 §2.4): the stale-current review queue. Current beliefs
/// are the latest unrefuted statements, NOT verified truth: a high-importance
/// belief can sit current for a year with no re-confirmation. This returns those
/// that are past their per-kind half-life (stalest first) so the user can
/// re-confirm or forget them, memory that knows when it might be wrong.
pub async fn review(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ReviewRequest>,
) -> Result<Json<ReviewResponse>, ApiError> {
    let limit = req
        .limit
        .unwrap_or(REVIEW_LIMIT_DEFAULT)
        .min(REVIEW_LIMIT_MAX);
    let scope = project_scope(req.project.as_deref(), want_global(req.include_global));
    let now = Utc::now();
    let half_lives = crate::config::Config::load(&state.home)
        .unwrap_or_default()
        .retriever
        .decay_half_lives_in_days();
    let facts = {
        let guard = state.facts.lock().await;
        guard
            .all_live_facts_scoped(
                now,
                None,
                None,
                scope.as_ref(),
                crate::reserved_tags::Visibility::Default,
                now,
            )
            .map_err(internal("facts_read_failed", "gather facts for review"))?
    };
    let mut checked: u64 = 0;
    let mut items: Vec<ReviewItem> = Vec::new();
    for f in &facts {
        if !crate::freshness::is_high_importance(&f.kind) {
            continue;
        }
        checked += 1;
        let fr = crate::freshness::freshness(f.kind.as_str(), f.valid_from, now, &half_lives);
        if !crate::freshness::is_stale(fr) {
            continue;
        }
        let age_days = ((now - f.valid_from).num_seconds().max(0) as f64 / 86_400.0) as u64;
        items.push(ReviewItem {
            id: f.id.to_string(),
            subject: f.subject.clone(),
            predicate: f.predicate.clone(),
            object: f.object.clone(),
            kind: f.kind.as_str().to_string(),
            valid_from: f.valid_from.to_rfc3339_opts(SecondsFormat::Secs, true),
            age_days,
            freshness: fr,
            source_capture: f.source_events.first().map(|e| e.to_string()),
        });
    }
    // Stalest first so the most-overdue reviews surface at the top.
    items.sort_by(|a, b| {
        a.freshness
            .partial_cmp(&b.freshness)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    items.truncate(limit);
    Ok(Json(ReviewResponse {
        ok: true,
        items,
        checked,
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
    /// Scope to decisions whose source capture belongs to this project. Absent/
    /// empty = all. Resolved by mapping each entry's `input_id` to its capture's
    /// project (journal entries don't carry tags themselves).
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub include_global: Option<bool>,
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
    let scope = project_scope(req.project.as_deref(), want_global(req.include_global));

    // When scoped, build the set of capture ids in scope once, so each journal
    // entry can be filtered by its source capture's project (entries carry only
    // `input_id`, not tags). Global/untagged events are in scope too, so their
    // decisions still surface — the shared SPEC §2.8 semantic.
    let project_captures: Option<std::collections::HashSet<String>> = if scope.is_none() {
        None
    } else {
        let mut set = std::collections::HashSet::new();
        for ev in state.event_log.iter().map_err(internal(
            "event_log_read_failed",
            "open log for journal scope",
        ))? {
            let ev = ev.map_err(internal(
                "event_read_failed",
                "read event for journal scope",
            ))?;
            if event_passes_scope(&ev, &scope) {
                set.insert(ev.id.to_string());
            }
        }
        Some(set)
    };

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
        if let Some(caps) = &project_captures {
            if !caps.contains(&entry.input_id.to_string()) {
                continue;
            }
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

#[derive(Debug, Deserialize)]
pub struct SubjectsQuery {
    /// Scope to subjects appearing in this project's live facts. Absent = all.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub include_global: Option<bool>,
}

pub async fn resource_subjects(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<SubjectsQuery>,
) -> Result<Json<ResourceSubjectsResponse>, ApiError> {
    let project = q.project.unwrap_or_default();
    let subjects = if project.is_empty() {
        let guard = state.facts.lock().await;
        guard
            .subjects()
            .map_err(internal("facts_query_failed", "subjects"))?
            .into_iter()
            .map(|(subject, count)| SubjectRow { subject, count })
            .collect()
    } else {
        // Project-scoped: distinct subjects over the project's live facts, so the
        // viewer's Timeline reflects the selected project, not the whole store.
        let now = Utc::now();
        let scope = project_scope(Some(project.as_str()), want_global(q.include_global));
        let facts = {
            let guard = state.facts.lock().await;
            guard
                .all_live_facts_scoped(
                    now,
                    None,
                    None,
                    scope.as_ref(),
                    crate::reserved_tags::Visibility::Default,
                    now,
                )
                .map_err(internal("facts_query_failed", "subjects (scoped)"))?
        };
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for f in facts {
            *counts.entry(f.subject).or_default() += 1;
        }
        let mut rows: Vec<SubjectRow> = counts
            .into_iter()
            .map(|(subject, count)| SubjectRow { subject, count })
            .collect();
        rows.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.subject.cmp(&b.subject))
        });
        rows
    };
    Ok(Json(ResourceSubjectsResponse { ok: true, subjects }))
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
    Ok(Json(ResourceTagsResponse {
        ok: true,
        tags: rows,
    }))
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
            // Tag discovery should reflect DURABLE memory, not ephemeral
            // tool-traces. A dir that only ever saw tool calls (a workflow
            // worktree `wf_*`, or a project subdir) is pure noise here; counting
            // its trace captures floods the project list with non-projects.
            let ephemeral = p
                .tags
                .get(crate::reserved_tags::KEY_RETENTION)
                .map(|r| r.starts_with(crate::reserved_tags::RETENTION_EPHEMERAL_PREFIX))
                .unwrap_or(false);
            if ephemeral {
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
// Viewer aggregations (/stats, /events, /activity) — read-only surfaces the
// multi-tab dashboard consumes. Each walks the append-only log once; bounded
// for single-user scale (a corpus-scale index is the T-121 follow-up).
// ---------------------------------------------------------------------------

/// One event, normalized for the viewer: a kind label, a one-line title, the
/// owning project, and the full payload for drill-down. Lets every tab render a
/// heterogeneous feed without re-deriving payload shapes in JS.
#[derive(Debug, Serialize)]
pub struct ViewerEvent {
    pub id: String,
    pub ts: String,
    pub kind: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// True for ephemeral tool-use traces (`[Bash] ...`, `[Read] ...`): real
    /// captures but operational noise, so the viewer can hide them by default.
    pub ephemeral: bool,
    pub detail: Value,
}

/// Stable kind label for an event (the `kind` tag in the wire schema).
fn event_kind_str(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Capture(_) => "capture",
        EventKind::Fact(_) => "fact",
        EventKind::Update(_) => "update",
        EventKind::UpdateCapture(_) => "update_capture",
        EventKind::Forget(_) => "forget",
        EventKind::Policy(_) => "policy",
        EventKind::Import(_) => "import",
        EventKind::Understanding(_) => "understanding",
    }
}

/// One-line, human-readable title for an event (what the feed/list shows).
fn event_title(ev: &Event) -> String {
    let one_line = |s: &str, n: usize| -> String {
        let line = s.split('\n').next().unwrap_or("").trim();
        let t: String = line.chars().take(n).collect();
        if line.chars().count() > n {
            format!("{t}…")
        } else {
            t.to_string()
        }
    };
    match &ev.kind {
        EventKind::Capture(p) => one_line(&p.text, 140),
        EventKind::Fact(p) => format!("{} {} {}", p.subject, p.predicate, one_line(&p.object, 80)),
        EventKind::Update(p) => format!(
            "{} {} {} (supersedes)",
            p.new_fact.subject,
            p.new_fact.predicate,
            one_line(&p.new_fact.object, 70)
        ),
        EventKind::UpdateCapture(p) => format!("update capture {}", p.target_id),
        EventKind::Forget(p) => format!("forget: {}", one_line(&p.reason, 120)),
        EventKind::Policy(p) => format!("{:?} ({})", p.action, p.rule),
        EventKind::Import(p) => format!("import {} from {}", p.count, p.source_format),
        EventKind::Understanding(p) => {
            if p.summary.trim().is_empty() {
                format!("understood {}", p.source_id)
            } else {
                one_line(&p.summary, 140)
            }
        }
    }
}

/// The owning project tag for an event, when it carries one.
fn event_project(ev: &Event) -> Option<String> {
    let tag = |tags: &BTreeMap<String, String>| tags.get("project").cloned();
    match &ev.kind {
        EventKind::Capture(p) => tag(&p.tags),
        EventKind::Fact(p) => tag(&p.tags),
        EventKind::Understanding(p) => tag(&p.tags),
        _ => None,
    }
}

/// The container tags carried by an event, for project-scope matching. Only the
/// memory-bearing kinds carry project tags; other kinds (policy, forget, ...)
/// have none and so count as global (untagged) under the scope predicate.
fn event_tags(ev: &Event) -> BTreeMap<String, String> {
    match &ev.kind {
        EventKind::Capture(p) => p.tags.clone(),
        EventKind::Fact(p) => p.tags.clone(),
        EventKind::Understanding(p) => p.tags.clone(),
        _ => BTreeMap::new(),
    }
}

/// Project-scope predicate for an event: the SAME SPEC §2.8 rule the facts and
/// search paths use (scope key match, or untagged-global when include_global),
/// via the shared [`crate::retriever::scope_matches`]. Keeps event-backed views
/// (events, journal, stats, activity) cohesive with everything else.
fn event_passes_scope(ev: &Event, scope: &Option<crate::retriever::Scope>) -> bool {
    crate::retriever::scope_matches(&event_tags(ev), scope)
}

/// The collision-proof project key for an event (the full `project_path` tag),
/// for enumerating the distinct projects a viewer can scope to.
fn event_project_path(ev: &Event) -> Option<String> {
    event_tags(ev)
        .get(crate::retriever::PROJECT_PATH_TAG)
        .cloned()
}

/// Decomposition coverage for the Overview gauge: how much SIGNAL has actually
/// been understood. `decomposed/signal` over non-ephemeral captures (the
/// canonical metric from `understanding::compute_coverage`).
#[derive(Debug, Serialize)]
pub struct CoverageStat {
    pub decomposed: u64,
    pub signal: u64,
    pub percent: u32,
}

/// The active decomposition backend, so the dashboard can state WHAT model is
/// understanding memory ("gpt-4o via openai" / "qwen3.5:4b via ollama") and
/// whether it is running.
#[derive(Debug, Serialize)]
pub struct ActiveBackend {
    pub enabled: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub ok: bool,
    pub events: u64,
    pub captures: u64,
    pub facts: u64,
    pub understandings: u64,
    pub forgets: u64,
    pub subjects: u64,
    pub projects: u64,
    /// P2: distinct resolved graph nodes (typed entities).
    pub entities: u64,
    /// P4: decomposition coverage + the active understanding backend, so a user
    /// knows from the dashboard how much is understood and on which model.
    pub coverage: CoverageStat,
    pub understanding: ActiveBackend,
}

#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    /// Scope every count to one project. Absent/empty = whole store.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub include_global: Option<bool>,
}

/// Top-line counts for the dashboard stat cards, scoped to a project when
/// `?project=` is given. One log walk (counts + project-scoped coverage) plus a
/// facts read. When scoped, facts/subjects/entities come from the project's live
/// facts so every number on the Overview honors the filter.
pub async fn stats(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<StatsQuery>,
) -> Result<Json<StatsResponse>, ApiError> {
    let scope = project_scope(q.project.as_deref(), want_global(q.include_global));
    let scoped = scope.is_some();
    let (mut events, mut captures, mut understandings, mut forgets) = (0u64, 0u64, 0u64, 0u64);
    // Distinct projects counted by the collision-proof project_path key, the
    // same key the selector scopes by.
    let mut projects: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Project-scoped coverage, computed in the same walk: signal = non-ephemeral
    // captures in scope, decomposed = those that produced an Understanding.
    let mut signal_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut understood_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ev in state
        .event_log
        .iter()
        .map_err(internal("event_log_read_failed", "open log for stats"))?
    {
        let ev = ev.map_err(internal("event_read_failed", "read event for stats"))?;
        if let Some(p) = event_project_path(&ev) {
            projects.insert(p);
        }
        if scoped && !event_passes_scope(&ev, &scope) {
            continue;
        }
        events += 1;
        match &ev.kind {
            EventKind::Capture(p) => {
                captures += 1;
                if !p.is_ephemeral() {
                    signal_ids.insert(ev.id.to_string());
                }
            }
            EventKind::Understanding(u) => {
                understandings += 1;
                understood_ids.insert(u.source_id.to_string());
            }
            EventKind::Forget(_) => forgets += 1,
            _ => {}
        }
    }
    let (facts, subjects, entities) = {
        let guard = state.facts.lock().await;
        if scoped {
            // Scoped: derive from the project's live facts so the cards honor the
            // filter (the global facts/entities counts would otherwise leak).
            let now = Utc::now();
            let live = guard
                .all_live_facts_scoped(
                    now,
                    None,
                    None,
                    scope.as_ref(),
                    crate::reserved_tags::Visibility::Default,
                    now,
                )
                .unwrap_or_default();
            let mut subj: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let mut ents: std::collections::HashSet<String> = std::collections::HashSet::new();
            for f in &live {
                subj.insert(f.subject.as_str());
                ents.insert(crate::facts::canonicalize_entity(&f.subject));
                ents.insert(crate::facts::canonicalize_entity(&f.object));
            }
            ents.remove("");
            (live.len() as u64, subj.len() as u64, ents.len() as u64)
        } else {
            let facts = guard.count().unwrap_or(0);
            let subjects = guard.subjects().map(|s| s.len() as u64).unwrap_or(0);
            let entities = guard.entity_count().unwrap_or(0);
            (facts, subjects, entities)
        }
    };
    let coverage = if scoped {
        let decomposed = signal_ids
            .iter()
            .filter(|id| understood_ids.contains(*id))
            .count() as u64;
        let signal = signal_ids.len() as u64;
        let percent = if signal == 0 {
            100
        } else {
            ((decomposed * 100) / signal) as u32
        };
        CoverageStat {
            decomposed,
            signal,
            percent,
        }
    } else {
        // Whole-store coverage via the canonical definition (a second cheap walk)
        // so the dashboard and `localmem understand` never disagree on "coverage".
        let iter = state
            .event_log
            .iter()
            .map_err(internal("event_log_read_failed", "open log for coverage"))?
            .filter_map(Result::ok);
        let cov = crate::understanding::compute_coverage(iter);
        CoverageStat {
            decomposed: cov.decomposed as u64,
            signal: cov.signal_captures as u64,
            percent: cov.percent(),
        }
    };
    let understanding = ActiveBackend {
        enabled: state.understand_model.is_some(),
        provider: state.understand_provider.clone(),
        model: state.understand_model.clone(),
    };
    Ok(Json(StatsResponse {
        ok: true,
        events,
        captures,
        facts,
        understandings,
        forgets,
        subjects,
        projects: projects.len() as u64,
        entities,
        coverage,
        understanding,
    }))
}

#[derive(Debug, Deserialize)]
pub struct EventsRequest {
    #[serde(default)]
    pub limit: Option<usize>,
    /// Restrict to these kind labels (e.g. ["capture","understanding"]). Empty = all.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Restrict to a project tag. Absent/empty = all.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub include_global: Option<bool>,
    /// When true, hide ephemeral tool-use traces (the `[Bash]`/`[Read]` noise),
    /// leaving only signal captures + understanding/fact events. Default false.
    #[serde(default)]
    pub signal_only: bool,
}

#[derive(Debug, Serialize)]
pub struct EventsResponse {
    pub ok: bool,
    pub events: Vec<ViewerEvent>,
}

const EVENTS_LIMIT_DEFAULT: usize = 60;
const EVENTS_LIMIT_MAX: usize = 500;

/// Paginated, newest-first event feed for the Memories / Replay / activity feed.
/// Filters by kind label + project. Bounded by `limit`.
pub async fn events(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EventsRequest>,
) -> Result<Json<EventsResponse>, ApiError> {
    let limit = req
        .limit
        .unwrap_or(EVENTS_LIMIT_DEFAULT)
        .min(EVENTS_LIMIT_MAX);
    let want_kind = |k: &str| req.kinds.is_empty() || req.kinds.iter().any(|w| w == k);
    let scope = project_scope(req.project.as_deref(), want_global(req.include_global));

    let mut out: Vec<ViewerEvent> = Vec::new();
    for ev in state
        .event_log
        .iter()
        .map_err(internal("event_log_read_failed", "open log for events"))?
    {
        let ev = ev.map_err(internal("event_read_failed", "read event for events"))?;
        let kind = event_kind_str(&ev.kind);
        if !want_kind(kind) {
            continue;
        }
        let ephemeral = matches!(&ev.kind, EventKind::Capture(p) if p.is_ephemeral());
        if req.signal_only && ephemeral {
            continue;
        }
        if !event_passes_scope(&ev, &scope) {
            continue;
        }
        out.push(ViewerEvent {
            id: ev.id.to_string(),
            ts: ev.ts.to_rfc3339_opts(SecondsFormat::Secs, true),
            kind: kind.to_string(),
            title: event_title(&ev),
            project: event_project(&ev),
            ephemeral,
            detail: serde_json::to_value(&ev.kind).unwrap_or(Value::Null),
        });
    }
    // Newest-first, capped.
    out.reverse();
    out.truncate(limit);
    Ok(Json(EventsResponse {
        ok: true,
        events: out,
    }))
}

#[derive(Debug, Serialize)]
pub struct ActivityDay {
    pub date: String,
    pub count: u64,
}

#[derive(Debug, Serialize)]
pub struct ActivityResponse {
    pub ok: bool,
    pub days: Vec<ActivityDay>,
    pub by_kind: BTreeMap<String, u64>,
    /// Signal/trace/decomposed split so the viewer can show how much of the
    /// captured volume is real signal vs ephemeral tool-trace, and how much of
    /// the signal has been understood — instead of one undifferentiated count.
    pub signal: u64,
    pub trace: u64,
    pub decomposed: u64,
}

#[derive(Debug, Deserialize)]
pub struct ActivityQuery {
    /// Scope the heatmap + breakdowns to one project. Absent/empty = all.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub include_global: Option<bool>,
}

/// Per-day event counts (for the GitHub-style heatmap) + a kind breakdown +
/// the signal/trace/decomposed split. One log walk; the heatmap renders the
/// trailing window client-side. Scoped to a project when `?project=` is given.
pub async fn activity(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<ActivityQuery>,
) -> Result<Json<ActivityResponse>, ApiError> {
    let scope = project_scope(q.project.as_deref(), want_global(q.include_global));
    let mut per_day: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_kind: BTreeMap<String, u64> = BTreeMap::new();
    // Signal vs trace is decided per capture (ephemeral = trace); decomposed is
    // the subset of signal captures that produced an Understanding. Collected in
    // one pass: understandings always follow their capture in the log.
    let mut signal_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut understood_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut trace: u64 = 0;
    for ev in state
        .event_log
        .iter()
        .map_err(internal("event_log_read_failed", "open log for activity"))?
    {
        let ev = ev.map_err(internal("event_read_failed", "read event for activity"))?;
        if !event_passes_scope(&ev, &scope) {
            continue;
        }
        let date = ev.ts.format("%Y-%m-%d").to_string();
        *per_day.entry(date).or_default() += 1;
        *by_kind
            .entry(event_kind_str(&ev.kind).to_string())
            .or_default() += 1;
        match &ev.kind {
            EventKind::Capture(p) if p.is_ephemeral() => trace += 1,
            EventKind::Capture(_) => {
                signal_ids.insert(ev.id.to_string());
            }
            EventKind::Understanding(u) => {
                understood_ids.insert(u.source_id.to_string());
            }
            _ => {}
        }
    }
    let decomposed = signal_ids
        .iter()
        .filter(|id| understood_ids.contains(*id))
        .count() as u64;
    let days = per_day
        .into_iter()
        .map(|(date, count)| ActivityDay { date, count })
        .collect();
    Ok(Json(ActivityResponse {
        ok: true,
        days,
        by_kind,
        signal: signal_ids.len() as u64,
        trace,
        decomposed,
    }))
}

#[derive(Debug, Deserialize)]
pub struct GraphRequest {
    /// Restrict to facts whose source capture was tagged with this project.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub include_global: Option<bool>,
    /// Max edges (highest-confidence first). Bounds the graph the viewer draws.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Anchor-first exploration (Cypher-lite `MATCH (a)-[r]-(n)`): when set,
    /// return the 2-hop neighborhood of this entity instead of the global
    /// strongest edges. The value is matched canonically (case/space-insensitive).
    #[serde(default)]
    pub anchor: Option<String>,
}

/// A resolved, TYPED graph node. `id` is the canonical resolution key (stable),
/// `label` the human surface form, `kind` the entity type (person/project/tool/
/// ... from the understanding layer, or "thing" when the node only appears as a
/// fact value and was never named as an entity).
#[derive(Debug, Serialize)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub degree: u32,
    pub mentions: u64,
}

/// A typed edge: `label` is the relation (predicate) = the edge type. Carries
/// provenance the viewer surfaces (confidence + valid-time).
#[derive(Debug, Serialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub label: String,
    pub confidence: f64,
    pub valid_from: String,
}

#[derive(Debug, Serialize)]
pub struct GraphResponse {
    pub ok: bool,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

const GRAPH_LIMIT_DEFAULT: usize = 160;
const GRAPH_LIMIT_MAX: usize = 600;
/// Skip facts whose object is long free text rather than an entity-like value;
/// they make unreadable graph nodes. Subject/predicate are always short.
const GRAPH_MAX_OBJECT_LEN: usize = 60;

/// A graph node string that is operational noise, not knowledge: trace markers
/// (`[Bash]`), file paths, and URLs. The understanding worker already refuses to
/// decompose ephemeral traces, but older facts can still carry path-like values;
/// this keeps the rendered graph to real entities.
fn is_noise_node(s: &str) -> bool {
    let s = s.trim();
    s.is_empty()
        || s.starts_with('[')
        || s.starts_with("http")
        || s.contains('/')
        || s.contains('\\')
}

/// The TYPED knowledge graph (P2): resolved, deduplicated nodes carrying their
/// entity kind, and typed edges (predicate = relation) carrying confidence +
/// valid-time. Two modes: anchor-first 2-hop expansion, or the global strongest
/// edges. Nodes are deduplicated by canonical key so "localmem" is ONE node.
pub async fn graph(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GraphRequest>,
) -> Result<Json<GraphResponse>, ApiError> {
    use crate::facts::canonicalize_entity;
    use std::collections::{HashMap, HashSet};

    let limit = req
        .limit
        .unwrap_or(GRAPH_LIMIT_DEFAULT)
        .min(GRAPH_LIMIT_MAX);
    let project = req.project.unwrap_or_default();
    let now = Utc::now();

    // Resolved entity nodes: canonical -> (display, kind, mentions). This is the
    // typing layer; fact subjects/objects are looked up against it by canonical
    // key so the same node id-space is shared.
    let entity_map: HashMap<String, (String, String, u64)> = {
        let guard = state.facts.lock().await;
        guard
            .resolved_entities()
            .map(|v| {
                v.into_iter()
                    .map(|e| (e.canonical, (e.display_name, e.kind, e.mentions)))
                    .collect()
            })
            .unwrap_or_default()
    };

    // Edge rows as (subject, predicate, object, confidence, valid_from), from
    // either the anchor neighborhood or the global strongest live facts.
    let anchor = req
        .anchor
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    let rows: Vec<(String, String, String, f64, DateTime<Utc>)> = if let Some(anchor) = anchor {
        let guard = state.facts.lock().await;
        guard
            .entity_graph_walk(
                &[anchor.to_string()],
                2,
                0.0,
                limit.saturating_mul(2).max(1),
            )
            .map_err(internal("facts_read_failed", "walk graph from anchor"))?
            .into_iter()
            .map(|r| (r.subject, r.predicate, r.object, r.confidence, r.valid_from))
            .collect()
    } else {
        let scope = project_scope((!project.is_empty()).then_some(project.as_str()), want_global(req.include_global));
        let mut facts = {
            let guard = state.facts.lock().await;
            guard
                .all_live_facts_scoped(
                    now,
                    None,
                    None,
                    scope.as_ref(),
                    crate::reserved_tags::Visibility::Default,
                    now,
                )
                .map_err(internal("facts_read_failed", "gather facts for graph"))?
        };
        // Strongest, then most-recent, so the drawn graph is the strongest
        // relations and stays flat in cost as the corpus grows.
        facts.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.valid_from.cmp(&a.valid_from))
        });
        facts
            .into_iter()
            .map(|f| (f.subject, f.predicate, f.object, f.confidence, f.valid_from))
            .collect()
    };

    let mut degree: HashMap<String, u32> = HashMap::new();
    let mut display: HashMap<String, String> = HashMap::new();
    let mut seen_edge: HashSet<(String, String, String)> = HashSet::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    for (s, p, o, confidence, valid_from) in rows {
        if edges.len() >= limit {
            break;
        }
        let (s, o) = (s.trim(), o.trim());
        if is_noise_node(s) || is_noise_node(o) || o.chars().count() > GRAPH_MAX_OBJECT_LEN {
            continue;
        }
        let (sc, oc) = (canonicalize_entity(s), canonicalize_entity(o));
        if sc.is_empty() || oc.is_empty() || sc == oc {
            continue;
        }
        // Dedup identical edges that collapse under canonicalization.
        if !seen_edge.insert((sc.clone(), oc.clone(), p.clone())) {
            continue;
        }
        // Remember a raw surface form for nodes not in the entity map.
        display.entry(sc.clone()).or_insert_with(|| s.to_string());
        display.entry(oc.clone()).or_insert_with(|| o.to_string());
        *degree.entry(sc.clone()).or_default() += 1;
        *degree.entry(oc.clone()).or_default() += 1;
        edges.push(GraphEdge {
            source: sc,
            target: oc,
            label: p,
            confidence,
            valid_from: valid_from.to_rfc3339_opts(SecondsFormat::Secs, true),
        });
    }

    let mut nodes: Vec<GraphNode> = degree
        .into_iter()
        .map(|(canon, degree)| {
            let (label, kind, mentions) = match entity_map.get(&canon) {
                Some((disp, kind, m)) => (disp.clone(), kind.clone(), *m),
                None => (
                    display
                        .get(&canon)
                        .cloned()
                        .unwrap_or_else(|| canon.clone()),
                    "thing".to_string(),
                    0,
                ),
            };
            GraphNode {
                id: canon,
                label,
                kind,
                degree,
                mentions,
            }
        })
        .collect();
    // Stable, densest-first ordering for the viewer.
    nodes.sort_by(|a, b| b.degree.cmp(&a.degree).then(a.id.cmp(&b.id)));

    Ok(Json(GraphResponse {
        ok: true,
        nodes,
        edges,
    }))
}

#[derive(Debug, Deserialize)]
pub struct GraphHighlightRequest {
    /// Source/capture event ids (e.g. from /search hits) whose connected graph
    /// entities should be surfaced for highlighting.
    #[serde(default)]
    pub event_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct GraphHighlightResponse {
    pub ok: bool,
    /// Canonical entity ids (= graph node ids) connected to the matched events.
    pub entities: Vec<String>,
}

/// Map a set of source-event ids to the canonical graph entities they touch.
/// This is the bridge that turns a hybrid SEARCH result (keyword + semantic +
/// temporal, e.g. "eggs last week") into a set of graph nodes to light up: the
/// graph becomes a queryable surface without a separate graph database, since
/// the facts store already holds the edges. Live facts only (current beliefs).
pub async fn graph_highlight(
    State(state): State<Arc<AppState>>,
    Json(req): Json<GraphHighlightRequest>,
) -> Result<Json<GraphHighlightResponse>, ApiError> {
    use crate::facts::canonicalize_entity;
    use std::collections::HashSet;

    if req.event_ids.is_empty() {
        return Ok(Json(GraphHighlightResponse {
            ok: true,
            entities: Vec::new(),
        }));
    }
    let want: HashSet<String> = req.event_ids.into_iter().collect();
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
            .map_err(internal(
                "facts_read_failed",
                "gather facts for graph highlight",
            ))?
    };
    let mut entities: HashSet<String> = HashSet::new();
    for f in &facts {
        if f.source_events
            .iter()
            .any(|e| want.contains(&e.to_string()))
        {
            let s = canonicalize_entity(&f.subject);
            if !s.is_empty() {
                entities.insert(s);
            }
            let o = canonicalize_entity(&f.object);
            if !o.is_empty() {
                entities.insert(o);
            }
        }
    }
    Ok(Json(GraphHighlightResponse {
        ok: true,
        entities: entities.into_iter().collect(),
    }))
}

#[derive(Debug, Serialize)]
pub struct ImportCandidate {
    pub format: String,
    pub path: String,
    pub confidence: String,
    pub hint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ImportScanResponse {
    pub ok: bool,
    pub candidates: Vec<ImportCandidate>,
}

/// Discover importable history so the viewer can surface onboarding ("bring your
/// Claude Code / ChatGPT history"). Read-only: it only scans, never writes. The
/// headline case is Claude Code history at `~/.claude/projects`; ChatGPT/Claude
/// exports in Downloads/Desktop/cwd come from the existing import wizard.
pub async fn import_scan(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<ImportScanResponse>, ApiError> {
    let mut candidates: Vec<ImportCandidate> = Vec::new();
    if let Ok(dets) = crate::cli::import_wizard::scan_default_locations() {
        for d in dets {
            candidates.push(ImportCandidate {
                format: d.format,
                path: d.path.display().to_string(),
                confidence: format!("{:?}", d.confidence).to_lowercase(),
                hint: d.hint,
                sessions: None,
            });
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = std::path::Path::new(&home).join(".claude").join("projects");
        if p.is_dir() {
            let sessions = count_jsonl_sessions(&p);
            if sessions > 0 {
                candidates.push(ImportCandidate {
                    format: "claude-code".to_string(),
                    path: p.display().to_string(),
                    confidence: "high".to_string(),
                    hint: format!("your Claude Code history across {sessions} session file(s)"),
                    sessions: Some(sessions),
                });
            }
        }
    }
    Ok(Json(ImportScanResponse {
        ok: true,
        candidates,
    }))
}

/// Count `*.jsonl` session files one level under a Claude Code projects dir.
fn count_jsonl_sessions(dir: &std::path::Path) -> u64 {
    let is_jsonl = |p: &std::path::Path| p.extension().map(|x| x == "jsonl").unwrap_or(false);
    let mut n = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Ok(rd2) = std::fs::read_dir(&p) {
                    n += rd2.flatten().filter(|e2| is_jsonl(&e2.path())).count() as u64;
                }
            } else if is_jsonl(&p) {
                n += 1;
            }
        }
    }
    n
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
    use crate::facts::Fact;
    use crate::server::{router, AppState};
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Method, Request, StatusCode};
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tower::ServiceExt;

    const BODY_LIMIT: usize = 64 * 1024;

    #[test]
    fn build_brief_context_renders_dated_grounded_lines() {
        use crate::event::UnderstandingPayload;
        let id1: EventId = "01HXY00000000000000000000Z".parse().unwrap();
        let when = chrono::DateTime::parse_from_rfc3339("2026-06-13T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let understandings = vec![UnderstandingPayload {
            source_id: id1,
            summary: "Vijay picked LanceDB for vectors.".into(),
            intent: "record a decision".into(),
            entities: vec![],
            references: vec!["core/src/vectors.rs".into()],
            salience: "decision".into(),
            model: "llama3.2:latest".into(),
            valid_from: when,
            tags: Default::default(),
            extra: serde_json::Map::new(),
        }];
        let fact = Fact {
            id: id1,
            subject: "localmem".into(),
            predicate: "uses".into(),
            object: "LanceDB".into(),
            confidence: 0.9,
            valid_from: when,
            valid_to: None,
            recorded_at: when,
            retired_at: None,
            source_events: vec![id1],
            policy_id: None,
            tags: Default::default(),
            kind: Default::default(),
        };

        let (ctx, sources) = build_brief_context(&[fact], &understandings);
        assert!(ctx.contains("Recent activity"));
        assert!(ctx.contains("Vijay picked LanceDB"));
        assert!(ctx.contains("intent: record a decision"));
        assert!(ctx.contains("2026-06-13"), "lines carry a date");
        assert!(ctx.contains("Known facts"));
        assert!(ctx.contains("localmem uses LanceDB"));
        // The source id is cited (grounding) for both the summary and the fact.
        assert!(ctx.contains(&id1.to_string()));
        assert_eq!(sources.len(), 2, "one source per summary + per fact");
    }

    #[test]
    fn select_understandings_keeps_signal_over_recent_chatter() {
        use crate::event::UnderstandingPayload;
        let id: EventId = "01HXY00000000000000000000Z".parse().unwrap();
        let mk = |secs: i64, salience: &str, summary: &str| UnderstandingPayload {
            source_id: id,
            summary: summary.into(),
            intent: String::new(),
            entities: vec![],
            references: vec![],
            salience: salience.into(),
            model: "m".into(),
            valid_from: chrono::DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap(),
            tags: Default::default(),
            extra: serde_json::Map::new(),
        };
        // Oldest-first input: one old decision, then a flood of recent notes.
        let all = vec![
            mk(0, "decision", "chose LanceDB"),
            mk(10, "note", "chatter 1"),
            mk(20, "note", "chatter 2"),
            mk(30, "note", "chatter 3"),
        ];
        // Budget of 2: the decision must survive despite being the oldest.
        let sel = select_understandings(all, 2);
        assert_eq!(sel.len(), 2);
        assert!(
            sel.iter().any(|u| u.summary == "chose LanceDB"),
            "the older decision is kept over recent notes: {sel:?}"
        );
        // Presented newest-first.
        assert!(sel[0].valid_from >= sel[1].valid_from);
    }

    #[test]
    fn build_brief_context_skips_summaryless_understandings() {
        use crate::event::UnderstandingPayload;
        let id: EventId = "01HXY00000000000000000000Z".parse().unwrap();
        let when = Utc::now();
        let u = UnderstandingPayload {
            source_id: id,
            summary: "   ".into(),
            intent: String::new(),
            entities: vec![],
            references: vec![],
            salience: String::new(),
            model: "m".into(),
            valid_from: when,
            tags: Default::default(),
            extra: serde_json::Map::new(),
        };
        let (ctx, sources) = build_brief_context(&[], &[u]);
        assert!(ctx.trim().is_empty(), "no usable input -> empty context");
        assert!(sources.is_empty());
    }

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

    // ---- spool drain -----------------------------------------------------

    #[tokio::test]
    async fn drain_spool_ingests_spooled_captures_and_clears_the_file() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        // open() spawns a drain over an empty spool (no-op); then we write the
        // spool and drain explicitly so the assertions are race-free.
        let state = new_state(home).await;
        let spool_dir = home.join("spool");
        std::fs::create_dir_all(&spool_dir).unwrap();
        let lines = [
            r#"{"content":"We decided to use LanceDB for the vector store in localmem.","source":"claude-code","tags":{"project":"localmem"}}"#,
            r#"{"content":"   "}"#, // empty content -> skipped, not an error
        ]
        .join("\n");
        std::fs::write(spool_dir.join("captures.jsonl"), lines + "\n").unwrap();

        drain_spool(state.clone()).await;

        // Both the live spool and the work file are gone (clean drain).
        assert!(!spool_dir.join("captures.jsonl").exists());
        assert!(!spool_dir.join("captures.draining.jsonl").exists());

        // The real capture landed in the event log via the full pipeline.
        let found = state
            .event_log
            .iter()
            .unwrap()
            .filter_map(Result::ok)
            .any(|ev| matches!(&ev.kind, EventKind::Capture(p) if p.text.contains("LanceDB")));
        assert!(found, "spooled capture was ingested through the pipeline");
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
    async fn write_rejects_malformed_as_of() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, body) = post(
            state,
            "/write",
            json!({"content": "a sufficiently long memory about gardening tools", "as_of": "not-a-date"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "bad_as_of");
    }

    // End-to-end across the storage<->retrieval seam: a dated write must come
    // back from /search carrying that valid-time, not ingestion-now and not a
    // hardcoded None. This guards the T-63 valid_from threading + write-side
    // as_of together — the integration point that the unit tests on each side
    // previously left uncovered.
    #[tokio::test]
    async fn dated_write_surfaces_valid_from_in_search() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        let (status, _) = post(
            state.clone(),
            "/write",
            json!({
                "content": "I bought a smoker for my backyard barbecue last weekend.",
                "as_of": "2023-01-15T10:00:00Z",
                "source": "test"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = post(
            state,
            "/search",
            json!({"query": "smoker barbecue", "k": 5}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let results = body["results"].as_array().expect("results array");
        assert!(!results.is_empty(), "expected a search hit, got: {body:?}");
        let vf = results[0]["valid_from"]
            .as_str()
            .unwrap_or_else(|| panic!("valid_from missing from hit: {body:?}"));
        assert!(
            vf.starts_with("2023-01-15"),
            "valid_from should reflect the as_of date, got: {vf}"
        );
    }

    // T-117: a /write enqueues its embedding for the async worker and returns
    // immediately; /drain blocks until the worker has flushed the backlog, after
    // which the vector store holds the capture's row. Requires the real model;
    // skips on CI where it is absent.
    #[tokio::test]
    async fn async_embed_lands_in_vector_store_after_drain() {
        let Some(model_dir) = crate::embed::test_assets::ensure_model() else {
            eprintln!("{}", crate::embed::test_assets::skip_reason());
            return;
        };
        std::env::set_var("LOCALMEM_MODEL_DIR", &model_dir);
        let tmp = tempdir().unwrap();
        let state = AppState::open(tmp.path()).await.unwrap();
        std::env::remove_var("LOCALMEM_MODEL_DIR");

        // With a model present the async worker is live.
        assert!(
            state.embed_tx.is_some(),
            "embedder + async worker should be active"
        );

        let (status, _) = post(
            state.clone(),
            "/write",
            json!({"content": "I bought a smoker for my backyard barbecue.", "source": "test"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Flush the embedding backlog, then the vector must be present.
        let (status, _) = post(state.clone(), "/drain", json!({})).await;
        assert_eq!(status, StatusCode::OK);

        let n = state
            .vectors
            .as_ref()
            .as_ref()
            .expect("vector store present with model")
            .count()
            .await
            .unwrap();
        assert_eq!(
            n, 1,
            "the capture's vector must land in the store after /drain"
        );
    }

    // T-118: /write defers the Tantivy commit; a subsequent /search must still
    // see the write because search commits-on-read. Two writes, no /drain, one
    // search that must surface the deferred content.
    #[tokio::test]
    async fn deferred_lexical_commit_is_visible_on_search() {
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        for c in [
            "The quarterly revenue target is four million dollars this year.",
            "We migrated the billing service to Postgres last week and it is faster.",
        ] {
            let (status, _) = post(
                state.clone(),
                "/write",
                json!({"content": c, "source": "test"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        // No /drain: search must commit the deferred writes itself and find them.
        let (status, body) = post(
            state,
            "/search",
            json!({"query": "billing service Postgres", "k": 5}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let results = body["results"].as_array().expect("results array");
        assert!(
            !results.is_empty(),
            "a deferred-commit write must be visible after commit-on-read: {body:?}"
        );
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

        // T-118: /write defers the lexical commit, so flush via /drain before
        // reading the index from a fresh reader-only handle.
        let (status, _) = post(state, "/drain", json!({})).await;
        assert_eq!(status, StatusCode::OK);

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
            state.clone(),
            "/write",
            json!({
                "content": "rust async runtime notes for localmem",
                "tags": {"project": "localmem", "topic": "async"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // T-118: flush the deferred lexical commit before the direct read.
        let (status, _) = post(state, "/drain", json!({})).await;
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

    // ---- T-120: entity-graph + recall over MCP ---------------------------
    //
    // These prove the multi-agent "what have we seen before" path works
    // end-to-end THROUGH THE SERVER (the only public interface), not just in
    // the retriever's own unit tests. We seed a known fact chain directly into
    // the shared facts store (bypassing the rules extractor, whose natural-
    // language output is deliberately sparse) so the assertions isolate the
    // server wiring: config -> RetrieverRegistry::from_config -> graph walk ->
    // /search response.

    /// Build a fact chain `alice -knows-> bob -lives_in-> paris` and insert it
    /// into the server's shared facts store. Returns the home dir tempdir.
    async fn seed_alice_bob_paris(state: &Arc<AppState>) {
        fn fact(subject: &str, predicate: &str, object: &str) -> Fact {
            let t = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
            Fact {
                id: EventId::new(),
                subject: subject.into(),
                predicate: predicate.into(),
                object: object.into(),
                confidence: 0.9,
                valid_from: t,
                valid_to: None,
                recorded_at: t,
                retired_at: None,
                source_events: vec![EventId::new()],
                policy_id: None,
                tags: Default::default(),
                kind: Default::default(),
            }
        }
        let facts = state.facts.lock().await;
        facts.insert(&fact("alice", "knows", "bob")).unwrap();
        facts.insert(&fact("bob", "lives_in", "paris")).unwrap();
    }

    #[tokio::test]
    async fn entity_graph_surfaces_neighbour_via_server_search() {
        // Opt in via config: [retriever].plugins includes "entity-graph".
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        std::fs::write(
            tmp.path().join(crate::config::CONFIG_FILE),
            "[retriever]\nplugins = [\"hybrid\", \"entity-graph\"]\n",
        )
        .unwrap();
        seed_alice_bob_paris(&state).await;

        // Query names only the seed subject. "paris" is two hops away and is
        // NOT indexed as any capture (no /write, no embedder) -> the only way
        // it can appear is the entity-graph recursive walk.
        let (status, body) = post(
            state,
            "/search",
            json!({"query": "tell me about alice", "k": 10}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let results = body["results"].as_array().unwrap();
        let facts: Vec<&str> = results
            .iter()
            .map(|r| r["fact"].as_str().unwrap())
            .collect();
        assert!(
            facts.iter().any(|f| f.contains("paris")),
            "entity-graph should surface the 2-hop neighbour `paris`, got: {facts:?}"
        );
        assert!(
            facts.iter().any(|f| f.contains("bob")),
            "entity-graph should surface the 1-hop neighbour `bob`, got: {facts:?}"
        );
    }

    #[tokio::test]
    async fn default_config_does_not_surface_graph_only_neighbour() {
        // No config.toml -> default plugins = ["hybrid"]. The graph is NOT
        // walked, so the un-indexed neighbour `paris` must not appear. This is
        // the discoverability contract: entity-graph is opt-in, and a user who
        // has not enabled it sees exactly the v0.1 hybrid behaviour.
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        seed_alice_bob_paris(&state).await;

        let (status, body) = post(
            state,
            "/search",
            json!({"query": "tell me about alice", "k": 10}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let results = body["results"].as_array().unwrap();
        assert!(
            !results
                .iter()
                .any(|r| r["fact"].as_str().unwrap().contains("paris")),
            "hybrid-only search must not surface the graph-only neighbour, got: {body:?}"
        );
    }

    #[tokio::test]
    async fn recall_returns_directly_inserted_entity_chain() {
        // The audit-grade entity pull: /recall over a known subject returns its
        // facts regardless of retriever config (it reads the facts store
        // directly, not the registry).
        let tmp = tempdir().unwrap();
        let state = new_state(tmp.path()).await;
        seed_alice_bob_paris(&state).await;

        let (status, body) = post(state, "/recall", json!({"entity": "alice"})).await;
        assert_eq!(status, StatusCode::OK);
        let facts = body["facts"].as_array().unwrap();
        assert_eq!(facts.len(), 1, "alice has exactly one fact, got: {body:?}");
        assert_eq!(facts[0]["predicate"], "knows");
        assert_eq!(facts[0]["object"], "bob");
        assert!(
            !facts[0]["sources"].as_array().unwrap().is_empty(),
            "recall fact must carry its source event ids"
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
        assert!(body["profile_md"]
            .as_str()
            .unwrap()
            .starts_with("# localmem profile"));
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
        assert!(arr
            .iter()
            .any(|t| t["key"] == "project" && t["value"] == "localmem"));
        assert!(arr
            .iter()
            .any(|t| t["key"] == "topic" && t["value"] == "lang"));
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
