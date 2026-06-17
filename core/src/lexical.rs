//! Tantivy-based BM25 lexical index.
//!
//! See ARCHITECTURE.md (Derived stores -> `lexical.tantivy`). Implementation
//! tasks: T-05 (index setup + schema), T-06 (BM25 search), T-07 (CLI wiring).
//!
//! Why a lexical index alongside vectors: pure embedding search misses
//! exact-term recall (URLs, function names, error codes, ULIDs). Hybrid
//! retrieval (T-23) weights BM25 scores with ANN scores at query time.

use crate::event::{Event, EventKind};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    DateOptions, Field, OwnedValue, Schema, Value, INDEXED, STORED, STRING, TEXT,
};
use tantivy::{
    DateTime, Index, IndexReader, IndexWriter, ReloadPolicy, SnippetGenerator, TantivyDocument,
};

/// On-disk location of the index relative to the localmem home directory.
pub const LEXICAL_DIR: &str = "derived/lexical.tantivy";

/// Writer heap budget. 50 MB is the tantivy-recommended minimum for a
/// single-writer setup; lower values trade indexing speed for memory.
const WRITER_HEAP_BYTES: usize = 50_000_000;

/// Maximum characters in a search-result snippet (T-06).
const SNIPPET_MAX_CHARS: usize = 200;

/// Schema version stamped on the on-disk lexical index. Bumped any time
/// `build_schema()` changes (field added, type changed, indexing options
/// changed). On open, the on-disk version is compared to this constant:
/// a mismatch is surfaced as [`LexicalError::SchemaDrift`] so the caller
/// can render the actionable "run: localmem replay" hint instead of
/// letting Tantivy's opaque "Schema error" bubble up.
///
/// The version is persisted in a sibling file
/// (`<home>/derived/lexical.tantivy.version`) so it lives next to the
/// tantivy dir without confusing tantivy's directory scan, and so
/// `replay::swap_derived` (which renames the whole `derived/`) carries
/// or recreates it automatically.
///
/// History:
/// - v1 (2026-06): introduced as part of the field-feedback self-heal
///   fix. First version that explicitly marks the on-disk format.
///   Pre-versioning installs read as `None` and are surfaced as drift
///   so they go through one replay to gain a marker.
pub const LEXICAL_SCHEMA_VERSION: u32 = 1;

/// Path to the schema version sidecar, relative to the home directory.
/// Sibling of [`LEXICAL_DIR`] rather than a child so tantivy's directory
/// scan never sees it.
const SCHEMA_VERSION_FILE: &str = "derived/lexical.tantivy.version";

/// BM25 hit returned by [`LexicalIndex::search`]. `ts` and `tags` are
/// the source capture's stored metadata; callers use them to apply
/// reserved-tag rules (T-51c retention/visibility) without a second
/// lookup. Both are populated from the doc fields the indexer wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LexicalHit {
    pub event_id: String,
    pub snippet: String,
    pub score: f32,
    /// Capture timestamp (second precision; see `index_event`).
    /// Defaults to the epoch when absent on legacy docs.
    #[serde(default)]
    pub ts: chrono::DateTime<chrono::Utc>,
    /// Capture's container tags (empty when the capture had none).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    /// Capture's kind taxonomy slot (T-52). Empty on legacy docs
    /// indexed before T-73 added the field — callers treat that as
    /// `Kind::Note` (the default) when scoring kind-aware bonuses.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// T-52b: latest `done` state for `Kind::Todo` captures. Defaults
    /// to `false` (legacy docs, non-todo captures, todos that haven't
    /// been completed). Flipped by `UpdateCapture` events through
    /// [`LexicalIndex::apply_capture_update`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

/// Errors that callers may want to handle specifically. Today only
/// schema-drift detection is structured; other failures continue to
/// surface as plain `anyhow::Error` with `.context()` chains.
///
/// The CLI catches this via `anyhow::Error::downcast::<LexicalError>()`
/// so the user-facing top-line message is the actionable hint, not a
/// generic "open lexical index" wrapper.
#[derive(Debug)]
pub enum LexicalError {
    /// The on-disk lexical index was written by a different schema
    /// version than the running binary expects. The fix is to rebuild
    /// from the event log via `localmem replay`. `found = None` means
    /// the index pre-dates schema versioning and we cannot verify
    /// compatibility, so we treat it as drift.
    SchemaDrift { found: Option<u32>, current: u32 },
}

impl std::fmt::Display for LexicalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SchemaDrift { found, current } => {
                let found_str = found
                    .map(|v| format!("v{v}"))
                    .unwrap_or_else(|| "unversioned (pre-v0.2.x install)".to_string());
                write!(
                    f,
                    "lexical index schema is stale (on-disk {found_str}, binary v{current}). \
                     Run: localmem replay to rebuild derived stores from events.jsonl (safe; \
                     the event log is the source of truth)"
                )
            }
        }
    }
}

impl std::error::Error for LexicalError {}

/// Extension trait that preserves [`LexicalError::SchemaDrift`] as the
/// top-line error while attaching `context` to any other failure mode.
/// Lets every CLI open site present the actionable hint without losing
/// context-chain debugging for unrelated failures.
///
/// Usage:
/// ```ignore
/// let idx = LexicalIndex::open_reader_only(&home)
///     .lex_context("open lexical index for reading")?;
/// ```
pub trait LexicalResultExt<T> {
    fn lex_context(self, context: &'static str) -> Result<T>;
}

impl<T> LexicalResultExt<T> for Result<T> {
    fn lex_context(self, context: &'static str) -> Result<T> {
        self.map_err(|e| {
            if e.downcast_ref::<LexicalError>().is_some() {
                e
            } else {
                e.context(context)
            }
        })
    }
}

/// Read the schema-version sidecar at `<home>/derived/lexical.tantivy.version`.
/// Returns `Ok(None)` when the file is absent (fresh install or
/// pre-versioning install); `Err` only on a real I/O failure or a
/// malformed file.
fn read_schema_version(home: &Path) -> Result<Option<u32>> {
    let path = home.join(SCHEMA_VERSION_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let trimmed = s.trim();
            let v: u32 = trimmed
                .parse()
                .with_context(|| format!("parse schema version from {}", path.display()))?;
            Ok(Some(v))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(anyhow::Error::new(e).context(format!("read schema version at {}", path.display())))
        }
    }
}

/// Write the schema-version sidecar. Ensures the parent dir exists so
/// callers do not need to mkdir separately on a fresh home.
fn write_schema_version(home: &Path, v: u32) -> Result<()> {
    let path = home.join(SCHEMA_VERSION_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir for {}", path.display()))?;
    }
    std::fs::write(&path, format!("{v}\n"))
        .with_context(|| format!("write schema version to {}", path.display()))
}

/// Wrapper around a Tantivy index pinned to the localmem schema.
///
/// Two open modes:
/// - [`LexicalIndex::open`] — full read+write. Acquires Tantivy's
///   directory-level writer lock. Only one writer per process tree
///   may be live; the server uses this.
/// - [`LexicalIndex::open_reader_only`] — search-only. Does NOT
///   acquire the writer lock, so it coexists with a running server
///   that holds the writer. The CLI search path uses this so
///   `localmem search` works while `localmem serve` is up.
///
/// The two modes share the same struct; `writer` is `None` in
/// reader-only mode. Write methods ([`Self::index_event`],
/// [`Self::commit`]) return a clear error when called on a
/// reader-only handle rather than silently dropping the write.
pub struct LexicalIndex {
    path: PathBuf,
    index: Index,
    writer: Option<IndexWriter>,
    reader: IndexReader,
    fields: Fields,
}

#[derive(Clone, Copy)]
struct Fields {
    event_id: Field,
    content: Field,
    source: Field,
    ts: Field,
    /// Container tags as a JSON-encoded string. STORED-only, no
    /// indexing: filters are applied post-search by overfetching and
    /// dropping non-matching hits. Per the T-51 prompt, this is
    /// "JSON-stringified for now"; richer indexing (faceted,
    /// multi-valued) is a follow-up task.
    tags_json: Field,
    /// Capture kind taxonomy (T-52). STORED-only — the retriever
    /// reads it post-search to apply per-kind recency decay (T-73).
    kind: Field,
    /// T-52b: mutable done state for `Kind::Todo` captures. STORED
    /// only (as u64: 1=done, 0=open). Absent on legacy docs; the
    /// read path treats absence as `false`. Mutated post-write only
    /// via [`LexicalIndex::apply_capture_update`].
    done: Field,
}

impl LexicalIndex {
    /// Open (or create) the lexical index at `<home>/derived/lexical.tantivy/`
    /// in read+write mode.
    ///
    /// Creates an [`IndexWriter`] which acquires Tantivy's directory-level
    /// lock. Only one writer per process tree may be live at a time.
    /// Callers that ONLY need to search should use
    /// [`Self::open_reader_only`] instead so they coexist with a running
    /// server.
    pub fn open(home: impl AsRef<Path>) -> Result<Self> {
        Self::open_internal(home, true)
    }

    /// Open the lexical index in search-only mode.
    ///
    /// Does NOT acquire the Tantivy writer lock. Multiple reader-only
    /// handles can coexist with each other and with one writer handle
    /// in another process. This is the path the CLI search uses so it
    /// works while `localmem serve` is running.
    ///
    /// Calling [`Self::index_event`] or [`Self::commit`] on a
    /// reader-only handle returns an error rather than silently
    /// dropping the write.
    pub fn open_reader_only(home: impl AsRef<Path>) -> Result<Self> {
        Self::open_internal(home, false)
    }

    fn open_internal(home: impl AsRef<Path>, with_writer: bool) -> Result<Self> {
        let home = home.as_ref();
        let path = home.join(LEXICAL_DIR);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("create lexical index dir at {}", path.display()))?;

        // Schema-drift gate. If tantivy's `meta.json` is already on disk
        // an index was initialised at some point; we only proceed when
        // the sidecar version matches LEXICAL_SCHEMA_VERSION. A fresh
        // dir (no meta.json) skips the check and we stamp the marker
        // after `Index::open_or_create` succeeds.
        //
        // Without this gate the failure mode is the field-feedback
        // P0: tantivy returns a generic "Schema error: 'An index
        // exists but the schema does not match.'" with no actionable
        // recovery hint, and the next read dead-ends.
        let meta_path = path.join("meta.json");
        let has_existing_index = meta_path.exists();
        let on_disk_version = read_schema_version(home)?;
        if has_existing_index && on_disk_version != Some(LEXICAL_SCHEMA_VERSION) {
            return Err(LexicalError::SchemaDrift {
                found: on_disk_version,
                current: LEXICAL_SCHEMA_VERSION,
            }
            .into());
        }

        let schema = build_schema();
        let dir = tantivy::directory::MmapDirectory::open(&path)
            .with_context(|| format!("open mmap directory at {}", path.display()))?;
        let index = match Index::open_or_create(dir, schema.clone()) {
            Ok(idx) => idx,
            Err(e) => {
                // Defense in depth: an index whose `meta.json` drifted
                // at the tantivy layer still surfaces here even when
                // our sidecar said the version was current (e.g. user
                // hand-edited the sidecar). Convert to SchemaDrift so
                // callers render the actionable hint.
                let msg = format!("{e}");
                if msg.contains("schema does not match") || msg.contains("Schema error") {
                    return Err(LexicalError::SchemaDrift {
                        found: on_disk_version,
                        current: LEXICAL_SCHEMA_VERSION,
                    }
                    .into());
                }
                return Err(anyhow::Error::new(e).context("open or create tantivy index"));
            }
        };

        // First successful open in a fresh dir: stamp the version
        // marker so future opens can detect drift. Writing here (not
        // before `Index::open_or_create`) keeps the sidecar consistent
        // with tantivy actually having materialised `meta.json`.
        if !has_existing_index {
            write_schema_version(home, LEXICAL_SCHEMA_VERSION)
                .context("stamp lexical schema version after first open")?;
        }

        let writer = if with_writer {
            Some(
                index
                    .writer(WRITER_HEAP_BYTES)
                    .context("create tantivy index writer")?,
            )
        } else {
            None
        };
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .context("build tantivy index reader")?;

        let fields = Fields {
            event_id: get_field(&schema, "event_id")?,
            content: get_field(&schema, "content")?,
            source: get_field(&schema, "source")?,
            ts: get_field(&schema, "ts")?,
            tags_json: get_field(&schema, "tags")?,
            kind: get_field(&schema, "kind")?,
            done: get_field(&schema, "done")?,
        };

        Ok(Self {
            path,
            index,
            writer,
            reader,
            fields,
        })
    }

    /// Filesystem location of the index directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Index a single event.
    ///
    /// v0.1 only indexes `capture` events. Other kinds (`fact`, `update`,
    /// `forget`, `policy`, `import`) are no-ops here. Indexing facts as
    /// separate documents is deferred to T-23 (hybrid retriever).
    pub fn index_event(&mut self, event: &Event) -> Result<()> {
        let EventKind::Capture(payload) = &event.kind else {
            return Ok(());
        };
        // Index the capture's VALID-TIME (when it actually occurred) via the
        // canonical helper, not the recorded-at `event.ts`. The facts path
        // already sources valid_from from this; the lex index must agree or
        // imported/dated history (life-import, a benchmark haystack) loses its
        // real time and temporal reasoning / valid-time recency break.
        // Truncate to second precision: tantivy's DateTime is precision-aware
        // and our `ts` queries operate at coarser granularity than the log.
        let ts =
            DateTime::from_timestamp_secs(payload.effective_capture_instant(event.ts).timestamp());
        let mut doc = TantivyDocument::default();
        doc.add_text(self.fields.event_id, event.id.to_string());
        // T-55: index the context-rewritten text when present, else
        // the original. Snippets returned from `search` will match
        // whichever the user sees in `localmem recall` output.
        doc.add_text(self.fields.content, payload.indexable_text());
        doc.add_text(self.fields.source, &event.source.app);
        doc.add_date(self.fields.ts, ts);
        // Only write the tags field when there are tags. An empty map
        // serializes to "{}" which we'd rather not store on every
        // capture; the read path treats absence as an empty map.
        if !payload.tags.is_empty() {
            let json =
                serde_json::to_string(&payload.tags).context("serialize capture tags to JSON")?;
            doc.add_text(self.fields.tags_json, &json);
        }
        // T-73: persist the capture's kind so the retriever can apply
        // per-kind recency decay without re-deriving from the event
        // log. Default-kind (Note) is still stored verbatim — the
        // retriever falls back to the uniform tau when the stored
        // string is empty (legacy docs) OR resolves to Kind::Other,
        // so writing every kind keeps replay deterministic.
        doc.add_text(self.fields.kind, payload.kind.as_str());
        // T-52b: todo captures land with done=0 by default. The flag
        // flips later via `apply_capture_update` when an
        // `UpdateCapture` event is emitted. Storing it on every
        // capture (not just todos) keeps the schema stable and lets
        // future kinds opt into the same mutable-metadata flow.
        doc.add_u64(self.fields.done, 0);
        let writer = self.writer.as_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "LexicalIndex was opened reader-only; cannot index_event. \
                 Use LexicalIndex::open() (writer mode) for the indexing path."
            )
        })?;
        writer
            .add_document(doc)
            .context("add document to tantivy index")?;
        Ok(())
    }

    /// T-52b: re-apply mutable metadata from an `UpdateCapture`
    /// event to the lex index. Today only the `done` flag flips;
    /// future fields on `UpdateCapturePayload` (pinned, archived,
    /// ...) land here additively.
    ///
    /// Strategy: look up the target capture's current doc, copy all
    /// stored fields verbatim, swap in the new `done` value, delete
    /// the old doc by `event_id` term, and re-add. Tantivy gives us
    /// no partial-update primitive — delete + re-add is the standard
    /// pattern for mutable docs and stays atomic per commit.
    ///
    /// Returns Ok(()) if the target capture isn't in the index
    /// (legacy data; the lex layer doesn't carry every event the
    /// log has). Callers that need the rebuild to be loud should
    /// observe via `meta_for(target_id)` afterwards.
    pub fn apply_capture_update(
        &mut self,
        payload: &crate::event::UpdateCapturePayload,
    ) -> Result<()> {
        use tantivy::schema::Value;
        let target_id_str = payload.target_id.to_string();
        let searcher = self.reader.searcher();
        let term = tantivy::Term::from_field_text(self.fields.event_id, &target_id_str);
        let top = searcher
            .search(
                &tantivy::query::TermQuery::new(
                    term.clone(),
                    tantivy::schema::IndexRecordOption::Basic,
                ),
                &TopDocs::with_limit(1),
            )
            .context("lookup capture doc for update")?;
        let Some((_, address)) = top.first() else {
            // Target capture isn't in the lex index. Likely a non-capture
            // event id or a capture that pre-dates the lex layer; the
            // event log is the source of truth, so we silently skip.
            return Ok(());
        };
        let old: TantivyDocument = searcher
            .doc(*address)
            .context("retrieve old doc for update")?;

        // Rebuild the doc verbatim from the old one, swapping in the
        // new mutable fields. Anything we don't recognise here is
        // dropped — currently the schema is closed (every field is
        // in `Fields`), so this is exhaustive.
        let mut new_doc = TantivyDocument::default();
        if let Some(v) = old.get_first(self.fields.event_id).and_then(|v| v.as_str()) {
            new_doc.add_text(self.fields.event_id, v);
        }
        if let Some(v) = old.get_first(self.fields.content).and_then(|v| v.as_str()) {
            new_doc.add_text(self.fields.content, v);
        }
        if let Some(v) = old.get_first(self.fields.source).and_then(|v| v.as_str()) {
            new_doc.add_text(self.fields.source, v);
        }
        if let Some(td) = old.get_first(self.fields.ts).and_then(|v| v.as_datetime()) {
            new_doc.add_date(self.fields.ts, td);
        }
        if let Some(v) = old
            .get_first(self.fields.tags_json)
            .and_then(|v| v.as_str())
        {
            new_doc.add_text(self.fields.tags_json, v);
        }
        if let Some(v) = old.get_first(self.fields.kind).and_then(|v| v.as_str()) {
            new_doc.add_text(self.fields.kind, v);
        }
        // T-52b: swap in the new `done` value when the payload sets
        // one; otherwise carry the old value forward. None ⇒ "no
        // change" lets future fields on UpdateCapturePayload land
        // without forcing every event to specify done.
        let new_done = match payload.done {
            Some(b) => b as u64,
            None => doc_u64(&old, self.fields.done),
        };
        new_doc.add_u64(self.fields.done, new_done);

        let writer = self.writer.as_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "LexicalIndex was opened reader-only; cannot apply_capture_update. \
                 Use LexicalIndex::open() (writer mode) for the indexing path."
            )
        })?;
        writer.delete_term(term);
        writer
            .add_document(new_doc)
            .context("add updated document to tantivy index")?;
        Ok(())
    }

    /// Persist buffered writes. Without this call, search (T-06) cannot see
    /// documents added since the last commit.
    pub fn commit(&mut self) -> Result<()> {
        let writer = self.writer.as_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "LexicalIndex was opened reader-only; cannot commit. \
                 Use LexicalIndex::open() (writer mode) for the indexing path."
            )
        })?;
        writer.commit().context("commit tantivy writer")?;
        // Force the reader to pick up the new segment immediately. Without
        // this, callers that commit-then-search in the same tick can see
        // stale results until the background reload fires.
        self.reader.reload().context("reload tantivy reader")?;
        Ok(())
    }

    /// Number of committed documents in the index. Exposed so tests (and
    /// future replay diagnostics) can verify indexing without going through
    /// the search path.
    pub fn doc_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// BM25 search across capture content. Returns up to `k` hits in
    /// descending score order. T-06.
    ///
    /// Tantivy's `QueryParser` is set to conjunctive default: a multi-term
    /// query requires every term to appear, matching the BM25 "all terms"
    /// intuition. Users can still express disjunction via the `OR` operator.
    ///
    /// `tag_filter`, when `Some`, is a subset-match predicate: a hit
    /// passes when every `(key, value)` in the filter is present on the
    /// capture's tags. `None` (the default) skips post-filtering. When
    /// the filter is set, we overfetch by [`TAG_FILTER_OVERFETCH`] so
    /// the returned slice has a reasonable chance of carrying `k`
    /// surviving hits; we still truncate the result to `k`.
    pub fn search(
        &self,
        query: &str,
        k: usize,
        tag_filter: Option<&BTreeMap<String, String>>,
    ) -> Result<Vec<LexicalHit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let active_filter = tag_filter.filter(|m| !m.is_empty());
        let searcher = self.reader.searcher();
        let mut parser = QueryParser::for_index(&self.index, vec![self.fields.content]);
        parser.set_conjunction_by_default();
        let parsed = parser
            .parse_query(query)
            .with_context(|| format!("parse query: {query}"))?;

        let fetch = if active_filter.is_some() {
            k.saturating_mul(TAG_FILTER_OVERFETCH)
        } else {
            k
        };
        let top_docs = searcher
            .search(&parsed, &TopDocs::with_limit(fetch))
            .context("execute search")?;

        // SnippetGenerator may fail on queries it cannot highlight (e.g. a
        // pure phrase query against a non-positional field). Fall back to a
        // plain head-of-content snippet in that case rather than failing the
        // search.
        let snippet_gen = SnippetGenerator::create(&searcher, parsed.as_ref(), self.fields.content)
            .ok()
            .map(|mut g| {
                g.set_max_num_chars(SNIPPET_MAX_CHARS);
                g
            });

        let mut hits = Vec::with_capacity(top_docs.len().min(k));
        for (score, address) in top_docs {
            let doc: TantivyDocument = searcher
                .doc(address)
                .context("retrieve document by address")?;
            // Decode tags once: needed for the optional subset-match
            // filter AND for the returned hit (so callers can apply
            // reserved-tag rules without a second lex lookup).
            let doc_tags_map = doc_tags(&doc, self.fields.tags_json)?;
            if let Some(filter) = active_filter {
                if !crate::tag_match::matches(&doc_tags_map, filter) {
                    continue;
                }
            }
            let event_id = string_field(&doc, self.fields.event_id);
            let content = string_field(&doc, self.fields.content);
            let snippet = snippet_gen
                .as_ref()
                .map(|g| g.snippet_from_doc(&doc))
                .filter(|s| !s.fragment().is_empty())
                .map(|s| s.fragment().to_string())
                .unwrap_or_else(|| first_chars(&content, SNIPPET_MAX_CHARS));
            let ts = doc_ts(&doc, self.fields.ts);
            let kind = string_field(&doc, self.fields.kind);
            let done = doc_u64(&doc, self.fields.done) == 1;
            hits.push(LexicalHit {
                event_id,
                snippet,
                score,
                ts,
                tags: doc_tags_map,
                kind,
                done,
            });
            if hits.len() == k {
                break;
            }
        }
        Ok(hits)
    }

    /// Return the tags + capture timestamp stored against the given
    /// capture event, or [`HitMeta::default`] (empty tags, epoch) if
    /// the event isn't in the index. Used by the hybrid retriever
    /// (T-51 + T-51c) to filter vector-only hits without a second
    /// lookup against the event log: the vector store carries neither
    /// tag metadata nor a timestamp.
    ///
    /// Looks up by an exact `event_id` term against the STRING-indexed
    /// `event_id` field, so this is O(log n) on the index, not O(n).
    /// Look up a capture's meta by event id. Returns `None` when the id is NOT
    /// in the lexical index (a vector hit whose capture was never lex-indexed, or
    /// a vectors/lex divergence). The caller MUST distinguish this miss from a
    /// found-but-untagged capture: under an active scope or tag filter, an
    /// unverifiable hit is excluded (fail-closed), never leaked as "global."
    pub fn meta_for(&self, event_id: &str) -> Result<Option<HitMeta>> {
        let searcher = self.reader.searcher();
        let term = tantivy::Term::from_field_text(self.fields.event_id, event_id);
        let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        let top = searcher
            .search(&query, &TopDocs::with_limit(1))
            .context("lookup meta by event_id")?;
        let Some((_, address)) = top.first() else {
            return Ok(None);
        };
        let doc: TantivyDocument = searcher
            .doc(*address)
            .context("retrieve document for meta lookup")?;
        let tags = doc_tags(&doc, self.fields.tags_json)?;
        let ts = doc_ts(&doc, self.fields.ts);
        let kind = string_field(&doc, self.fields.kind);
        let done = doc_u64(&doc, self.fields.done) == 1;
        Ok(Some(HitMeta {
            tags,
            ts,
            kind,
            done,
        }))
    }
}

/// Bundle of stored capture metadata returned by [`LexicalIndex::meta_for`].
/// Tags drive the subset-match filter (T-51); `ts` is the capture time
/// used by the reserved-tag retention check (T-51c).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HitMeta {
    pub tags: BTreeMap<String, String>,
    pub ts: chrono::DateTime<chrono::Utc>,
    /// Kind taxonomy slot (T-52, T-73). Empty on legacy captures
    /// indexed before T-73; the retriever treats empty as
    /// [`crate::kind::Kind::Note`] (the default).
    pub kind: String,
    /// T-52b: latest `done` state for todo captures. Defaults to
    /// `false` for legacy docs and non-todo captures.
    pub done: bool,
}

fn doc_ts(doc: &TantivyDocument, field: Field) -> chrono::DateTime<chrono::Utc> {
    use tantivy::schema::Value;
    // Stored as Tantivy DateTime (second precision). On read we
    // convert to chrono::DateTime<Utc>. Missing field defaults to
    // epoch, which keeps `is_visible` predictable on legacy docs
    // that somehow lack a ts.
    let Some(v) = doc.get_first(field) else {
        return chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap_or_default();
    };
    let Some(td) = v.as_datetime() else {
        return chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap_or_default();
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp(td.into_timestamp_secs(), 0).unwrap_or_default()
}

/// Overfetch multiplier applied when a tag filter is set on
/// [`LexicalIndex::search`]. 4 strikes a balance: large enough to absorb
/// typical filter selectivities (project tags hit ~25-50% of corpus)
/// without ballooning lex passes when filters are highly selective.
const TAG_FILTER_OVERFETCH: usize = 4;

/// Decode the JSON-encoded tags field. Absent or empty values both
/// resolve to an empty map; only malformed JSON returns an error.
fn doc_tags(doc: &TantivyDocument, field: Field) -> Result<BTreeMap<String, String>> {
    let Some(raw) = doc.get_first(field).and_then(owned_value_as_str) else {
        return Ok(BTreeMap::new());
    };
    if raw.is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str(raw).context("parse stored tags JSON")
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    // event_id: opaque ULID. STRING (untokenized) + STORED for round-trip.
    builder.add_text_field("event_id", STRING | STORED);
    // content: BM25-indexed with the default tokenizer (lowercase + simple).
    builder.add_text_field("content", TEXT | STORED);
    // source: app name, used for filtering. STRING (no tokenization).
    builder.add_text_field("source", STRING | STORED);
    // ts: indexed + stored. Fast for future range queries (e.g. --at-time).
    let date_opts = DateOptions::from(INDEXED).set_stored().set_fast();
    builder.add_date_field("ts", date_opts);
    // tags: JSON-encoded BTreeMap stored as opaque text. Not indexed.
    // Filtering happens at search time by overfetching and dropping
    // non-matching hits (T-51).
    builder.add_text_field("tags", STORED);
    // kind: stored only, not indexed (the retriever reads it
    // post-search to apply per-kind recency decay, T-73). STRING
    // keeps round-trip exact for `Kind::Other` extension values.
    builder.add_text_field("kind", STORED);
    // done: T-52b. STORED-only u64 (1 = done, 0 = open). Absent on
    // legacy docs and skipped for non-todo captures. The read path
    // treats absence as `false`.
    builder.add_u64_field("done", STORED);
    builder.build()
}

fn get_field(schema: &Schema, name: &str) -> Result<Field> {
    schema
        .get_field(name)
        .with_context(|| format!("missing field {name} in lexical schema"))
}

/// T-52b: read a u64 field, defaulting to 0 when the doc didn't
/// write one (e.g. legacy captures indexed before T-52b added the
/// `done` field). Matches `doc_ts`'s "absent ⇒ default" discipline so
/// reads stay backward-compatible across schema additions.
fn doc_u64(doc: &TantivyDocument, field: Field) -> u64 {
    use tantivy::schema::Value;
    doc.get_first(field).and_then(|v| v.as_u64()).unwrap_or(0)
}

fn string_field(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(owned_value_as_str)
        .unwrap_or_default()
        .to_string()
}

fn owned_value_as_str(v: &OwnedValue) -> Option<&str> {
    v.as_str()
}

fn first_chars(s: &str, n: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= n {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CapturePayload, EventKind, FactPayload, Source};
    use crate::event_id::EventId;
    use chrono::Utc;
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

    #[test]
    fn open_creates_directory_and_index_files() {
        let tmp = tempdir().unwrap();
        let idx = LexicalIndex::open(tmp.path()).unwrap();
        let dir = tmp.path().join(LEXICAL_DIR);
        assert!(dir.exists(), "index dir should be created");
        // meta.json is tantivy's manifest; its presence proves the index was
        // initialized rather than just the parent dir being mkdir'd.
        assert!(dir.join("meta.json").exists(), "tantivy meta.json missing");
        assert_eq!(idx.path(), dir);
    }

    #[test]
    fn index_event_then_commit_increases_doc_count() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        assert_eq!(idx.doc_count(), 0);
        idx.index_event(&capture("hello world")).unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.doc_count(), 1);
    }

    #[test]
    fn reopen_preserves_indexed_documents() {
        let tmp = tempdir().unwrap();
        {
            let mut idx = LexicalIndex::open(tmp.path()).unwrap();
            idx.index_event(&capture("hello world")).unwrap();
            idx.commit().unwrap();
        }
        // Reopening must not fail or wipe state.
        let idx = LexicalIndex::open(tmp.path()).unwrap();
        assert_eq!(idx.doc_count(), 1);
    }

    #[test]
    fn non_capture_events_are_skipped() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        let fact_event = Event::new(
            EventKind::Fact(FactPayload {
                subject: "user".into(),
                predicate: "prefers".into(),
                object: "rust".into(),
                confidence: 0.9,
                valid_from: Utc::now(),
                valid_to: None,
                derived_from: vec![EventId::new()],
                kind: Default::default(),
                tags: Default::default(),
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "h".into(),
                user: None,
            },
        );
        idx.index_event(&fact_event).unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.doc_count(), 0, "fact events must not be indexed");
    }

    #[test]
    fn schema_has_expected_fields() {
        let schema = build_schema();
        for name in ["event_id", "content", "source", "ts"] {
            assert!(
                schema.get_field(name).is_ok(),
                "schema missing field {name}"
            );
        }
    }

    // ---- T-06: BM25 search ----------------------------------------------

    #[test]
    fn search_returns_results_after_commit() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        let ev = capture("the quick brown fox jumps over the lazy dog");
        idx.index_event(&ev).unwrap();
        idx.commit().unwrap();

        let hits = idx.search("fox", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event_id, ev.id.to_string());
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn search_ranks_more_relevant_higher() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();

        // Three docs with increasing term frequency for "rust".
        let a = capture("I write Python sometimes.");
        let b = capture("Rust is a programming language.");
        let c = capture("Rust rust rust. I love Rust deeply.");
        idx.index_event(&a).unwrap();
        idx.index_event(&b).unwrap();
        idx.index_event(&c).unwrap();
        idx.commit().unwrap();

        let hits = idx.search("rust", 10, None).unwrap();
        // Document a doesn't mention rust; the conjunctive default excludes it.
        assert_eq!(hits.len(), 2, "expected exactly 2 hits, got {hits:?}");
        // Higher-tf doc should rank above lower-tf doc.
        assert_eq!(hits[0].event_id, c.id.to_string());
        assert_eq!(hits[1].event_id, b.id.to_string());
        assert!(hits[0].score >= hits[1].score);
    }

    #[test]
    fn search_returns_empty_for_unmatched_query() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        idx.index_event(&capture("hello world")).unwrap();
        idx.commit().unwrap();
        let hits = idx.search("supercalifragilistic", 10, None).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_with_k_zero_returns_empty() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        idx.index_event(&capture("hello world")).unwrap();
        idx.commit().unwrap();
        let hits = idx.search("hello", 0, None).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn k_caps_result_count() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        for i in 0..20 {
            idx.index_event(&capture(&format!("rust note number {i}")))
                .unwrap();
        }
        idx.commit().unwrap();
        let hits = idx.search("rust", 5, None).unwrap();
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn snippet_is_bounded_and_nonempty() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        let long = "lorem ipsum ".repeat(200) + " needle " + &"dolor sit amet ".repeat(200);
        idx.index_event(&capture(&long)).unwrap();
        idx.commit().unwrap();
        let hits = idx.search("needle", 1, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.is_empty(), "snippet should not be empty");
        // Tantivy generates fragments around matches; allow a small overhead
        // over the configured cap.
        assert!(
            hits[0].snippet.chars().count() <= SNIPPET_MAX_CHARS + 32,
            "snippet too long: {} chars",
            hits[0].snippet.chars().count()
        );
    }

    #[test]
    fn ranks_100_synthetic_captures() {
        // T-06 acceptance: 100 synthetic captures, verify ranking.
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();

        let mut target = None;
        for i in 0..100 {
            if i == 42 {
                let ev = capture(
                    "stripe webhook signature verification fails when the body is re-encoded",
                );
                target = Some(ev.id.to_string());
                idx.index_event(&ev).unwrap();
                continue;
            }
            let text = if i % 7 == 0 {
                format!("note {i}: stripe charges sometimes fail intermittently")
            } else {
                format!("note {i}: unrelated content about cats and dogs")
            };
            idx.index_event(&capture(&text)).unwrap();
        }
        idx.commit().unwrap();

        let target = target.expect("target doc was inserted");
        let hits = idx.search("stripe webhook signature", 10, None).unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        // The single doc that contains all three terms must rank first.
        assert_eq!(
            hits[0].event_id, target,
            "expected target doc to be the top hit"
        );
    }

    // ---- reader-only mode (CLI search while serve is running) ---------

    #[test]
    fn open_reader_only_succeeds_on_empty_dir() {
        let tmp = tempdir().unwrap();
        let idx = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        // No documents committed yet, so search returns empty.
        let hits = idx.search("anything", 5, None).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn reader_only_index_event_errors_clearly() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let err = idx.index_event(&capture("hello")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reader-only"),
            "expected reader-only error, got: {msg}"
        );
    }

    #[test]
    fn reader_only_commit_errors_clearly() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let err = idx.commit().unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reader-only"),
            "expected reader-only error, got: {msg}"
        );
    }

    /// The load-bearing concurrency property: a writer and a reader can
    /// coexist in the same process. Mirrors the cross-process scenario
    /// (`localmem serve` + CLI search) which is what CLAUDE.md's
    /// "CLI and server must be peers" rule demands.
    #[test]
    fn writer_and_reader_coexist_in_same_process() {
        let tmp = tempdir().unwrap();
        let mut writer = LexicalIndex::open(tmp.path()).unwrap();
        let ev = capture("the quick brown fox");
        writer.index_event(&ev).unwrap();
        writer.commit().unwrap();
        // While the writer is still alive, open a reader-only handle on
        // the same directory. Acquiring the writer in open() takes the
        // tantivy directory lock; if open_reader_only also took it,
        // this open call would block or fail with LockBusy.
        let reader = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let hits = reader.search("fox", 5, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event_id, ev.id.to_string());
        // And the writer is still usable after the reader was opened.
        writer.index_event(&capture("lazy dog")).unwrap();
        writer.commit().unwrap();
        let hits2 = reader.search("lazy", 5, None).unwrap();
        // The reader is snapshot-based but reloads on commit signals.
        // We don't guarantee strict freshness here without a manual
        // reload, but a search should at least not error.
        let _ = hits2;
    }

    #[test]
    fn two_reader_only_handles_coexist() {
        let tmp = tempdir().unwrap();
        // Seed one document via a write handle, then drop the writer
        // so the lock is free.
        {
            let mut w = LexicalIndex::open(tmp.path()).unwrap();
            w.index_event(&capture("snowy owl")).unwrap();
            w.commit().unwrap();
        }
        // Two readers at the same time, no writer involved.
        let r1 = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let r2 = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let h1 = r1.search("snowy", 5, None).unwrap();
        let h2 = r2.search("snowy", 5, None).unwrap();
        assert_eq!(h1.len(), 1);
        assert_eq!(h2.len(), 1);
        assert_eq!(h1[0].event_id, h2[0].event_id);
    }

    // ---- T-51: container-tag indexing + filter ------------------------

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

    #[test]
    fn search_with_tag_filter_returns_only_matching_captures() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        let a = capture_with_tags(
            "rust async runtimes notes",
            &[("project", "localmem"), ("topic", "async")],
        );
        let b = capture_with_tags("rust async runtimes notes", &[("project", "other")]);
        let c = capture("rust async runtimes notes"); // no tags
        idx.index_event(&a).unwrap();
        idx.index_event(&b).unwrap();
        idx.index_event(&c).unwrap();
        idx.commit().unwrap();

        let mut filter = BTreeMap::new();
        filter.insert("project".into(), "localmem".into());
        let hits = idx.search("rust async", 10, Some(&filter)).unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.event_id.as_str()).collect();
        assert_eq!(ids, vec![a.id.to_string().as_str()]);
    }

    #[test]
    fn search_without_filter_returns_all_matches_including_untagged() {
        // No-filter (None) must behave identically to v0.1.
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        let a = capture_with_tags("hello world", &[("project", "x")]);
        let b = capture("hello world"); // no tags
        idx.index_event(&a).unwrap();
        idx.index_event(&b).unwrap();
        idx.commit().unwrap();
        let hits = idx.search("hello", 10, None).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_with_multikey_filter_requires_subset_match() {
        // The semantics are AND across all (k, v) pairs in the filter.
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        let both = capture_with_tags("foo", &[("project", "lm"), ("topic", "tags")]);
        let one = capture_with_tags("foo", &[("project", "lm")]);
        idx.index_event(&both).unwrap();
        idx.index_event(&one).unwrap();
        idx.commit().unwrap();
        let mut filter = BTreeMap::new();
        filter.insert("project".into(), "lm".into());
        filter.insert("topic".into(), "tags".into());
        let hits = idx.search("foo", 10, Some(&filter)).unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.event_id.as_str()).collect();
        assert_eq!(ids, vec![both.id.to_string().as_str()]);
    }

    #[test]
    fn search_with_empty_filter_map_is_equivalent_to_no_filter() {
        // An empty BTreeMap must not exclude untagged captures.
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        let untagged = capture("just text");
        idx.index_event(&untagged).unwrap();
        idx.commit().unwrap();
        let empty: BTreeMap<String, String> = BTreeMap::new();
        let hits = idx.search("text", 10, Some(&empty)).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn meta_for_returns_stored_tags_and_ts() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        let ev = capture_with_tags("anchor", &[("project", "localmem"), ("client", "internal")]);
        let expected_secs = ev.ts.timestamp();
        idx.index_event(&ev).unwrap();
        idx.commit().unwrap();
        let meta = idx.meta_for(&ev.id.to_string()).unwrap().unwrap();
        assert_eq!(
            meta.tags.get("project").map(String::as_str),
            Some("localmem")
        );
        assert_eq!(
            meta.tags.get("client").map(String::as_str),
            Some("internal")
        );
        // Indexer truncates to second precision (see `index_event`),
        // so we compare at that granularity.
        assert_eq!(meta.ts.timestamp(), expected_secs);
    }

    #[test]
    fn meta_for_returns_empty_for_unknown_event_or_untagged_capture() {
        let tmp = tempdir().unwrap();
        let mut idx = LexicalIndex::open(tmp.path()).unwrap();
        let bare = capture("untagged content");
        idx.index_event(&bare).unwrap();
        idx.commit().unwrap();
        // Unknown event: meta_for now returns None (the retriever fails closed
        // under an active scope/filter rather than leaking it as global).
        let unknown = idx.meta_for("01HXY00000000000000000000Z").unwrap();
        assert!(unknown.is_none(), "unknown event id has no meta");
        // Known but untagged: Some(meta) with empty tags, real ts.
        let bare_meta = idx
            .meta_for(&bare.id.to_string())
            .unwrap()
            .expect("known capture has meta");
        assert!(bare_meta.tags.is_empty());
        assert_eq!(bare_meta.ts.timestamp(), bare.ts.timestamp());
    }

    // ---- schema-drift self-heal (field-feedback P1, 2026-06-04) -------
    //
    // Repro pattern: an old binary stamps an index, a newer binary
    // (or a different schema version) opens it, must produce
    // [`LexicalError::SchemaDrift`] with an actionable Display message
    // rather than tantivy's opaque "Schema error" deadend.

    #[test]
    fn fresh_open_writes_schema_version_sidecar() {
        let tmp = tempdir().unwrap();
        // Sidecar must NOT exist before the first open.
        let sidecar = tmp.path().join(SCHEMA_VERSION_FILE);
        assert!(!sidecar.exists(), "sidecar should not exist pre-open");
        let _idx = LexicalIndex::open(tmp.path()).unwrap();
        assert!(sidecar.exists(), "open() must stamp the version sidecar");
        let raw = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(raw.trim(), LEXICAL_SCHEMA_VERSION.to_string());
    }

    #[test]
    fn reader_only_first_open_also_stamps_version() {
        let tmp = tempdir().unwrap();
        let _idx = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let sidecar = tmp.path().join(SCHEMA_VERSION_FILE);
        assert!(
            sidecar.exists(),
            "reader-only open on a fresh dir must also stamp the version sidecar"
        );
    }

    #[test]
    fn second_open_preserves_existing_sidecar() {
        let tmp = tempdir().unwrap();
        {
            let _ = LexicalIndex::open(tmp.path()).unwrap();
        }
        let sidecar = tmp.path().join(SCHEMA_VERSION_FILE);
        let before = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
        // Second open with the same version must NOT rewrite the sidecar
        // (we only stamp on fresh dirs to keep mtime meaningful for ops).
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = LexicalIndex::open(tmp.path()).unwrap();
        let after = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "sidecar mtime must not change on a same-version reopen"
        );
    }

    #[test]
    fn schema_drift_returns_structured_error_when_sidecar_missing() {
        // Simulate a pre-versioning install: the lex index exists but
        // no sidecar was ever written. This is the exact field-repro
        // shape an existing user hits on first upgrade.
        let tmp = tempdir().unwrap();
        {
            let _ = LexicalIndex::open(tmp.path()).unwrap();
        }
        let sidecar = tmp.path().join(SCHEMA_VERSION_FILE);
        std::fs::remove_file(&sidecar).expect("remove sidecar to simulate pre-versioning install");

        let err = LexicalIndex::open_reader_only(tmp.path())
            .err()
            .expect("open must fail on schema drift");
        let lex_err = err
            .downcast_ref::<LexicalError>()
            .expect("schema drift must surface as LexicalError");
        match lex_err {
            LexicalError::SchemaDrift { found, current } => {
                assert_eq!(*found, None, "pre-versioning installs report found=None");
                assert_eq!(*current, LEXICAL_SCHEMA_VERSION);
            }
        }
        // Display message must carry the actionable hint verbatim so
        // the CLI surfaces it without any extra plumbing.
        let msg = format!("{lex_err}");
        assert!(
            msg.contains("Run: localmem replay"),
            "drift error must tell the user to replay; got: {msg}"
        );
    }

    #[test]
    fn schema_drift_returns_structured_error_when_sidecar_version_mismatches() {
        // Simulate an upgrade across schema versions: index was
        // stamped at vN by an older binary, current binary is at vM.
        let tmp = tempdir().unwrap();
        {
            let _ = LexicalIndex::open(tmp.path()).unwrap();
        }
        let sidecar = tmp.path().join(SCHEMA_VERSION_FILE);
        // Write a stale version to the sidecar.
        std::fs::write(&sidecar, "0\n").unwrap();

        let err = LexicalIndex::open_reader_only(tmp.path())
            .err()
            .expect("open must fail on schema drift");
        let lex_err = err
            .downcast_ref::<LexicalError>()
            .expect("schema drift must surface as LexicalError");
        match lex_err {
            LexicalError::SchemaDrift { found, current } => {
                assert_eq!(*found, Some(0));
                assert_eq!(*current, LEXICAL_SCHEMA_VERSION);
            }
        }
        let msg = format!("{lex_err}");
        assert!(msg.contains("Run: localmem replay"));
        assert!(
            msg.contains("v0"),
            "drift message must include the found version: {msg}"
        );
    }

    #[test]
    fn lex_context_preserves_schema_drift_top_line() {
        // The CLI wraps `LexicalIndex::open*` with `.lex_context(...)`.
        // For SchemaDrift the top-line message must remain the
        // actionable drift hint, NOT the context wrapper.
        let tmp = tempdir().unwrap();
        {
            let _ = LexicalIndex::open(tmp.path()).unwrap();
        }
        let sidecar = tmp.path().join(SCHEMA_VERSION_FILE);
        std::fs::write(&sidecar, "999\n").unwrap();

        let err = LexicalIndex::open_reader_only(tmp.path())
            .lex_context("open lexical index for reading")
            .err()
            .expect("open must fail on schema drift");
        // Top-line message is the SchemaDrift display, not the context.
        let top = format!("{err}");
        assert!(
            top.contains("Run: localmem replay"),
            "lex_context must not shadow drift hint; top-line was: {top}"
        );
        assert!(
            !top.contains("open lexical index for reading"),
            "lex_context must not prepend its context for drift errors; top-line was: {top}"
        );
        // Sanity: downcast still succeeds through lex_context.
        assert!(
            err.downcast_ref::<LexicalError>().is_some(),
            "drift error must remain downcast-able after lex_context"
        );
    }

    #[test]
    fn lex_context_attaches_context_for_unrelated_errors() {
        // For non-drift failures, lex_context must behave exactly like
        // .context(...) — i.e. attach the supplied message.
        let res: Result<()> = Err(anyhow::anyhow!("boom"));
        let err = res.lex_context("doing the thing").unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("doing the thing"));
        assert!(chain.contains("boom"));
        // And the inner error is NOT a LexicalError.
        assert!(err.downcast_ref::<LexicalError>().is_none());
    }

    #[test]
    fn drift_resolution_via_swap_and_reopen() {
        // The whole point of the drift error: after the user (or
        // `localmem replay`) clears the derived dir, a fresh open
        // succeeds and re-stamps the sidecar. This test exercises
        // exactly that recovery loop without depending on replay.
        let tmp = tempdir().unwrap();
        {
            let _ = LexicalIndex::open(tmp.path()).unwrap();
        }
        let sidecar = tmp.path().join(SCHEMA_VERSION_FILE);
        std::fs::write(&sidecar, "0\n").unwrap();
        assert!(LexicalIndex::open(tmp.path()).is_err());

        // Recovery: blow away the derived dir + sidecar, just like
        // `replay::swap_derived` does.
        std::fs::remove_dir_all(tmp.path().join("derived")).unwrap();

        let idx = LexicalIndex::open(tmp.path()).expect("re-open after wipe must succeed");
        drop(idx);
        let restamped = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(restamped.trim(), LEXICAL_SCHEMA_VERSION.to_string());
    }
}
