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

use crate::embed::{Embedder, EMBEDDING_DIM};
use crate::event_log::EventLog;
use crate::extractor::ExtractorRegistry;
use crate::facts::FactsStore;
use crate::journal::Journal;
use crate::lexical::{LexicalIndex, LexicalResultExt};
use crate::policy::Policy;
use crate::vectors::VectorStore;
use anyhow::{Context, Result};
use axum::{
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{info, warn};

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
}

impl AppState {
    /// Open every store under `home` and wrap them for shared access.
    /// Idempotent on existing homes.
    pub async fn open(home: impl AsRef<Path>) -> Result<Arc<Self>> {
        let home = home.as_ref().to_path_buf();
        let event_log = EventLog::open(&home).context("open event log")?;
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

        Ok(Arc::new(Self {
            home,
            event_log: Arc::new(event_log),
            lexical: Arc::new(Mutex::new(lexical)),
            facts: Arc::new(Mutex::new(facts)),
            journal: Arc::new(journal),
            policy: Arc::new(policy),
            extractor: Arc::new(extractor),
            embedder: Arc::new(Mutex::new(embedder)),
            vectors: Arc::new(vectors),
        }))
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
        .route("/search", post(routes::search))
        .route("/recall", post(routes::recall))
        .route("/profile", post(routes::profile))
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

/// Bind a TCP listener on `addr` and serve until SIGINT/SIGTERM.
pub async fn serve(addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let bound = listener.local_addr().context("read local address")?;
    info!(addr = %bound, "localmem HTTP server listening");

    axum::serve(listener, router(state))
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
}
