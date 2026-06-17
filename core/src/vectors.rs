//! LanceDB vector store.
//!
//! Holds ANN-indexed embeddings of capture chunks and fact texts. See
//! ARCHITECTURE.md (Derived stores -> `vectors.lance/`) and TASKS.md task
//! T-09.
//!
//! v0.1 stores one row per capture event:
//!
//! | column     | type                              |
//! |------------|-----------------------------------|
//! | event_id   | Utf8                              |
//! | vector     | FixedSizeList<Float32, EMBED_DIM> |
//! | content    | Utf8                              |
//! | ts         | Timestamp(Millisecond, UTC)       |
//!
//! Re-embedding (the `localmem reindex` flow) drops and rebuilds this table
//! from `events.jsonl`. There is no in-place schema migration: the table is
//! a pure derived store per the [ARCHITECTURE.md] trust promise.

use anyhow::{anyhow, Context, Result};
use arrow_array::types::Float32Type;
use arrow_array::{
    Array, FixedSizeListArray, RecordBatch, RecordBatchIterator, RecordBatchReader, StringArray,
    TimestampMillisecondArray,
};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use chrono::{DateTime, TimeZone, Utc};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{connect, Connection, Table};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// On-disk location of the vector store relative to the localmem home.
pub const VECTORS_DIR: &str = "derived/vectors.lance";

/// Single table per home. Naming is fixed so `replay` and the hybrid
/// retriever (T-23) can address it without configuration.
pub const TABLE_NAME: &str = "captures";

/// Schema field names. Centralized so the retriever can decode rows
/// without re-typing string literals.
pub mod fields {
    pub const EVENT_ID: &str = "event_id";
    pub const VECTOR: &str = "vector";
    pub const CONTENT: &str = "content";
    pub const TS: &str = "ts";
    /// SPEC §2.8: the capture's container tags, JSON-encoded, stored ON the
    /// vector row so the vec retrieval path filters by its OWN data (scope,
    /// tag-subset, reserved) instead of borrowing from a lexical lookup that can
    /// diverge. This is the cohesion fix: every retrieval store self-describes.
    pub const TAGS: &str = "tags";
    /// Column LanceDB injects on `nearest_to` queries containing the L2
    /// distance from the query vector. Lower = closer.
    pub const DISTANCE: &str = "_distance";
}

/// Search result emitted by [`VectorStore::search`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorHit {
    pub event_id: String,
    pub content: String,
    pub ts: DateTime<Utc>,
    /// The capture's container tags, decoded from the row's `tags` column.
    #[serde(default)]
    pub tags: std::collections::BTreeMap<String, String>,
    /// Cosine-similarity-like score in `[0, 1]`. Converted from LanceDB's
    /// L2 distance using `1 / (1 + distance)` so callers can blend it with
    /// BM25 (which is unbounded but order-preserving the same way).
    pub score: f32,
}

/// Wrapper around a LanceDB connection pinned to the localmem schema.
pub struct VectorStore {
    path: PathBuf,
    /// Held so the connection stays alive for the table's lifetime.
    #[allow(dead_code)]
    conn: Connection,
    table: Table,
    dim: usize,
}

impl std::fmt::Debug for VectorStore {
    // `lancedb::Connection` and `lancedb::Table` are not `Debug`. Expose
    // the user-visible identity fields so tests and tracing can format a
    // `VectorStore` with `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorStore")
            .field("path", &self.path)
            .field("dim", &self.dim)
            .finish_non_exhaustive()
    }
}

impl VectorStore {
    /// Open (or create) the vector store under `home`. Embedding dimension
    /// is fixed at construction time: it has to match the embedder model.
    /// Passing a mismatched `dim` against an existing table returns an
    /// error rather than silently producing garbage results.
    pub async fn open(home: impl AsRef<Path>, dim: usize) -> Result<Self> {
        let home = home.as_ref();
        let path = home.join(VECTORS_DIR);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("create vector store dir at {}", path.display()))?;

        let uri = path.to_string_lossy().to_string();
        let conn = connect(&uri)
            .execute()
            .await
            .with_context(|| format!("open lancedb at {uri}"))?;

        let table = match conn.open_table(TABLE_NAME).execute().await {
            Ok(t) => {
                // Existing table: verify dim matches. `Table::schema()` returns
                // `Arc<Schema>`; deref before handing to our helper.
                let schema = t.schema().await.context("read existing table schema")?;
                let stored_dim = vector_dim_from_schema(schema.as_ref())?;
                if stored_dim != dim {
                    return Err(anyhow!(
                        "existing vectors.lance dim {stored_dim} does not match \
                         requested dim {dim}; run `localmem reindex` to rebuild"
                    ));
                }
                t
            }
            Err(_) => create_empty_table(&conn, dim).await?,
        };

        Ok(Self {
            path,
            conn,
            table,
            dim,
        })
    }

    /// Filesystem location of the vector store directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Embedding dimensionality stored in the table.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Number of rows in the table. Useful for tests and replay diagnostics.
    pub async fn count(&self) -> Result<usize> {
        let n = self
            .table
            .count_rows(None)
            .await
            .context("count rows in vectors.lance")?;
        Ok(n)
    }

    /// Every `event_id` currently present in the table. T-119 uses this on
    /// startup to find captures whose vector never landed (async embedding
    /// interrupted by a crash/eviction) so they can be re-embedded from
    /// events.jsonl. Projects to the id column only so the scan stays cheap on
    /// a large corpus (no vector payload pulled).
    pub async fn existing_ids(&self) -> Result<std::collections::HashSet<String>> {
        use lancedb::query::Select;
        let stream = self
            .table
            .query()
            .select(Select::columns(&[fields::EVENT_ID]))
            .execute()
            .await
            .context("scan vectors.lance for existing ids")?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .context("collect id batches from vectors.lance")?;
        let mut ids = std::collections::HashSet::new();
        for batch in &batches {
            let col = batch
                .column_by_name(fields::EVENT_ID)
                .ok_or_else(|| anyhow!("id scan batch missing `event_id`"))?
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow!("`event_id` column is not Utf8"))?;
            // `event_id` is a non-nullable column (see vector_schema), so every
            // row has a value.
            for i in 0..batch.num_rows() {
                ids.insert(col.value(i).to_string());
            }
        }
        Ok(ids)
    }

    /// Append one vector row. Idempotency is the caller's responsibility:
    /// LanceDB happily stores duplicates. Replay (T-25) walks the event log
    /// from scratch so duplicates would only matter on a partial rebuild.
    pub async fn add(
        &self,
        event_id: &str,
        vector: &[f32],
        content: &str,
        tags_json: &str,
        ts: DateTime<Utc>,
    ) -> Result<()> {
        if vector.len() != self.dim {
            return Err(anyhow!(
                "vector length {} does not match store dim {}",
                vector.len(),
                self.dim
            ));
        }
        let batch = build_batch(self.dim, &[(event_id, vector, content, tags_json, ts)])?;
        let schema = batch.schema();
        let iter = RecordBatchIterator::new(vec![Ok(batch)].into_iter(), schema);
        // `Table::add` requires `Scannable`, which is only implemented for
        // `Box<dyn RecordBatchReader + Send>`. The concrete iterator type
        // satisfies that trait but does not coerce automatically.
        let reader: Box<dyn RecordBatchReader + Send> = Box::new(iter);
        self.table
            .add(reader)
            .execute()
            .await
            .context("append row to vectors.lance")?;
        Ok(())
    }

    /// Append many vector rows in a single LanceDB write. Used by the async
    /// embedding worker (T-117) so a batch embedded in one ONNX pass is also
    /// persisted in one round-trip. Same idempotency contract as [`Self::add`].
    pub async fn add_many(&self, rows: &[(&str, &[f32], &str, &str, DateTime<Utc>)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        for (event_id, vector, _, _, _) in rows {
            if vector.len() != self.dim {
                return Err(anyhow!(
                    "vector length {} for {event_id} does not match store dim {}",
                    vector.len(),
                    self.dim
                ));
            }
        }
        let batch = build_batch(self.dim, rows)?;
        let schema = batch.schema();
        let iter = RecordBatchIterator::new(vec![Ok(batch)].into_iter(), schema);
        let reader: Box<dyn RecordBatchReader + Send> = Box::new(iter);
        self.table
            .add(reader)
            .execute()
            .await
            .context("append rows to vectors.lance")?;
        Ok(())
    }

    /// k-NN search. Returns up to `k` hits ranked by ascending L2 distance
    /// (i.e. most similar first). `k == 0` short-circuits to an empty Vec
    /// because LanceDB rejects a zero limit.
    pub async fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        if query.len() != self.dim {
            return Err(anyhow!(
                "query vector length {} does not match store dim {}",
                query.len(),
                self.dim
            ));
        }

        let stream = self
            .table
            .query()
            .nearest_to(query.to_vec())
            .context("build vector query")?
            .limit(k)
            .execute()
            .await
            .context("execute vector query")?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .context("collect lancedb result batches")?;

        let mut hits = Vec::new();
        for batch in &batches {
            decode_batch(batch, &mut hits)?;
        }
        Ok(hits)
    }
}

/// Build the Arrow schema for the captures table.
fn vector_schema(dim: usize) -> Schema {
    Schema::new(vec![
        Field::new(fields::EVENT_ID, DataType::Utf8, false),
        Field::new(
            fields::VECTOR,
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            false,
        ),
        Field::new(fields::CONTENT, DataType::Utf8, false),
        // Nullable: a capture may have no tags (the empty map encodes as "{}").
        Field::new(fields::TAGS, DataType::Utf8, true),
        Field::new(
            fields::TS,
            DataType::Timestamp(TimeUnit::Millisecond, Some(Arc::from("UTC"))),
            false,
        ),
    ])
}

fn vector_dim_from_schema(schema: &Schema) -> Result<usize> {
    let f = schema
        .field_with_name(fields::VECTOR)
        .map_err(|e| anyhow!("schema missing `vector` column: {e}"))?;
    match f.data_type() {
        DataType::FixedSizeList(_, n) => Ok(*n as usize),
        other => Err(anyhow!(
            "`vector` column has type {other:?}, expected FixedSizeList"
        )),
    }
}

async fn create_empty_table(conn: &Connection, dim: usize) -> Result<Table> {
    let schema = Arc::new(vector_schema(dim));
    // `create_table` requires a record batch (even an empty one) so it can
    // infer the column types. An empty batch over our schema does the job.
    let empty = RecordBatch::new_empty(schema.clone());
    let iter = RecordBatchIterator::new(vec![Ok(empty)].into_iter(), schema);
    let reader: Box<dyn RecordBatchReader + Send> = Box::new(iter);
    let table = conn
        .create_table(TABLE_NAME, reader)
        .execute()
        .await
        .context("create empty vectors.lance table")?;
    Ok(table)
}

fn build_batch(
    dim: usize,
    rows: &[(&str, &[f32], &str, &str, DateTime<Utc>)],
) -> Result<RecordBatch> {
    let event_ids = StringArray::from(rows.iter().map(|r| r.0).collect::<Vec<_>>());
    let contents = StringArray::from(rows.iter().map(|r| r.2).collect::<Vec<_>>());
    let tags = StringArray::from(rows.iter().map(|r| r.3).collect::<Vec<_>>());

    // FixedSizeListArray builder: a flat Float32Array sliced into `dim`-wide
    // chunks. Each input row must already have exactly `dim` values.
    let flat: Vec<Option<f32>> = rows
        .iter()
        .flat_map(|r| r.1.iter().copied().map(Some))
        .collect();
    let vectors = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        rows.iter()
            .map(|r| Some(r.1.iter().copied().map(Some).collect::<Vec<_>>())),
        dim as i32,
    );
    // Sanity check the builder's output: every row must have `dim` values.
    debug_assert_eq!(vectors.value_length(), dim as i32);
    debug_assert_eq!(flat.len(), rows.len() * dim);

    let timestamps_ms: Vec<i64> = rows.iter().map(|r| r.4.timestamp_millis()).collect();
    let timestamps = TimestampMillisecondArray::from(timestamps_ms).with_timezone("UTC");

    let schema = Arc::new(vector_schema(dim));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(event_ids),
            Arc::new(vectors),
            Arc::new(contents),
            Arc::new(tags),
            Arc::new(timestamps),
        ],
    )
    .context("build vectors.lance record batch")
}

fn decode_batch(batch: &RecordBatch, out: &mut Vec<VectorHit>) -> Result<()> {
    let event_ids = batch
        .column_by_name(fields::EVENT_ID)
        .ok_or_else(|| anyhow!("result batch missing `event_id`"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("`event_id` column is not Utf8"))?;
    let contents = batch
        .column_by_name(fields::CONTENT)
        .ok_or_else(|| anyhow!("result batch missing `content`"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("`content` column is not Utf8"))?;
    let timestamps = batch
        .column_by_name(fields::TS)
        .ok_or_else(|| anyhow!("result batch missing `ts`"))?
        .as_any()
        .downcast_ref::<TimestampMillisecondArray>()
        .ok_or_else(|| anyhow!("`ts` column is not Timestamp(ms, UTC)"))?;
    let distances = batch
        .column_by_name(fields::DISTANCE)
        .ok_or_else(|| anyhow!("result batch missing `_distance`"))?
        .as_any()
        .downcast_ref::<arrow_array::Float32Array>()
        .ok_or_else(|| anyhow!("`_distance` column is not Float32"))?;
    // `tags` is absent on tables written before the §2.8 schema; treat a missing
    // column as no tags so an un-reindexed store still reads (then reindex).
    let tags_col = batch
        .column_by_name(fields::TAGS)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());

    for i in 0..batch.num_rows() {
        let ms = timestamps.value(i);
        let ts = Utc
            .timestamp_millis_opt(ms)
            .single()
            .ok_or_else(|| anyhow!("invalid timestamp ms={ms}"))?;
        let distance = distances.value(i);
        let tags = match tags_col {
            Some(arr) if !arr.is_null(i) => serde_json::from_str(arr.value(i)).unwrap_or_default(),
            _ => std::collections::BTreeMap::new(),
        };
        out.push(VectorHit {
            event_id: event_ids.value(i).to_string(),
            content: contents.value(i).to_string(),
            ts,
            tags,
            score: 1.0 / (1.0 + distance),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const DIM: usize = 4;

    fn vec_of(values: [f32; DIM]) -> Vec<f32> {
        values.to_vec()
    }

    #[tokio::test]
    async fn open_creates_directory_and_table() {
        let tmp = tempdir().unwrap();
        let store = VectorStore::open(tmp.path(), DIM).await.unwrap();
        assert_eq!(store.dim(), DIM);
        assert!(store.path().exists());
        assert_eq!(store.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn add_then_count_increments() {
        let tmp = tempdir().unwrap();
        let store = VectorStore::open(tmp.path(), DIM).await.unwrap();
        store
            .add(
                "01HX1",
                &vec_of([1.0, 0.0, 0.0, 0.0]),
                "alpha",
                "{}",
                Utc::now(),
            )
            .await
            .unwrap();
        store
            .add(
                "01HX2",
                &vec_of([0.0, 1.0, 0.0, 0.0]),
                "beta",
                "{}",
                Utc::now(),
            )
            .await
            .unwrap();
        assert_eq!(store.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn search_returns_nearest_first() {
        let tmp = tempdir().unwrap();
        let store = VectorStore::open(tmp.path(), DIM).await.unwrap();

        store
            .add(
                "near",
                &vec_of([1.0, 0.0, 0.0, 0.0]),
                "near hit",
                "{}",
                Utc::now(),
            )
            .await
            .unwrap();
        store
            .add(
                "far",
                &vec_of([0.0, 0.0, 1.0, 0.0]),
                "far hit",
                "{}",
                Utc::now(),
            )
            .await
            .unwrap();

        let query = vec_of([0.9, 0.1, 0.0, 0.0]);
        let hits = store.search(&query, 2).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].event_id, "near");
        assert_eq!(hits[1].event_id, "far");
        // Near hit must score strictly higher.
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn search_caps_at_k() {
        let tmp = tempdir().unwrap();
        let store = VectorStore::open(tmp.path(), DIM).await.unwrap();
        for i in 0..5 {
            let v = vec_of([i as f32, 0.0, 0.0, 0.0]);
            store
                .add(&format!("ev{i}"), &v, &format!("c{i}"), "{}", Utc::now())
                .await
                .unwrap();
        }
        let hits = store
            .search(&vec_of([0.0, 0.0, 0.0, 0.0]), 3)
            .await
            .unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[tokio::test]
    async fn search_k_zero_short_circuits() {
        let tmp = tempdir().unwrap();
        let store = VectorStore::open(tmp.path(), DIM).await.unwrap();
        store
            .add("a", &vec_of([1.0, 0.0, 0.0, 0.0]), "a", "{}", Utc::now())
            .await
            .unwrap();
        let hits = store
            .search(&vec_of([1.0, 0.0, 0.0, 0.0]), 0)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn add_with_wrong_dim_errors() {
        let tmp = tempdir().unwrap();
        let store = VectorStore::open(tmp.path(), DIM).await.unwrap();
        let err = store
            .add("bad", &[1.0, 2.0, 3.0], "wrong dim", "{}", Utc::now())
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not match"), "unexpected msg: {msg}");
    }

    #[tokio::test]
    async fn search_with_wrong_dim_errors() {
        let tmp = tempdir().unwrap();
        let store = VectorStore::open(tmp.path(), DIM).await.unwrap();
        let err = store.search(&[1.0, 2.0], 5).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not match"), "unexpected msg: {msg}");
    }

    #[tokio::test]
    async fn reopen_preserves_rows() {
        let tmp = tempdir().unwrap();
        {
            let store = VectorStore::open(tmp.path(), DIM).await.unwrap();
            store
                .add(
                    "persist",
                    &vec_of([0.5, 0.5, 0.0, 0.0]),
                    "persist",
                    "{}",
                    Utc::now(),
                )
                .await
                .unwrap();
        }
        let store2 = VectorStore::open(tmp.path(), DIM).await.unwrap();
        assert_eq!(store2.count().await.unwrap(), 1);
        let hits = store2
            .search(&vec_of([0.5, 0.5, 0.0, 0.0]), 1)
            .await
            .unwrap();
        assert_eq!(hits[0].event_id, "persist");
    }

    #[tokio::test]
    async fn reopen_with_mismatched_dim_errors() {
        let tmp = tempdir().unwrap();
        {
            let _ = VectorStore::open(tmp.path(), DIM).await.unwrap();
        }
        let err = VectorStore::open(tmp.path(), DIM + 1).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not match"),
            "expected dim-mismatch error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn thousand_vector_insert_and_search() {
        // T-09 acceptance: 1000-vector insert, search returns expected
        // nearest neighbor.
        let tmp = tempdir().unwrap();
        let store = VectorStore::open(tmp.path(), DIM).await.unwrap();
        let target_id = "target";
        let target = vec_of([0.42, 0.13, 0.71, 0.05]);
        store
            .add(target_id, &target, "the needle", "{}", Utc::now())
            .await
            .unwrap();
        for i in 0..999 {
            // Spread the haystack across the unit cube but well away from
            // the target so it remains the unique nearest neighbor.
            let v = vec_of([
                ((i as f32 * 13.0) % 7.0) - 5.0,
                ((i as f32 * 17.0) % 11.0) - 5.0,
                ((i as f32 * 23.0) % 13.0) - 5.0,
                ((i as f32 * 29.0) % 17.0) - 5.0,
            ]);
            store
                .add(&format!("h{i}"), &v, &format!("hay {i}"), "{}", Utc::now())
                .await
                .unwrap();
        }
        assert_eq!(store.count().await.unwrap(), 1000);
        let hits = store.search(&target, 5).await.unwrap();
        assert_eq!(hits[0].event_id, target_id);
    }
}
