//! `localmem reindex` handler.
//!
//! Re-embeds every capture in `events.jsonl` with the currently
//! configured embedder. See SPEC.md "localmem reindex" and TASKS.md T-41.
//!
//! Unlike `localmem replay`, reindex touches ONLY `vectors.lance/`. It
//! leaves `facts.duckdb`, `lexical.tantivy/`, and `journal.log` untouched
//! so it's safe to run after swapping embedding models without disturbing
//! the rest of derived state. The vector store is dropped and rebuilt
//! using the same atomic rename-then-create-then-cleanup that replay
//! uses for `derived/` as a whole.

use crate::embed::{Embedder, EMBEDDING_DIM};
use crate::event::EventKind;
use crate::event_log::EventLog;
use crate::vectors::{VectorStore, VECTORS_DIR};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tracing::info;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ReindexStats {
    pub events_seen: u64,
    pub captures_seen: u64,
    pub vectors_written: u64,
}

/// Entry point for the `reindex` subcommand. `sample`, when set, stops after
/// that many SIGNAL captures: a fast smoke test that the embed + write path
/// works on this machine before committing to a full multi-thousand-capture
/// rebuild (and that the chosen batch sizes are sane for the hardware).
pub async fn run(home: Option<&str>, as_json: bool, sample: Option<u64>) -> Result<()> {
    let home = resolve_home(home)?;
    let stats = reindex_home_sampled(&home, sample).await?;
    emit(&stats, as_json)
}

/// Rebuild `<home>/derived/vectors.lance/` from scratch (full log). Requires a
/// loaded embedder; a missing model is a fatal error (use `localmem replay`
/// instead if the goal is to drop and rebuild every store, or install the model
/// and re-run this).
///
/// reindex re-embeds CAPTURES only, into vectors.lance. The complete rebuild
/// (facts, lexical, journal, AND the embed-both understanding-summary vectors)
/// is `localmem replay`; prefer it after a model or schema change.
pub async fn reindex_home(home: &Path) -> Result<ReindexStats> {
    reindex_home_sampled(home, None).await
}

/// Like [`reindex_home`] but stops after `sample` signal captures when set.
pub async fn reindex_home_sampled(home: &Path, sample: Option<u64>) -> Result<ReindexStats> {
    let event_log = EventLog::open(home).context("open event log for read")?;

    let model_dir = resolve_model_dir(home);
    let mut embedder = Embedder::load(&model_dir).with_context(|| {
        format!(
            "load embedder from {} (set LOCALMEM_MODEL_DIR to override)",
            model_dir.display()
        )
    })?;

    // Atomic swap of just the vectors directory. See replay.rs for the
    // same pattern applied to derived/ as a whole.
    let vectors_path = home.join(VECTORS_DIR);
    let stash = home.join("vectors.lance.old");
    swap_vectors_dir(&vectors_path, &stash).context("stash existing vectors.lance")?;

    let vector_store = VectorStore::open(home, EMBEDDING_DIM)
        .await
        .context("open fresh vector store")?;

    // Hardware-aware batched embed+write, shared with `replay`. See
    // index_batch.rs for why per-chunk writes bloat the store.
    let tuning = crate::config::Config::load(home)
        .map(|c| c.indexing.resolved())
        .unwrap_or_else(|_| crate::config::IndexingSection::default().resolved());
    info!(
        cores = tuning.cores,
        embed_batch = tuning.embed_batch,
        flush_rows = tuning.flush_rows,
        sample = ?sample,
        "reindex: batched indexing tuned for this machine"
    );
    let mut batcher = crate::index_batch::VectorBatcher::new(&mut embedder, &vector_store, tuning);

    let mut stats = ReindexStats::default();
    for ev_result in event_log.iter().context("open event log iterator")? {
        let event = ev_result.context("read event from event log")?;
        stats.events_seen += 1;
        let EventKind::Capture(payload) = &event.kind else {
            continue;
        };
        stats.captures_seen += 1;
        // Retrieval hygiene parity with replay: ephemeral tool-traces never
        // enter the search surface, so reindex must skip them too or it would
        // re-pollute vectors.lance with the exact noise replay strips.
        if payload.is_ephemeral() {
            continue;
        }
        let tags_json = serde_json::to_string(&payload.tags).unwrap_or_else(|_| "{}".into());
        let chunks = crate::chunk::chunk_text(&payload.text);
        if chunks.is_empty() {
            continue;
        }
        let added = chunks.len() as u64;
        batcher
            .add_capture(&event.id.to_string(), chunks, &tags_json, event.ts)
            .await
            .context("queue capture chunks during reindex")?;
        stats.vectors_written += added;
        if let Some(limit) = sample {
            if stats.vectors_written >= limit {
                break;
            }
        }
    }
    let written = batcher
        .finish()
        .await
        .context("flush vector batcher after reindex")?;
    // `vectors_written` counts queued chunks; reconcile with what actually
    // landed so the stat can never overstate the store (they match in practice).
    stats.vectors_written = written;

    if stash.exists() {
        std::fs::remove_dir_all(&stash)
            .with_context(|| format!("remove stash at {}", stash.display()))?;
    }

    info!(?stats, "reindex complete");
    Ok(stats)
}

fn swap_vectors_dir(vectors_path: &Path, stash: &Path) -> Result<()> {
    if stash.exists() {
        std::fs::remove_dir_all(stash)
            .with_context(|| format!("remove stale stash at {}", stash.display()))?;
    }
    if vectors_path.exists() {
        std::fs::rename(vectors_path, stash)
            .with_context(|| format!("rename {} to {}", vectors_path.display(), stash.display()))?;
    }
    Ok(())
}

fn emit(stats: &ReindexStats, as_json: bool) -> Result<()> {
    if as_json {
        let json = serde_json::json!({
            "ok": true,
            "events": stats.events_seen,
            "captures": stats.captures_seen,
            "vectors_written": stats.vectors_written,
        });
        println!("{json}");
    } else {
        println!(
            "reindex: events={} captures={} vectors_written={}",
            stats.events_seen, stats.captures_seen, stats.vectors_written
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
    use crate::cli::init::init_home;
    use crate::embed::test_assets;
    use crate::event::{CapturePayload, Event, EventKind, Source};
    use serde_json::Map;
    use tempfile::tempdir;

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

    /// Combined to dodge the LOCALMEM_MODEL_DIR race: cargo runs unit
    /// tests in parallel, std::env::set_var is process-wide, and one
    /// reindex test wants the var unset (or invalid) while the other
    /// wants it pointing at a real model. Sequencing both in one
    /// `#[tokio::test]` makes the env mutation observable.
    #[tokio::test]
    async fn reindex_errors_then_succeeds_after_model_install() {
        // Phase 1: model unavailable -> clear error.
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        std::env::set_var("LOCALMEM_MODEL_DIR", "/no/such/dir");
        let err = reindex_home(tmp.path()).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("load embedder") || msg.contains("missing model"),
            "expected embedder error, got: {msg}"
        );

        // Phase 2: model available -> reindex walks captures only.
        let Some(model_dir) = test_assets::ensure_model() else {
            std::env::remove_var("LOCALMEM_MODEL_DIR");
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        std::env::set_var("LOCALMEM_MODEL_DIR", &model_dir);
        let tmp2 = tempdir().unwrap();
        init_home(tmp2.path()).unwrap();
        let log = EventLog::open(tmp2.path()).unwrap();
        log.append(&capture("hello")).unwrap();
        log.append(&capture("world")).unwrap();
        drop(log);

        let stats = reindex_home(tmp2.path()).await.unwrap();
        std::env::remove_var("LOCALMEM_MODEL_DIR");
        assert_eq!(stats.captures_seen, 2);
        assert_eq!(stats.vectors_written, 2);
        let vs = VectorStore::open(tmp2.path(), EMBEDDING_DIM).await.unwrap();
        assert_eq!(vs.count().await.unwrap(), 2);
    }
}
