//! Local HTTP server (axum) that the MCP server talks to.
//!
//! See ARCHITECTURE.md (High-level shape -> server). Implementation tasks:
//! T-19 (skeleton + /health), T-20 (/write), T-21 (/search), T-22 (recall /
//! profile / forget / journal), T-43 + T-44 (finish the pipeline: real
//! policy + extractor on /write, real stores behind /recall, /profile,
//! /forget, /journal).
//!
//! The server is *private* to the MCP server (per CLAUDE.md architectural
//! conventions). It binds to a loopback address by default. Every endpoint
//! returns SPEC.md-shaped JSON: `{ ok: true, ... }` on success,
//! `{ ok: false, error: { code, message } }` on failure.

pub mod routes;
pub mod understand;

use crate::embed::{Embedder, EMBEDDING_DIM};
use crate::event::EventKind;
use crate::event_log::EventLog;
use crate::extractor::ExtractorRegistry;
use crate::facts::FactsStore;
use crate::journal::Journal;
use crate::lexical::{LexicalIndex, LexicalResultExt};
use crate::policy::Policy;
use crate::understanding::{build_decomposer, Decomposer};
use crate::vectors::VectorStore;
use anyhow::{Context, Result};
use axum::{
    response::{Html, IntoResponse},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};
use understand::{understand_worker, UnderstandJob, UNDERSTAND_QUEUE_CAP};

/// Max captures embedded in one ONNX forward pass (T-117). Batching amortizes
/// tokenizer + inference overhead; 32 is a good CPU sweet spot for bge-small.
const EMBED_BATCH_MAX: usize = 32;
/// Bounded embed queue capacity (T-117). Backpressure: a writer outrunning the
/// embedder blocks briefly rather than letting the queue grow without bound.
const EMBED_QUEUE_CAP: usize = 2048;

/// A capture awaiting its vector embedding, handed to the background worker.
pub struct EmbedJob {
    pub event_id: String,
    pub text: String,
    pub ts: DateTime<Utc>,
    /// SPEC §2.8: the source capture's container tags, stored ON the vector row
    /// so the vec retrieval path scopes/filters by its own data (cohesion: no
    /// borrowing from the lexical index).
    pub tags: std::collections::BTreeMap<String, String>,
}

/// Shared, cheaply-cloneable handle to every store the route handlers touch.
///
/// `LexicalIndex` owns an exclusive directory lock on the Tantivy index, so
/// only one `AppState` (and one server) may exist per localmem home. The
/// per-store mutexes are async-aware so handlers can hold them across
/// awaits without blocking the runtime.
///
/// `embedder` and `vectors` are optional: a home without the BGE model can
/// still run write + lex search + facts. Hybrid search and the vector
/// store will then be empty until the model is installed and
/// `localmem reindex` runs.
pub struct AppState {
    pub home: PathBuf,
    pub event_log: Arc<EventLog>,
    pub lexical: Arc<Mutex<LexicalIndex>>,
    /// DuckDB's `Connection` is `Send` but `!Sync`, so cross-handler
    /// access has to go through an async mutex (we already serialize
    /// at the application layer; the wrapper just satisfies the type
    /// system).
    pub facts: Arc<Mutex<FactsStore>>,
    pub journal: Arc<Journal>,
    pub policy: Arc<Policy>,
    pub extractor: Arc<ExtractorRegistry>,
    /// Wrapped in a Mutex because `Embedder::embed` takes `&mut self`.
    /// `None` means no model present; handlers gracefully degrade.
    pub embedder: Arc<Mutex<Option<Embedder>>>,
    /// `None` means no embedder loaded, so vectors are not maintained.
    pub vectors: Arc<Option<VectorStore>>,
    /// T-117: async embedding. `/write` enqueues here (after the synchronous
    /// event-log + lexical + facts steps) instead of embedding inline, so the
    /// write returns immediately and a burst of writes is embedded in batches.
    /// `None` when no embedder/vectors are present (writes skip the queue).
    pub embed_tx: Option<mpsc::Sender<EmbedJob>>,
    /// Captures queued-but-not-yet-embedded. `/drain` waits on this reaching
    /// zero so callers that need fresh vectors (the bench before search, a
    /// consistency-sensitive query) can force the backlog to flush.
    pub embed_pending: Arc<AtomicUsize>,
    /// Layer 2 understanding (SPEC 7c). `/write` enqueues each committed capture
    /// here for ASYNC LLM decomposition, off the write path. `None` when
    /// `[understanding].enabled` is false (the default), so a fresh install does
    /// no LLM work and writes skip the queue entirely.
    pub understand_tx: Option<mpsc::Sender<UnderstandJob>>,
    /// Captures queued-but-not-yet-understood. `/drain` waits on this reaching
    /// zero so a caller that needs the derived facts current can force the
    /// backlog to flush. Always zero when understanding is disabled.
    pub understand_pending: Arc<AtomicUsize>,
    /// The model `/brief` synthesizes with — the same resolved tag the worker
    /// uses. `None` when understanding is disabled (so `/brief` returns a clear
    /// 400 instead of guessing a model).
    pub understand_model: Option<String>,
    /// The active decomposition provider (`ollama` | `openai` | `anthropic` | ...)
    /// when the worker is actually running, so the dashboard can show WHAT model
    /// is understanding memory. `None` when understanding is disabled or idle.
    pub understand_provider: Option<String>,
    /// Ollama endpoint for `/brief` synthesis (from `[understanding]`).
    pub understand_endpoint: String,
    /// Projects whose understanding changed since their cached briefing was last
    /// refreshed. The debounced refresher drains this to keep briefings warm
    /// without needing a session start. Stays empty when understanding is off.
    pub briefing_dirty: Arc<Mutex<HashSet<String>>>,
    /// T-118: the lexical index has docs indexed-but-not-committed. `/write`
    /// indexes without committing (Tantivy is meant to commit in batches, not
    /// per write); the next `/search` or `/drain` commits if this is set. The
    /// event log is the durable source of truth, so deferring the lexical
    /// commit risks no data loss — the index is a recomputable projection.
    pub lex_dirty: Arc<AtomicBool>,
}

impl AppState {
    /// Open every store under `home` and wrap them for shared access.
    /// Idempotent on existing homes.
    pub async fn open(home: impl AsRef<Path>) -> Result<Arc<Self>> {
        let home = home.as_ref().to_path_buf();
        let event_log = Arc::new(EventLog::open(&home).context("open event log")?);
        let lexical = LexicalIndex::open(&home).lex_context("open lexical index")?;
        let facts = FactsStore::open(&home).context("open facts store")?;
        let journal = Journal::open(&home).context("open journal")?;
        let policy = Policy::load(&home).context("load policy")?;

        // T-58 + T-59: build the extractor registry from
        // `[extractor]` config AND scan the home's custom-extractors
        // dir for user-authored YAML extractors. Bails loudly on a
        // config typo OR a broken YAML file so the server refuses to
        // start rather than silently shipping with no extraction.
        // Missing config falls back to defaults via `Config::load`,
        // which already returns `Config::default()` for an absent
        // file.
        let cfg = crate::config::Config::load(&home).context("load config for extractor")?;
        let extractor = ExtractorRegistry::from_config_with_home(&cfg.extractor, &home)
            .context("build extractor registry")?;

        let (embedder, vectors) = open_embedder_and_vectors(&home).await;
        let embedder = Arc::new(Mutex::new(embedder));
        let vectors = Arc::new(vectors);
        let embed_pending = Arc::new(AtomicUsize::new(0));

        // T-117: spawn the async embedding worker only when both an embedder
        // and a vector store are present. Otherwise vectors aren't maintained
        // and writes simply skip the queue (embed_tx = None). The worker holds
        // clones of the SAME Arcs the handlers use, so it shares the one
        // embedder (serialized with query-embedding via the Mutex) and the one
        // vector table.
        let embed_tx = if embedder.lock().await.is_some() && vectors.is_some() {
            let (tx, rx) = mpsc::channel::<EmbedJob>(EMBED_QUEUE_CAP);
            tokio::spawn(embed_worker(
                rx,
                embedder.clone(),
                vectors.clone(),
                embed_pending.clone(),
            ));
            // T-119: re-embed any capture whose vector never landed (a crash or
            // eviction between the events.jsonl append and the async embed).
            // events.jsonl is the source of truth, so this is replay of a
            // derived projection, not recovery of lost data. Runs in the
            // background so startup and request serving are not blocked.
            //
            // Snapshot the log length HERE (before open() returns, so before any
            // write can arrive) and bound the scan to it. Captures written by
            // THIS process land via the write path; the backfill only owns the
            // prefix from prior runs. Without the bound, the scan would race a
            // concurrent write and embed the same capture twice.
            let backfill_limit = event_log.byte_len().unwrap_or(0);
            tokio::spawn(backfill_missing_vectors(
                event_log.clone(),
                vectors.clone(),
                tx.clone(),
                embed_pending.clone(),
                backfill_limit,
            ));
            Some(tx)
        } else {
            None
        };

        // Hoist the facts + journal handles into shared Arcs BEFORE the
        // understanding worker so it derives facts through the SAME store +
        // T-56 contradiction resolution the handlers use (not a parallel copy).
        let facts = Arc::new(Mutex::new(facts));
        let journal = Arc::new(journal);
        let understand_pending = Arc::new(AtomicUsize::new(0));

        // Layer 2 understanding worker (SPEC 7c). Spawned only when opted in via
        // [understanding].enabled, so a fresh install does no LLM work and
        // writes skip the queue (understand_tx = None). The worker calls Ollama
        // OFF the write path; if Ollama is down a decompose fails and the
        // capture stays raw (recoverable by re-running). Backfill across a
        // restart-with-backlog is a documented follow-up (2b adds the
        // `understanding` event that marks a capture done, the idempotency key
        // the embed backfill gets from vector ids).
        //
        // The model is resolved against what Ollama REALLY has installed, not
        // trusted blindly: an exact tag is used as-is, a same-family tag is
        // substituted, and if nothing suitable is installed the worker stays
        // idle rather than 404-ing every capture. Nothing is forced on the user
        // and no model name is load-bearing.
        let understand_endpoint = cfg.understanding.ollama_endpoint.clone();
        let briefing_dirty: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let (understand_tx, understand_model, understand_provider) = if cfg.understanding.enabled {
            let endpoint = understand_endpoint.clone();
            let provider = cfg.understanding.provider.trim().to_ascii_lowercase();
            // Empty provider means the local Ollama default (the resolver below
            // treats "" and "ollama" identically); normalize the LABEL so the
            // dashboard always names the active backend.
            let provider_label = if provider.is_empty() {
                "ollama".to_string()
            } else {
                provider.clone()
            };

            // Resolve a decomposer + its model label, per provider. Local
            // (Ollama) verifies the configured tag against what is actually
            // installed; a remote provider (the user's OWN key) skips that and
            // builds directly. `None` => do not spawn the worker (captures stay
            // raw + fully searchable).
            let built: Option<(Arc<dyn Decomposer>, String)> = if provider.is_empty()
                || provider == "ollama"
            {
                let configured = cfg.understanding.model.clone();
                let installed = tokio::task::spawn_blocking({
                    let endpoint = endpoint.clone();
                    move || crate::understanding::installed_models(&endpoint)
                })
                .await
                .ok()
                .flatten();
                let resolution =
                    crate::understanding::resolve_model(installed.as_deref(), &configured);
                match &resolution {
                    crate::understanding::ModelResolution::Exact(m) => {
                        info!(model = %m, "understanding: local worker enabled, model ready")
                    }
                    crate::understanding::ModelResolution::Substituted { used, configured } => {
                        warn!(
                            used = %used, configured = %configured,
                            "understanding: configured model not installed; using same-family tag"
                        )
                    }
                    crate::understanding::ModelResolution::NoMatch { installed } => warn!(
                        configured = %configured, installed = ?installed,
                        "understanding: no suitable model installed; worker idle (pull the model or set [understanding].model)"
                    ),
                    crate::understanding::ModelResolution::Unprobed(m) => warn!(
                        model = %m, endpoint = %endpoint,
                        "understanding: Ollama not reachable at startup; will use configured model and retry per capture"
                    ),
                }
                // NoMatch -> no model to call; do not spawn.
                resolution.model_to_use().and_then(|model| {
                        let model = model.to_string();
                        match build_decomposer("ollama", &model, &endpoint, "") {
                            Ok(d) => Some((d, model)),
                            Err(e) => {
                                warn!(error = %e, "understanding: failed to build local decomposer; worker idle");
                                None
                            }
                        }
                    })
            } else {
                // Remote: the user's OWN frontier key (BYO). No resolve_model.
                let model = cfg.understanding.model.clone();
                match build_decomposer(&provider, &model, &endpoint, &cfg.understanding.api_key_env)
                {
                    Ok(d) => {
                        info!(provider = %provider, model = %model,
                                "understanding: remote decomposition backend ready (bring-your-own key)");
                        Some((d, model))
                    }
                    Err(e) => {
                        warn!(provider = %provider, error = %e,
                                "understanding: remote backend unavailable; worker idle (captures stay raw)");
                        None
                    }
                }
            };

            match built {
                Some((decomposer, model)) => {
                    let (tx, rx) = mpsc::channel::<UnderstandJob>(UNDERSTAND_QUEUE_CAP);
                    tokio::spawn(understand_worker(
                        rx,
                        decomposer,
                        cfg.understanding.user_subject.clone(),
                        model.clone(),
                        event_log.clone(),
                        facts.clone(),
                        journal.clone(),
                        understand_pending.clone(),
                        briefing_dirty.clone(),
                        embed_tx.clone(),
                        embed_pending.clone(),
                    ));
                    // `/brief` synthesizes with this model label.
                    (Some(tx), Some(model), Some(provider_label))
                }
                None => (None, None, None),
            }
        } else {
            (None, None, None)
        };

        let app = Arc::new(Self {
            home,
            event_log,
            lexical: Arc::new(Mutex::new(lexical)),
            facts,
            journal,
            policy: Arc::new(policy),
            extractor: Arc::new(extractor),
            embedder,
            vectors,
            embed_tx,
            embed_pending,
            understand_tx,
            understand_pending,
            understand_model,
            understand_provider,
            understand_endpoint,
            briefing_dirty,
            lex_dirty: Arc::new(AtomicBool::new(false)),
        });

        // Drain any captures the hook spooled while the core was down, replaying
        // them through the full ingest pipeline so a momentarily-down core never
        // loses capture. Background so startup + serving aren't blocked.
        tokio::spawn(routes::drain_spool(app.clone()));

        // Keep cached briefings warm as work flows in (not just at session
        // start): a debounced refresher regenerates the briefing for any project
        // the worker understood since the last tick. Only when understanding is
        // on (otherwise nothing ever dirties).
        if app.understand_tx.is_some() {
            tokio::spawn(briefing_refresher(app.clone()));
        }

        Ok(app)
    }
}

/// Debounce window for the briefing refresher: at most one re-synthesis per
/// project per this interval, so a burst of captures collapses into one refresh.
const BRIEFING_REFRESH_DEBOUNCE_SECS: u64 = 90;

/// Periodically regenerate cached briefings for projects whose understanding
/// changed (drained from `briefing_dirty`), so the viewer's Brain tab + the next
/// session boot read a fresh briefing without a manual regenerate. Runs until
/// the `AppState` is dropped.
async fn briefing_refresher(app: Arc<AppState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(BRIEFING_REFRESH_DEBOUNCE_SECS)).await;
        let dirty: Vec<String> = {
            let mut set = app.briefing_dirty.lock().await;
            if set.is_empty() {
                continue;
            }
            set.drain().collect()
        };
        for project in dirty {
            match routes::synthesize_project_briefing(&app, &project).await {
                Ok((md, _)) if !md.trim().is_empty() => {
                    if let Err(err) =
                        crate::understanding::write_briefing_cache(&app.home, &project, &md)
                    {
                        warn!(error = %err, project = %project, "briefing refresher: cache write failed");
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    warn!(error = ?err, project = %project, "briefing refresher: synthesis failed")
                }
            }
        }
    }
}

/// T-117 background embedding worker. Pulls queued captures, embeds each batch
/// in one ONNX forward pass, and persists the vectors in one LanceDB write.
/// Runs until the channel closes (AppState dropped). A failed batch is logged
/// and skipped; events.jsonl remains the source of truth, so a missing vector
/// is recoverable by replay.
async fn embed_worker(
    mut rx: mpsc::Receiver<EmbedJob>,
    embedder: Arc<Mutex<Option<Embedder>>>,
    vectors: Arc<Option<VectorStore>>,
    pending: Arc<AtomicUsize>,
) {
    while let Some(first) = rx.recv().await {
        let mut jobs = vec![first];
        // Coalesce whatever is already queued, up to the batch cap, so a burst
        // of writes collapses into one forward pass.
        while jobs.len() < EMBED_BATCH_MAX {
            match rx.try_recv() {
                Ok(job) => jobs.push(job),
                Err(_) => break,
            }
        }

        let texts: Vec<&str> = jobs.iter().map(|j| j.text.as_str()).collect();
        let embeds = {
            let mut guard = embedder.lock().await;
            match guard.as_mut() {
                Some(e) => e.embed_batch(&texts),
                None => Ok(Vec::new()),
            }
        };

        match embeds {
            Ok(vs) if vs.len() == jobs.len() => {
                if let Some(store) = vectors.as_ref() {
                    // Pre-serialize tags so the row tuples can borrow them (§2.8:
                    // tags live on the vector row).
                    let tags_json: Vec<String> = jobs
                        .iter()
                        .map(|j| {
                            serde_json::to_string(&j.tags).unwrap_or_else(|_| "{}".to_string())
                        })
                        .collect();
                    let rows: Vec<(&str, &[f32], &str, &str, DateTime<Utc>)> = jobs
                        .iter()
                        .zip(vs.iter())
                        .zip(tags_json.iter())
                        .map(|((j, v), tj)| {
                            (
                                j.event_id.as_str(),
                                v.as_slice(),
                                j.text.as_str(),
                                tj.as_str(),
                                j.ts,
                            )
                        })
                        .collect();
                    if let Err(err) = store.add_many(&rows).await {
                        warn!(error = %err, count = jobs.len(), "async embed: vector add_many failed");
                    }
                }
            }
            Ok(_) => warn!(
                count = jobs.len(),
                "async embed: batch size mismatch; skipped"
            ),
            Err(err) => warn!(error = %err, count = jobs.len(), "async embed: embed_batch failed"),
        }

        // The jobs are no longer pending regardless of outcome (a failure is
        // recoverable via replay). Decrement so `/drain` can make progress.
        pending.fetch_sub(jobs.len(), Ordering::SeqCst);
    }
}

/// T-119 crash-recovery backfill. Scans events.jsonl on startup and re-queues
/// any capture whose vector is absent from vectors.lance, so a `serve` process
/// killed mid-embed (or whose vectors.lance was evicted) self-heals without a
/// full `localmem reindex`. The diff is against the actual stored ids, so this
/// is idempotent: a fully-embedded home queues nothing.
///
/// Jobs flow through the SAME bounded channel and `pending` counter as live
/// writes, so `/drain` waits for the backfill too and the queue stays bounded
/// (the channel applies backpressure if the worker falls behind).
async fn backfill_missing_vectors(
    event_log: Arc<EventLog>,
    vectors: Arc<Option<VectorStore>>,
    embed_tx: mpsc::Sender<EmbedJob>,
    pending: Arc<AtomicUsize>,
    event_log_limit: u64,
) {
    let Some(store) = vectors.as_ref() else {
        return;
    };
    let existing = match store.existing_ids().await {
        Ok(ids) => ids,
        Err(err) => {
            warn!(error = %err, "embed backfill: scan of existing vector ids failed; skipping");
            return;
        }
    };
    // Bounded to the startup snapshot so a concurrent live write (handled by the
    // write path) is never re-embedded here.
    let iter = match event_log.iter_to(event_log_limit) {
        Ok(it) => it,
        Err(err) => {
            warn!(error = %err, "embed backfill: open event log failed; skipping");
            return;
        }
    };

    let mut queued = 0usize;
    for ev_result in iter {
        let event = match ev_result {
            Ok(e) => e,
            Err(err) => {
                warn!(error = %err, "embed backfill: skipping unreadable event");
                continue;
            }
        };
        let EventKind::Capture(payload) = &event.kind else {
            continue;
        };
        // Parity with the write path and replay: ephemeral tool-traces never
        // enter the vector store. Without this the backfill re-embeds the exact
        // [Bash]/[Read]/trace noise those paths correctly skip — they have no
        // vector, so they look "missing" — re-polluting search after a clean
        // replay. (The retrieval-hygiene rule lives in one place: is_ephemeral.)
        if payload.is_ephemeral() {
            continue;
        }
        let id = event.id.to_string();
        if existing.contains(&id) {
            continue;
        }
        // Use the capture's valid-time (not recorded-at) so the backfilled
        // vector carries the same instant the live write path stamps (T-63).
        let job = EmbedJob {
            event_id: id,
            text: payload.text.clone(),
            ts: payload.effective_capture_instant(event.ts),
            tags: payload.tags.clone(),
        };
        pending.fetch_add(1, Ordering::SeqCst);
        if embed_tx.send(job).await.is_err() {
            // The worker/receiver is gone (shutdown). Undo the increment so a
            // concurrent /drain can still reach zero, and stop.
            pending.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        queued += 1;
    }

    if queued > 0 {
        info!(
            queued,
            "embed backfill: re-queued captures missing a vector"
        );
    }
}

/// Mirror of `cli::search::resolve_model_dir`. Kept private here so the
/// CLI module isn't pulled into the server's surface.
fn resolve_model_dir(home: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var("LOCALMEM_MODEL_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home.join("models").join("bge-small-en-v1.5")
}

/// Try to load the embedder; on failure log a warning and run without
/// vectors. The vector store is only opened when the embedder loads so
/// we don't leave an empty lance directory with the wrong schema.
async fn open_embedder_and_vectors(home: &Path) -> (Option<Embedder>, Option<VectorStore>) {
    let model_dir = resolve_model_dir(home);
    let embedder = match Embedder::load(&model_dir) {
        Ok(e) => Some(e),
        Err(err) => {
            warn!(
                model_dir = %model_dir.display(),
                error = %err,
                "embedder unavailable; server will run without vector store"
            );
            return (None, None);
        }
    };
    let vectors = match VectorStore::open(home, EMBEDDING_DIM).await {
        Ok(v) => Some(v),
        Err(err) => {
            warn!(error = %err, "vector store open failed; server will run without it");
            return (embedder, None);
        }
    };
    (embedder, vectors)
}

/// Build the axum [`Router`] with every route registered.
///
/// Split from [`serve`] so tests can drive handlers via
/// `tower::ServiceExt::oneshot` without binding a TCP listener.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(routes::health))
        .route("/version", get(routes::version))
        .route("/write", post(routes::write))
        .route("/drain", post(routes::drain))
        .route("/brief", post(routes::brief))
        .route("/brief/cached", get(routes::brief_cached))
        .route("/brief/refresh", post(routes::brief_refresh))
        .route("/understand/backfill", post(routes::understand_backfill))
        // Viewer aggregations (the multi-tab dashboard's read surfaces).
        .route("/stats", get(routes::stats))
        .route("/events", post(routes::events))
        .route("/activity", get(routes::activity))
        .route("/graph", post(routes::graph))
        .route("/graph/highlight", post(routes::graph_highlight))
        .route("/import/scan", get(routes::import_scan))
        .route("/search", post(routes::search))
        .route("/north-star", get(routes::north_star))
        .route("/getting-started", get(routes::getting_started))
        .route("/recall", post(routes::recall))
        .route("/get", post(routes::get))
        .route("/profile", post(routes::profile))
        .route("/profile/grouped", post(routes::profile_grouped))
        .route("/review", post(routes::review))
        .route("/forget", post(routes::forget))
        .route("/journal", post(routes::journal))
        // T-54: discovery primitives backing the MCP Resources surface.
        // GET so the MCP server can short-circuit subscription polls with
        // a simple HTTP cache; the read-only nature matches REST idiom.
        .route("/resource/profile", get(routes::resource_profile))
        .route("/resource/subjects", get(routes::resource_subjects))
        .route("/resource/tags", get(routes::resource_tags))
        .route("/resource/recent", get(routes::resource_recent))
        .with_state(state)
}

/// Router with the embedded local dashboard served at `/` (P6).
///
/// The API is exposed twice: bare (`/search`, …) for the MCP server, and under
/// `/api/*` so the same-origin dashboard (whose default API base is `/api`)
/// works WITHOUT the `serve.py` proxy. Store-switching (`/__meta/*`) is a
/// serve.py supervisor feature, so the natively-served dashboard is
/// single-store; app.js degrades gracefully when those routes are absent.
pub fn router_with_dashboard(state: Arc<AppState>) -> Router {
    Router::new()
        .merge(router(state.clone()))
        .nest("/api", router(state.clone()))
        .route("/", get(dashboard_index))
        .route("/app.js", get(dashboard_app_js))
        .route("/styles.css", get(dashboard_styles))
        .route("/vendor/cytoscape.min.js", get(dashboard_cytoscape))
        .route("/vendor/layout-base.js", get(dashboard_layout_base))
        .route("/vendor/cose-base.js", get(dashboard_cose_base))
        .route("/vendor/cytoscape-fcose.js", get(dashboard_fcose))
}

// Dashboard assets are embedded in the binary (single-binary install), so
// `serve --dashboard` works without the `dashboard/` directory on disk.
async fn dashboard_index() -> impl IntoResponse {
    Html(include_str!("../../../dashboard/index.html"))
}

async fn dashboard_app_js() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        include_str!("../../../dashboard/app.js"),
    )
}

async fn dashboard_styles() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../../dashboard/styles.css"),
    )
}

/// Cytoscape.js (MIT), vendored as a single minified file so the typed-graph
/// viewer renders offline with no CDN — honoring local-first (Sigma/Neo4j Bloom
/// are the paid Cloud/Enterprise tier). Served from the binary like the rest of
/// the dashboard.
async fn dashboard_cytoscape() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        include_str!("../../../dashboard/vendor/cytoscape.min.js"),
    )
}

/// fcose force-directed layout (MIT) + its cose-base/layout-base deps, vendored
/// so the graph auto-arranges cleanly offline. Loaded in dependency order before
/// app.js; app.js registers fcose and falls back to built-in cose if absent.
async fn dashboard_layout_base() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        include_str!("../../../dashboard/vendor/layout-base.js"),
    )
}

async fn dashboard_cose_base() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        include_str!("../../../dashboard/vendor/cose-base.js"),
    )
}

async fn dashboard_fcose() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        include_str!("../../../dashboard/vendor/cytoscape-fcose.js"),
    )
}

/// Bind a TCP listener on `addr` and serve until SIGINT/SIGTERM. When
/// `dashboard` is set, the embedded local dashboard is served at `/`.
pub async fn serve(addr: SocketAddr, state: Arc<AppState>, dashboard: bool) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let bound = listener.local_addr().context("read local address")?;
    info!(addr = %bound, dashboard, "localmem HTTP server listening");

    let app = if dashboard {
        router_with_dashboard(state)
    } else {
        router(state)
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve loop")?;
    info!("localmem HTTP server stopped");
    Ok(())
}

/// Resolve on ctrl-c (all platforms) or SIGTERM (unix). Without this, a
/// `kill <pid>` from a service manager produces a non-zero exit and skips
/// any cleanup we add later (e.g. flushing the lexical writer).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "ctrl-c handler install failed");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler install failed");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("ctrl-c received, shutting down"),
        _ = terminate => info!("SIGTERM received, shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tempfile::tempdir;
    use tower::ServiceExt;

    /// Disable the embedder for unit tests: AppState then runs in
    /// lex+facts-only mode (no vectors, no hybrid search) which keeps
    /// the test runtime fast and offline-safe.
    fn force_no_embedder() {
        std::env::set_var("LOCALMEM_MODEL_DIR", "/this/path/does/not/exist");
    }
    fn restore_embedder_env() {
        std::env::remove_var("LOCALMEM_MODEL_DIR");
    }

    #[tokio::test]
    async fn health_returns_ok_true() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        let state = AppState::open(tmp.path()).await.unwrap();
        restore_embedder_env();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["status"], "healthy");
    }

    #[tokio::test]
    async fn dashboard_router_serves_ui_and_both_api_paths() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        let state = AppState::open(tmp.path()).await.unwrap();
        restore_embedder_env();

        // GET / -> the embedded dashboard HTML.
        let resp = router_with_dashboard(state.clone())
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&bytes)
                .to_lowercase()
                .contains("localmem"),
            "/ must serve the dashboard HTML"
        );

        // GET /api/health -> the API under /api so the same-origin dashboard
        // (default base "/api") works without serve.py.
        let resp = router_with_dashboard(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The embedded assets include the multi-tab viewer's Timeline code.
        for (uri, needle) in [("/app.js", "tl-item"), ("/styles.css", "tl-item")] {
            let resp = router_with_dashboard(state.clone())
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri} should serve");
            let bytes = axum::body::to_bytes(resp.into_body(), 512 * 1024)
                .await
                .unwrap();
            assert!(
                String::from_utf8_lossy(&bytes).contains(needle),
                "{uri} must contain the timeline code ({needle})"
            );
        }

        // Bare /health still works (the MCP server calls the core directly).
        let resp = router_with_dashboard(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn version_returns_pkg_version() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        let state = AppState::open(tmp.path()).await.unwrap();
        restore_embedder_env();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn unknown_route_is_404() {
        let tmp = tempdir().unwrap();
        force_no_embedder();
        let state = AppState::open(tmp.path()).await.unwrap();
        restore_embedder_env();
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/no-such-route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn open_creates_home_directory_tree() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("nested").join("home");
        force_no_embedder();
        let _state = AppState::open(&home).await.unwrap();
        restore_embedder_env();
        assert!(home.exists(), "home dir should be created");
        assert!(
            home.join(crate::lexical::LEXICAL_DIR).exists(),
            "lexical index dir should be created on AppState::open"
        );
    }

    // ---- T-119: crash-recovery embed backfill ----------------------------
    //
    // Tests drive `backfill_missing_vectors` directly. It needs no embedder
    // (the embedder lives in the worker, downstream of the channel), so these
    // are deterministic and offline: the backfill's job is purely "diff the
    // event log against the stored vector ids and enqueue the gap".

    fn capture_event(text: &str) -> crate::event::Event {
        use crate::event::{CapturePayload, Event, Source};
        Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
                text: text.into(),
                rewritten_text: None,
                kind: Default::default(),
                mime: None,
                attachments: vec![],
                tags: Default::default(),
                extra: serde_json::Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        )
    }

    #[tokio::test]
    async fn backfill_queues_only_captures_missing_a_vector() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let log = Arc::new(EventLog::open(home).unwrap());
        let e1 = capture_event("first memory about rust");
        let e2 = capture_event("second memory about postgres");
        log.append(&e1).unwrap();
        log.append(&e2).unwrap();

        // e1 already has its vector; e2's never landed (the interrupted-embed
        // case). Backfill must queue exactly e2.
        let store = VectorStore::open(home, EMBEDDING_DIM).await.unwrap();
        store
            .add(
                &e1.id.to_string(),
                &vec![0.0_f32; EMBEDDING_DIM],
                "first memory about rust",
                "{}",
                e1.ts,
            )
            .await
            .unwrap();
        let vectors = Arc::new(Some(store));

        let (tx, mut rx) = mpsc::channel::<EmbedJob>(16);
        let pending = Arc::new(AtomicUsize::new(0));
        let limit = log.byte_len().unwrap();
        backfill_missing_vectors(log.clone(), vectors, tx, pending.clone(), limit).await;

        let mut jobs = Vec::new();
        while let Ok(j) = rx.try_recv() {
            jobs.push(j);
        }
        assert_eq!(jobs.len(), 1, "only the gap capture should be queued");
        assert_eq!(jobs[0].event_id, e2.id.to_string());
        assert_eq!(jobs[0].text, "second memory about postgres");
        assert_eq!(
            pending.load(Ordering::SeqCst),
            1,
            "pending must account for the queued job so /drain waits for it"
        );
    }

    #[tokio::test]
    async fn backfill_ignores_captures_appended_after_the_snapshot() {
        // The race guard: a capture written AFTER the startup boundary belongs
        // to the live write path, not the backfill. Snapshotting the limit
        // before the second append must exclude it.
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let log = Arc::new(EventLog::open(home).unwrap());
        let before = capture_event("present at startup, no vector yet");
        log.append(&before).unwrap();

        // Snapshot the boundary, THEN append a capture that arrives "after open".
        let limit = log.byte_len().unwrap();
        let after = capture_event("written after startup by the live path");
        log.append(&after).unwrap();

        let store = VectorStore::open(home, EMBEDDING_DIM).await.unwrap();
        let vectors = Arc::new(Some(store));
        let (tx, mut rx) = mpsc::channel::<EmbedJob>(16);
        let pending = Arc::new(AtomicUsize::new(0));
        backfill_missing_vectors(log.clone(), vectors, tx, pending.clone(), limit).await;

        let mut jobs = Vec::new();
        while let Ok(j) = rx.try_recv() {
            jobs.push(j);
        }
        assert_eq!(jobs.len(), 1, "only the pre-snapshot capture is backfilled");
        assert_eq!(jobs[0].event_id, before.id.to_string());
        assert_eq!(pending.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn backfill_queues_nothing_when_all_vectors_present() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let log = Arc::new(EventLog::open(home).unwrap());
        let e1 = capture_event("alpha");
        let e2 = capture_event("beta");
        log.append(&e1).unwrap();
        log.append(&e2).unwrap();

        let store = VectorStore::open(home, EMBEDDING_DIM).await.unwrap();
        for e in [&e1, &e2] {
            store
                .add(
                    &e.id.to_string(),
                    &vec![0.0_f32; EMBEDDING_DIM],
                    "x",
                    "{}",
                    e.ts,
                )
                .await
                .unwrap();
        }
        let vectors = Arc::new(Some(store));

        let (tx, mut rx) = mpsc::channel::<EmbedJob>(16);
        let pending = Arc::new(AtomicUsize::new(0));
        let limit = log.byte_len().unwrap();
        backfill_missing_vectors(log.clone(), vectors, tx, pending.clone(), limit).await;

        assert!(
            rx.try_recv().is_err(),
            "a fully-embedded home must queue nothing (idempotent)"
        );
        assert_eq!(pending.load(Ordering::SeqCst), 0);
    }
}
