//! Hardware-aware batched embed-and-write for the rebuild paths.
//!
//! `replay` and `reindex` both re-embed every capture in the log. The naive
//! shape (embed one chunk, `vs.add()` it, repeat) opens one LanceDB
//! transaction per chunk: on a multi-thousand-capture log that is tens of
//! thousands of fragments, which bloats `vectors.lance` several-fold and grinds
//! the rebuild to a crawl on a memory-constrained machine (the exact failure
//! that motivated this module: a per-chunk replay ballooned the store to 857 MB
//! and stalled).
//!
//! [`VectorBatcher`] decouples the two batch sizes that matter:
//!   * `embed_batch` chunks are embedded in ONE BGE forward pass (bounds peak
//!     inference memory: BGE pads to the longest sequence and runs the whole
//!     batch as a single tensor).
//!   * `flush_rows` embedded rows accumulate before ONE `add_many` transaction
//!     (bounds fragment count, hence store bloat, at the cost of buffered RAM).
//!
//! Both sizes come from [`crate::config::ResolvedIndexing`], which auto-tunes
//! from the core count so a laptop and a 16-core server pick different sizes.
//! Batching ACROSS captures (not just within one capture's chunks) is what lets
//! a high-power machine fill a large forward pass even though most captures are
//! a single short chunk.

use crate::config::ResolvedIndexing;
use crate::embed::Embedder;
use crate::vectors::VectorStore;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

/// A chunk waiting to be embedded. Owns its strings so the batcher can hold
/// pending work across many `add` calls without borrowing the source events.
struct Pending {
    eid: String,
    text: String,
    tags: String,
    ts: DateTime<Utc>,
}

/// An embedded chunk waiting to be flushed to the vector store.
struct Ready {
    eid: String,
    vec: Vec<f32>,
    text: String,
    tags: String,
    ts: DateTime<Utc>,
}

/// Accumulates chunks, embeds them in `embed_batch`-sized forward passes, and
/// writes them in `flush_rows`-sized `add_many` transactions. Drives both
/// `replay` and `reindex` so the two rebuild paths share one cohesive,
/// hardware-tuned write strategy. Call [`VectorBatcher::finish`] at the end to
/// drain the remainder; dropping without finishing silently loses buffered
/// rows.
pub struct VectorBatcher<'a> {
    embedder: &'a mut Embedder,
    store: &'a VectorStore,
    tuning: ResolvedIndexing,
    pending: Vec<Pending>,
    ready: Vec<Ready>,
    written: u64,
}

impl<'a> VectorBatcher<'a> {
    pub fn new(
        embedder: &'a mut Embedder,
        store: &'a VectorStore,
        tuning: ResolvedIndexing,
    ) -> Self {
        Self {
            embedder,
            store,
            tuning,
            pending: Vec::with_capacity(tuning.embed_batch),
            ready: Vec::with_capacity(tuning.flush_rows),
            written: 0,
        }
    }

    /// Queue every chunk of one capture under a shared event id, tags, and
    /// timestamp. Triggers an embed pass and/or flush whenever a threshold is
    /// crossed, so peak memory stays bounded regardless of log size.
    pub async fn add_capture(
        &mut self,
        eid: &str,
        chunks: Vec<String>,
        tags: &str,
        ts: DateTime<Utc>,
    ) -> Result<()> {
        for text in chunks {
            self.pending.push(Pending {
                eid: eid.to_string(),
                text,
                tags: tags.to_string(),
                ts,
            });
            if self.pending.len() >= self.tuning.embed_batch {
                self.embed_pending().await?;
            }
        }
        Ok(())
    }

    /// Embed all currently-pending chunks in one forward pass, moving them to
    /// the ready buffer. Flushes if the ready buffer reaches `flush_rows`.
    async fn embed_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let texts: Vec<&str> = self.pending.iter().map(|p| p.text.as_str()).collect();
        let vecs = self
            .embedder
            .embed_batch(&texts)
            .context("embed chunk batch during rebuild")?;
        for (p, vec) in self.pending.drain(..).zip(vecs.into_iter()) {
            self.ready.push(Ready {
                eid: p.eid,
                vec,
                text: p.text,
                tags: p.tags,
                ts: p.ts,
            });
        }
        if self.ready.len() >= self.tuning.flush_rows {
            self.flush().await?;
        }
        Ok(())
    }

    /// Write all ready rows in a single `add_many` transaction.
    async fn flush(&mut self) -> Result<()> {
        if self.ready.is_empty() {
            return Ok(());
        }
        let rows: Vec<(&str, &[f32], &str, &str, DateTime<Utc>)> = self
            .ready
            .iter()
            .map(|r| {
                (
                    r.eid.as_str(),
                    r.vec.as_slice(),
                    r.text.as_str(),
                    r.tags.as_str(),
                    r.ts,
                )
            })
            .collect();
        self.store
            .add_many(&rows)
            .await
            .context("write vector batch during rebuild")?;
        self.written += self.ready.len() as u64;
        self.ready.clear();
        Ok(())
    }

    /// Drain any remaining pending + ready rows and return the total number of
    /// vector rows written across the batcher's life.
    pub async fn finish(mut self) -> Result<u64> {
        self.embed_pending().await?;
        self.flush().await?;
        Ok(self.written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{test_assets, EMBEDDING_DIM};
    use crate::vectors::VectorStore;
    use tempfile::tempdir;

    fn tuning(embed_batch: usize, flush_rows: usize) -> ResolvedIndexing {
        ResolvedIndexing {
            embed_batch,
            flush_rows,
            cores: 1,
        }
    }

    /// The batcher must write exactly one row per chunk across captures, even
    /// when batch boundaries fall mid-capture, and the store must end with the
    /// same count. Tiny embed_batch/flush_rows force many boundary crossings so
    /// the drain logic in `finish` is exercised.
    #[tokio::test]
    async fn batches_across_captures_and_writes_every_chunk() {
        let Some(model_dir) = test_assets::ensure_model() else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        std::env::set_var("LOCALMEM_MODEL_DIR", &model_dir);
        let mut embedder = Embedder::load(&model_dir).unwrap();
        std::env::remove_var("LOCALMEM_MODEL_DIR");

        let tmp = tempdir().unwrap();
        let store = VectorStore::open(tmp.path(), EMBEDDING_DIM).await.unwrap();

        let written = {
            let mut b = VectorBatcher::new(&mut embedder, &store, tuning(3, 5));
            // 4 captures with 1, 2, 1, 3 chunks = 7 rows; thresholds of 3 and 5
            // guarantee mid-capture embed passes and at least one mid-stream
            // flush before the final drain.
            b.add_capture("e1", vec!["alpha one".into()], "{}", Utc::now())
                .await
                .unwrap();
            b.add_capture(
                "e2",
                vec!["beta two".into(), "beta three".into()],
                "{}",
                Utc::now(),
            )
            .await
            .unwrap();
            b.add_capture("e3", vec!["gamma four".into()], "{}", Utc::now())
                .await
                .unwrap();
            b.add_capture(
                "e4",
                vec!["delta five".into(), "delta six".into(), "delta seven".into()],
                "{}",
                Utc::now(),
            )
            .await
            .unwrap();
            b.finish().await.unwrap()
        };

        assert_eq!(written, 7, "every chunk written exactly once");
        assert_eq!(store.count().await.unwrap(), 7, "store row count matches");
    }

    #[tokio::test]
    async fn empty_batcher_writes_nothing() {
        let Some(model_dir) = test_assets::ensure_model() else {
            eprintln!("{}", test_assets::skip_reason());
            return;
        };
        let mut embedder = Embedder::load(&model_dir).unwrap();
        let tmp = tempdir().unwrap();
        let store = VectorStore::open(tmp.path(), EMBEDDING_DIM).await.unwrap();
        let b = VectorBatcher::new(&mut embedder, &store, tuning(8, 256));
        assert_eq!(b.finish().await.unwrap(), 0);
        assert_eq!(store.count().await.unwrap(), 0);
    }
}
