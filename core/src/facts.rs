//! Bitemporal facts in DuckDB.
//!
//! See ARCHITECTURE.md (Derived stores -> `facts.duckdb`). Schema lives in
//! `core/migrations/*.sql`. Implementation tasks: T-11 (schema + open),
//! T-12 (bitemporal queries).
//!
//! Why DuckDB and not Postgres or SQLite: single-file, embedded, column-store,
//! ships native TIMESTAMPTZ + array types. Recursive CTEs let us answer
//! entity-graph queries without bolting on a graph DB. The `bundled` feature
//! compiles DuckDB into the binary so users (and CI) need no system library.

use crate::event::FactPayload;
use crate::event_id::EventId;
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use duckdb::{params, Connection};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Directory (relative to localmem home) where derived stores live.
pub const FACTS_DIR: &str = "derived";
/// Filename of the DuckDB file inside [`FACTS_DIR`].
pub const FACTS_FILE: &str = "facts.duckdb";

/// T-56 smart-forgetting confidence gate. New facts below this
/// threshold append without retiring prior beliefs — the journal
/// flags the contradiction for the user to resolve manually. The
/// threshold matches the rule extractor's `prefer` confidence
/// (0.7); anything weaker is treated as "noisy signal, don't
/// invalidate yet."
pub const CONFIDENCE_THRESHOLD: f64 = 0.7;

/// Migration set, applied in id order. Adding a new schema change means
/// appending a `(id, include_str!("..."))` entry, never editing an existing
/// one. The runner records applied ids in `schema_migrations`.
const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_facts_tags.sql")),
    (3, include_str!("../migrations/0003_facts_kind.sql")),
];

/// T-60: one row returned by [`FactsStore::entity_graph_walk`]. Surfaces
/// the source CAPTURE id (not the fact id) so consumers can merge with
/// lex/vec hits in the same id-space. Score is a depth-discounted
/// confidence in `[0, 1]`.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityGraphRow {
    pub capture_id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub depth: u32,
    pub confidence: f64,
    pub score: f32,
}

/// In-memory row mirroring the `facts` table. Mirrors but does not reuse
/// [`FactPayload`] because the table carries fields the event payload does
/// not (`recorded_at`, `retired_at`, `policy_id`) and because the id here is
/// the event id of the `fact` event in events.jsonl.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub id: EventId,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f64,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    pub recorded_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
    pub source_events: Vec<EventId>,
    pub policy_id: Option<String>,
    /// Container tags inherited from the source capture (T-51b).
    /// Populated by [`Fact::from_event`] from `FactPayload.tags`, which
    /// the capture write pipeline copied from the source capture's
    /// tags. Stored on the row so `recall`/`profile` can filter by tag
    /// without joining back to the event log.
    pub tags: BTreeMap<String, String>,
    /// Closed-core kind taxonomy (T-52). Inherited from the source
    /// capture at extraction time so `profile` can group by kind
    /// and T-56 smart forgetting can apply the
    /// decision-never-retires rule without a join back to events.
    pub kind: crate::kind::Kind,
}

impl Fact {
    /// Build a `Fact` row from a `fact` event's id+payload plus the time we
    /// recorded it. `policy_id` defaults to None until Group D wires the
    /// policy engine through (T-15+).
    pub fn from_event(
        id: EventId,
        payload: &FactPayload,
        recorded_at: DateTime<Utc>,
        policy_id: Option<String>,
    ) -> Self {
        Self {
            id,
            subject: payload.subject.clone(),
            predicate: payload.predicate.clone(),
            object: payload.object.clone(),
            confidence: payload.confidence,
            valid_from: payload.valid_from,
            valid_to: payload.valid_to,
            recorded_at,
            retired_at: None,
            source_events: payload.derived_from.clone(),
            policy_id,
            tags: payload.tags.clone(),
            kind: payload.kind.clone(),
        }
    }
}

/// Bitemporal fact store backed by a single embedded DuckDB file.
///
/// One instance per process. The DuckDB `Connection` has interior mutability,
/// so writers take `&self` and the caller does not need to wrap us in a
/// `Mutex` for single-threaded use.
pub struct FactsStore {
    conn: Connection,
    path: PathBuf,
}

impl FactsStore {
    /// Open (or create) the facts database at `<home>/derived/facts.duckdb`.
    /// Applies any pending migrations from [`MIGRATIONS`].
    pub fn open(home: impl AsRef<Path>) -> Result<Self> {
        let home = home.as_ref();
        let dir = home.join(FACTS_DIR);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create derived dir at {}", dir.display()))?;
        let path = dir.join(FACTS_FILE);
        let conn = Connection::open(&path)
            .with_context(|| format!("open duckdb at {}", path.display()))?;
        let store = Self { conn, path };
        store.apply_migrations()?;
        Ok(store)
    }

    /// Filesystem location of the DuckDB file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Apply any migrations not yet recorded in `schema_migrations`.
    ///
    /// The tracking table is bootstrapped outside [`MIGRATIONS`] so it always
    /// exists before we query it. Each migration is recorded only after its
    /// SQL succeeds, so a partial application leaves the row absent and the
    /// next open retries.
    fn apply_migrations(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                    id          INTEGER PRIMARY KEY,
                    applied_at  TIMESTAMPTZ NOT NULL
                );",
            )
            .context("create schema_migrations")?;

        let already: Vec<i32> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM schema_migrations")
                .context("prepare schema_migrations select")?;
            stmt.query_map([], |row| row.get::<_, i32>(0))
                .context("query schema_migrations")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("collect applied migrations")?
        };

        for (id, sql) in MIGRATIONS {
            if already.contains(id) {
                continue;
            }
            self.conn
                .execute_batch(sql)
                .with_context(|| format!("apply migration {id}"))?;
            self.conn
                .execute(
                    "INSERT INTO schema_migrations (id, applied_at) VALUES (?, now())",
                    params![*id],
                )
                .with_context(|| format!("record migration {id}"))?;
        }
        Ok(())
    }

    /// Insert a fact. Caller is responsible for first appending the
    /// corresponding `fact` event to `events.jsonl` (so replay can rebuild).
    pub fn insert(&self, fact: &Fact) -> Result<()> {
        // ULIDs are validated 26-char Crockford base32 (see EventId) so
        // formatting them into a `[ '...', ... ]` list literal is safe from
        // SQL injection. We do this because duckdb-rs does not currently
        // accept a Rust `Vec<String>` as a TEXT[] parameter.
        let source_events = source_events_literal(&fact.source_events);
        // T-51b: tags persisted as a JSON-encoded BTreeMap so the
        // column round-trips losslessly through replay. BTreeMap's
        // ordering makes the encoding deterministic, which keeps
        // diff-based golden tests stable across runs.
        let tags_json = serde_json::to_string(&fact.tags).context("serialize fact tags to JSON")?;
        // T-52: persist the canonical kind string. Note → "note"
        // appears on disk verbatim (we don't skip-when-default
        // because the read path needs to distinguish "row predates
        // T-52" (NULL) from "row was explicitly note" — though both
        // collapse to Kind::Note in practice).
        let kind_str = fact.kind.as_str().to_string();
        let sql = format!(
            "INSERT INTO facts (
                id, subject, predicate, object, confidence,
                valid_from, valid_to, recorded_at, retired_at,
                source_events, policy_id, tags, kind
            ) VALUES (?, ?, ?, ?, ?,
                       CAST(? AS TIMESTAMPTZ), CAST(? AS TIMESTAMPTZ),
                       CAST(? AS TIMESTAMPTZ), CAST(? AS TIMESTAMPTZ),
                       {source_events}, ?, ?, ?)"
        );
        self.conn
            .execute(
                &sql,
                params![
                    fact.id.to_string(),
                    fact.subject,
                    fact.predicate,
                    fact.object,
                    fact.confidence,
                    fmt_ts(&fact.valid_from),
                    fact.valid_to.as_ref().map(fmt_ts),
                    fmt_ts(&fact.recorded_at),
                    fact.retired_at.as_ref().map(fmt_ts),
                    fact.policy_id,
                    tags_json,
                    kind_str,
                ],
            )
            .context("insert fact")?;
        Ok(())
    }

    /// Total row count. Useful as a smoke test and for replay diagnostics.
    pub fn count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM facts", [], |row| row.get(0))
            .context("count facts")?;
        Ok(n as u64)
    }

    /// Has migration `id` been applied? Used by tests to verify idempotency.
    pub fn migration_applied(&self, id: i32) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE id = ?",
                params![id],
                |row| row.get(0),
            )
            .context("query schema_migrations")?;
        Ok(n > 0)
    }

    /// Column names of the `facts` table, in declaration order. Used by
    /// tests to assert the schema matches ARCHITECTURE.md.
    pub fn fact_columns(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("PRAGMA table_info('facts')")
            .context("prepare PRAGMA table_info")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .context("query PRAGMA table_info")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("collect column names")
    }

    // ---- T-12 bitemporal queries ----------------------------------------

    /// Facts about `subject` that are valid at `at_time` and not yet retired.
    ///
    /// Bitemporal predicate, evaluated as a half-open interval:
    /// `valid_from <= at_time AND (valid_to IS NULL OR valid_to > at_time)`.
    /// Rows whose `retired_at <= at_time` are additionally hidden, so a
    /// `forget` event landing at T removes the fact from any "as of >= T"
    /// query while leaving the row intact for audit.
    pub fn facts_at_time(&self, subject: &str, at_time: DateTime<Utc>) -> Result<Vec<Fact>> {
        let at = fmt_ts(&at_time);
        let sql = format!(
            "SELECT {SELECT_FACT_COLS}
               FROM facts
              WHERE subject = ?
                AND valid_from <= CAST(? AS TIMESTAMPTZ)
                AND (valid_to IS NULL OR valid_to > CAST(? AS TIMESTAMPTZ))
                AND (retired_at IS NULL OR retired_at > CAST(? AS TIMESTAMPTZ))
              ORDER BY valid_from ASC"
        );
        let mut stmt = self.conn.prepare(&sql).context("prepare facts_at_time")?;
        let rows = stmt
            .query_map(params![subject, at, at, at], fact_from_row)
            .context("execute facts_at_time")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("collect facts_at_time rows")
    }

    /// All facts about `subject`, ordered by `valid_from`. Includes retired
    /// rows so this is the right method for an audit-style "recall"; callers
    /// that want only currently-true facts should use
    /// [`Self::facts_at_time`] with `Utc::now()`.
    pub fn facts_for_subject(&self, subject: &str) -> Result<Vec<Fact>> {
        let sql = format!(
            "SELECT {SELECT_FACT_COLS}
               FROM facts
              WHERE subject = ?
              ORDER BY valid_from ASC"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .context("prepare facts_for_subject")?;
        let rows = stmt
            .query_map(params![subject], fact_from_row)
            .context("execute facts_for_subject")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("collect facts_for_subject rows")
    }

    /// All facts believed true at `at_time`, optionally filtered to a
    /// single `subject`. "Believed true" means `valid_from <= at_time`,
    /// the validity window has not closed by `at_time`, and the row is
    /// not retired by `at_time`. Used by `localmem profile` (T-39).
    pub fn all_live_facts(
        &self,
        at_time: DateTime<Utc>,
        subject: Option<&str>,
    ) -> Result<Vec<Fact>> {
        let at = fmt_ts(&at_time);
        let mut sql = format!(
            "SELECT {SELECT_FACT_COLS}
               FROM facts
              WHERE valid_from <= CAST(? AS TIMESTAMPTZ)
                AND (valid_to IS NULL OR valid_to > CAST(? AS TIMESTAMPTZ))
                AND (retired_at IS NULL OR retired_at > CAST(? AS TIMESTAMPTZ))"
        );
        if subject.is_some() {
            sql.push_str(" AND subject = ?");
        }
        sql.push_str(" ORDER BY subject ASC, valid_from ASC");

        let mut stmt = self.conn.prepare(&sql).context("prepare all_live_facts")?;
        let rows = if let Some(s) = subject {
            stmt.query_map(params![at, at, at, s], fact_from_row)
                .context("execute all_live_facts (scoped)")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("collect all_live_facts rows")?
        } else {
            stmt.query_map(params![at, at, at], fact_from_row)
                .context("execute all_live_facts")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("collect all_live_facts rows")?
        };
        Ok(rows)
    }

    /// Tag-filtered companion of [`Self::facts_for_subject`] (T-51b
    /// and T-51c).
    ///
    /// Three filter layers compose, all post-SELECT:
    /// 1. `tag_filter` (T-51b): subset match against `Fact.tags`.
    ///    `None` or empty disables it.
    /// 2. Reserved-tag visibility (T-51c): `Visibility::Default`
    ///    excludes facts whose source capture carried
    ///    `visibility=private`; `Visibility::IncludePrivate` is the
    ///    audit-grade override for entity-only recall.
    /// 3. Reserved-tag retention (T-51c): facts whose source capture
    ///    carried `retention=ephemeral:<TTL>` drop once
    ///    `now - capture_ts > TTL`. `now` is the caller's reference
    ///    instant so a single query is consistent across lex + facts
    ///    paths even when they fire microseconds apart.
    ///
    /// Filters are applied post-SELECT in Rust rather than pushed
    /// into SQL: DuckDB's JSON path operators are version-sensitive
    /// and the per-subject result sets are small.
    pub fn facts_for_subject_filtered(
        &self,
        subject: &str,
        tag_filter: Option<&BTreeMap<String, String>>,
        visibility: crate::reserved_tags::Visibility,
        now: DateTime<Utc>,
    ) -> Result<Vec<Fact>> {
        let rows = self.facts_for_subject(subject)?;
        Ok(apply_filters(rows, tag_filter, visibility, now))
    }

    /// Tag-filtered companion of [`Self::all_live_facts`] (T-51b +
    /// T-51c). Same semantics as
    /// [`Self::facts_for_subject_filtered`] for the filter arguments.
    pub fn all_live_facts_filtered(
        &self,
        at_time: DateTime<Utc>,
        subject: Option<&str>,
        tag_filter: Option<&BTreeMap<String, String>>,
        visibility: crate::reserved_tags::Visibility,
        now: DateTime<Utc>,
    ) -> Result<Vec<Fact>> {
        let rows = self.all_live_facts(at_time, subject)?;
        Ok(apply_filters(rows, tag_filter, visibility, now))
    }

    /// Smart forgetting (T-56). Find every still-live fact with the
    /// same `(subject, predicate)` as `new_fact` and retire it by
    /// setting `retired_at = new_fact.valid_from`. Returns the ids
    /// of the retired rows so the caller can emit `Update` events
    /// + journal entries that record the contradiction.
    ///
    /// Two gates per SPEC_V0_2 "Smart Forgetting":
    /// 1. `new_fact.confidence < CONFIDENCE_THRESHOLD` (0.7) → no
    ///    retirement. Low-confidence facts append without
    ///    invalidating prior beliefs; the journal flags the
    ///    contradiction for the user to resolve.
    /// 2. `new_fact.kind` opts out of contradiction resolution
    ///    (currently only `Kind::Decision`) → no retirement, even
    ///    if confidence is high. Decisions are append-only audit
    ///    entries.
    ///
    /// We also exclude prior facts whose own kind is `decision`
    /// from the retirement set: a decision in the audit trail
    /// stays put even when a non-decision new fact would otherwise
    /// retire it. The asymmetry mirrors the spec's
    /// "Decision kind is append-only".
    ///
    /// The caller is responsible for emitting the `Update` event in
    /// `events.jsonl` — this method only touches DuckDB. That
    /// separation matters because the new fact's id must equal the
    /// `Update` event's id (so `replay` materialises the new fact
    /// under the right primary key); the caller has to allocate
    /// the id before invoking `resolve_contradiction` so it can
    /// thread it into both the event and the fact row.
    pub fn resolve_contradiction(&self, new_fact: &Fact) -> Result<Vec<EventId>> {
        if !new_fact.kind.allows_contradiction_resolution() {
            return Ok(Vec::new());
        }
        if new_fact.confidence < CONFIDENCE_THRESHOLD {
            return Ok(Vec::new());
        }

        // Find live prior facts. `kind IS NULL` covers v0.1.x rows
        // that pre-date T-52; we treat NULL as `Kind::Note`, which
        // allows retirement. The new fact's own id is not in the
        // table yet (the caller hasn't inserted it), so we don't
        // need an explicit self-exclusion clause.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id FROM facts \
                 WHERE subject = ? AND predicate = ? \
                 AND retired_at IS NULL \
                 AND (kind IS NULL OR kind != 'decision')",
            )
            .context("prepare resolve_contradiction select")?;
        let candidates: Vec<EventId> = stmt
            .query_map(params![new_fact.subject, new_fact.predicate], |row| {
                let s: String = row.get(0)?;
                Ok(s)
            })
            .context("execute resolve_contradiction select")?
            .collect::<std::result::Result<Vec<String>, _>>()
            .context("collect contradiction candidates")?
            .into_iter()
            .filter_map(|s| s.parse::<EventId>().ok())
            .filter(|id| id != &new_fact.id)
            .collect();

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Retire all matches in a single transaction so a crash
        // mid-loop doesn't leave the table half-updated.
        let ts = fmt_ts(&new_fact.valid_from);
        for id in &candidates {
            self.conn
                .execute(
                    "UPDATE facts SET retired_at = CAST(? AS TIMESTAMPTZ) WHERE id = ?",
                    params![ts, id.to_string()],
                )
                .with_context(|| format!("retire contradicted fact {id}"))?;
        }
        Ok(candidates)
    }

    /// Mark every still-live fact tied to `target_id` as retired at
    /// `retired_at`. "Tied to" means either the fact's own PK equals
    /// `target_id` (forget-by-fact-id) or the id appears in `source_events`
    /// (forget-by-capture-id). Already-retired rows are left untouched so
    /// replay is idempotent under repeated forget events.
    ///
    /// Used by `localmem replay` (T-25) when walking `Forget` and `Update`
    /// events. The replay path is the only consumer in v0.1; emitting a
    /// `forget` event at write time also flows through replay on the next
    /// rebuild, so this is the single source of truth for the operation.
    pub fn retire_facts_for_target(
        &self,
        target_id: &str,
        retired_at: DateTime<Utc>,
    ) -> Result<u64> {
        let ts = fmt_ts(&retired_at);
        let sql = "UPDATE facts
                      SET retired_at = CAST(? AS TIMESTAMPTZ)
                    WHERE retired_at IS NULL
                      AND (id = ? OR list_contains(source_events, ?))";
        let n = self
            .conn
            .execute(sql, params![ts, target_id, target_id])
            .context("retire facts for target")?;
        Ok(n as u64)
    }

    /// Distinct fact subjects with their row counts (T-53 discovery).
    ///
    /// Counts all rows in the facts table, including retired ones, so
    /// `subjects` answers "what entities have we ever extracted facts
    /// about?" rather than "what entities are currently live?" The
    /// audit-grade framing matches the spec wording ("known entities
    /// with fact counts") and keeps the command useful even after
    /// smart forgetting (T-56) retires many rows. Callers that want
    /// live-only counts can post-filter via [`Self::all_live_facts`].
    ///
    /// Returned sorted by count desc, then subject asc for
    /// deterministic ordering across runs.
    pub fn subjects(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT subject, COUNT(*) AS n FROM facts \
                 GROUP BY subject ORDER BY n DESC, subject ASC",
            )
            .context("prepare subjects select")?;
        let rows = stmt
            .query_map([], |row| {
                let s: String = row.get(0)?;
                let n: i64 = row.get(1)?;
                Ok((s, n as u64))
            })
            .context("execute subjects select")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("collect subjects rows")
    }

    /// Find a single fact by its event id (T-53 audit).
    ///
    /// Returns `None` when no row carries that primary key. Includes
    /// retired rows because audit traces are the primary use; the
    /// retired_at field is on the returned [`Fact`] so the caller can
    /// distinguish live vs retired.
    /// T-60: entity-graph 2-hop walk. Returns rows for facts whose
    /// subject matches one of `seeds` (depth 0) plus facts whose
    /// subject equals any depth-N row's object (depth N+1), up to
    /// `max_depth` inclusive. Excludes retired facts and edges
    /// below `min_confidence`. Subject match uses case-insensitive
    /// equality so seeds from query parsing don't need to preserve
    /// original casing.
    ///
    /// Result rows surface CAPTURE ids (not fact ids) via
    /// `source_events[0]` so consumers can join with lex/vec hits
    /// in the same id-space.
    ///
    /// `seeds` is empty → returns Ok(vec![]) without any query.
    /// `limit = 0` → returns Ok(vec![]) likewise (caller's choice).
    pub fn entity_graph_walk(
        &self,
        seeds: &[String],
        max_depth: u32,
        min_confidence: f64,
        limit: usize,
    ) -> Result<Vec<EntityGraphRow>> {
        if seeds.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // Subjects are user-supplied strings: escape single quotes
        // before inlining into the IN list. DuckDB SQL string
        // literal escaping is `'` → `''`. Same discipline as the
        // existing source_events_literal helper. Inlining (vs
        // parameter binding) avoids the variadic-IN gymnastics
        // that duckdb-rs doesn't support cleanly.
        let in_list = seeds
            .iter()
            .map(|s| format!("LOWER('{}')", s.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "WITH RECURSIVE walk AS (
                SELECT id, subject, predicate, object, confidence, source_events, 0 AS depth
                FROM facts
                WHERE LOWER(subject) IN ({in_list})
                  AND retired_at IS NULL
                  AND confidence >= ?
                UNION ALL
                SELECT f.id, f.subject, f.predicate, f.object, f.confidence, f.source_events, w.depth + 1
                FROM facts f
                JOIN walk w ON LOWER(w.object) = LOWER(f.subject)
                WHERE f.retired_at IS NULL
                  AND f.confidence >= ?
                  AND w.depth < ?
            )
            SELECT
                CAST(list_extract(source_events, 1) AS TEXT) AS capture_id,
                subject,
                predicate,
                object,
                MIN(depth) AS min_depth,
                MAX(confidence) AS max_confidence
            FROM walk
            WHERE source_events IS NOT NULL AND len(source_events) > 0
            GROUP BY capture_id, subject, predicate, object
            ORDER BY min_depth ASC, max_confidence DESC
            LIMIT ?"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .context("prepare entity_graph_walk")?;
        let rows = stmt
            .query_map(
                params![min_confidence, min_confidence, max_depth as i64, limit as i64],
                |row| {
                    let capture_id: String = row.get(0)?;
                    let subject: String = row.get(1)?;
                    let predicate: String = row.get(2)?;
                    let object: String = row.get(3)?;
                    let depth: i64 = row.get(4)?;
                    let confidence: f64 = row.get(5)?;
                    // Score: closer depth wins (1/(1+d)) blended with
                    // confidence (so a depth-1 0.9-confidence edge
                    // beats a depth-1 0.7-confidence edge). Bounded
                    // in [0, 1].
                    let score = (1.0 / (1.0 + depth as f64)) * confidence;
                    Ok(EntityGraphRow {
                        capture_id,
                        subject,
                        predicate,
                        object,
                        depth: depth as u32,
                        confidence,
                        score: score as f32,
                    })
                },
            )
            .context("execute entity_graph_walk")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("collect entity_graph_walk rows")
    }

    pub fn find_by_id(&self, id: &EventId) -> Result<Option<Fact>> {
        let sql = format!(
            "SELECT {SELECT_FACT_COLS} FROM facts WHERE id = ? LIMIT 1"
        );
        let mut stmt = self.conn.prepare(&sql).context("prepare find_by_id")?;
        let mut rows = stmt
            .query_map(params![id.to_string()], fact_from_row)
            .context("execute find_by_id")?;
        match rows.next() {
            Some(row) => Ok(Some(row.context("collect find_by_id row")?)),
            None => Ok(None),
        }
    }

    /// Bitemporal "is this capture's downstream fact still believed at `at_time`?"
    ///
    /// Returns `true` when the capture identified by `event_id`:
    /// 1. never produced any fact (a raw capture the extractor passed on), or
    /// 2. produced at least one fact that is still valid at `at_time` and has
    ///    not been retired by then.
    ///
    /// Returns `false` only when every fact derived from this capture has
    /// either fallen out of its `valid_from..valid_to` window or been retired
    /// at or before `at_time`. Used by the hybrid retriever (T-23) to drop
    /// hits whose downstream beliefs no longer hold at the query's `at_time`.
    ///
    /// The query collapses the three cases into a single round trip rather
    /// than three: cheap on a hot retrieval path and avoids race windows
    /// between the existence check and the validity check.
    pub fn is_event_valid_at(&self, event_id: &str, at_time: DateTime<Utc>) -> Result<bool> {
        let at = fmt_ts(&at_time);
        // The CTE form keeps the three predicates readable and lets DuckDB
        // short-circuit each LIMIT 1 independently. The outer SELECT returns
        // exactly one boolean row.
        let sql = "WITH has_facts AS (
                       SELECT 1 AS x FROM facts
                        WHERE list_contains(source_events, ?)
                        LIMIT 1
                   ),
                   has_valid AS (
                       SELECT 1 AS x FROM facts
                        WHERE list_contains(source_events, ?)
                          AND valid_from <= CAST(? AS TIMESTAMPTZ)
                          AND (valid_to IS NULL OR valid_to > CAST(? AS TIMESTAMPTZ))
                          AND (retired_at IS NULL OR retired_at > CAST(? AS TIMESTAMPTZ))
                        LIMIT 1
                   )
                   SELECT
                       CASE
                           WHEN (SELECT x FROM has_facts) IS NULL THEN TRUE
                           WHEN (SELECT x FROM has_valid) IS NOT NULL THEN TRUE
                           ELSE FALSE
                       END";
        let keep: bool = self
            .conn
            .query_row(sql, params![event_id, event_id, at, at, at], |row| {
                row.get(0)
            })
            .context("evaluate is_event_valid_at")?;
        Ok(keep)
    }
}

/// SELECT projection used by every bitemporal query. Order must match
/// `fact_from_row`'s `row.get(N)` indices.
///
/// Timestamps come back as epoch microseconds (BIGINT) so we sidestep
/// DuckDB's printable TIMESTAMPTZ format (`YYYY-MM-DD HH:MM:SS+00`), which
/// chrono's RFC3339 parser rejects because the timezone has no minutes. The
/// TEXT[] column collapses to a comma-joined string for the same reason: no
/// `Vec<String>` `FromSql` impl in duckdb-rs.
const SELECT_FACT_COLS: &str = "id, subject, predicate, object, confidence, \
     epoch_us(valid_from), \
     epoch_us(valid_to), \
     epoch_us(recorded_at), \
     epoch_us(retired_at), \
     COALESCE(array_to_string(source_events, ','), ''), \
     policy_id, \
     tags, \
     kind";

/// Apply both the user-supplied tag subset filter (T-51b) and the
/// reserved-tag visibility/retention predicate (T-51c) to a list of
/// facts. `tag_filter = None` or empty disables that layer; the
/// reserved-tag check always runs because retention TTL may fire
/// even when the user passed no filter.
fn apply_filters(
    facts: Vec<Fact>,
    tag_filter: Option<&BTreeMap<String, String>>,
    visibility: crate::reserved_tags::Visibility,
    now: DateTime<Utc>,
) -> Vec<Fact> {
    let active_tag_filter = tag_filter.filter(|m| !m.is_empty());
    facts
        .into_iter()
        .filter(|fact| {
            if let Some(f) = active_tag_filter {
                if !crate::tag_match::matches(&fact.tags, f) {
                    return false;
                }
            }
            // `valid_from` on a derived fact mirrors the source
            // capture's ts (see `build_fact_event` in write.rs /
            // routes.rs / indexer.rs), so it's the right input for
            // the retention TTL check.
            crate::reserved_tags::is_visible(&fact.tags, fact.valid_from, now, visibility)
        })
        .collect()
}

fn source_events_literal(ids: &[EventId]) -> String {
    if ids.is_empty() {
        // DuckDB infers an empty literal `[]` as INTEGER[] without a cast.
        return "CAST([] AS TEXT[])".to_string();
    }
    let parts: Vec<String> = ids.iter().map(|id| format!("'{id}'")).collect();
    format!("[{}]", parts.join(","))
}

fn fmt_ts(ts: &DateTime<Utc>) -> String {
    // Microsecond precision matches DuckDB TIMESTAMPTZ storage so round-trip
    // is lossless. SecondsFormat::Micros forces six fractional digits even
    // when the time has fewer (e.g. 1700000000.000000Z).
    ts.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn ts_from_epoch_us(us: i64) -> Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_micros(us)
        .with_context(|| format!("out-of-range timestamp: {us} us"))
}

fn parse_source_events(csv: &str) -> Result<Vec<EventId>> {
    if csv.is_empty() {
        return Ok(Vec::new());
    }
    csv.split(',')
        .map(|s| {
            s.parse::<EventId>()
                .with_context(|| format!("parse source event id: {s}"))
        })
        .collect()
}

fn fact_from_row(row: &duckdb::Row<'_>) -> duckdb::Result<Fact> {
    use duckdb::types::Type;
    let id_str: String = row.get(0)?;
    let subject: String = row.get(1)?;
    let predicate: String = row.get(2)?;
    let object: String = row.get(3)?;
    let confidence: f64 = row.get(4)?;
    let valid_from_us: i64 = row.get(5)?;
    let valid_to_us: Option<i64> = row.get(6)?;
    let recorded_at_us: i64 = row.get(7)?;
    let retired_at_us: Option<i64> = row.get(8)?;
    let source_events_csv: String = row.get(9)?;
    let policy_id: Option<String> = row.get(10)?;
    // Nullable: legacy v0.1.x rows have NULL here because the
    // migration could not set a NOT NULL DEFAULT in DuckDB. Treat
    // NULL the same as the empty map.
    let tags_json: Option<String> = row.get(11)?;
    // T-52: kind nullable for the same reason. NULL / empty / "note"
    // all collapse to Kind::Note via the From<String> impl + an
    // explicit None branch below.
    let kind_str: Option<String> = row.get(12)?;

    let cast_err =
        |idx, e: anyhow::Error| duckdb::Error::FromSqlConversionFailure(idx, Type::Text, e.into());

    let id = id_str
        .parse::<EventId>()
        .map_err(|e| cast_err(0, anyhow::Error::new(e)))?;
    let valid_from = ts_from_epoch_us(valid_from_us).map_err(|e| cast_err(5, e))?;
    let valid_to = valid_to_us
        .map(ts_from_epoch_us)
        .transpose()
        .map_err(|e| cast_err(6, e))?;
    let recorded_at = ts_from_epoch_us(recorded_at_us).map_err(|e| cast_err(7, e))?;
    let retired_at = retired_at_us
        .map(ts_from_epoch_us)
        .transpose()
        .map_err(|e| cast_err(8, e))?;
    let source_events = parse_source_events(&source_events_csv).map_err(|e| cast_err(9, e))?;
    let tags: BTreeMap<String, String> = match tags_json.as_deref() {
        None | Some("") | Some("{}") => BTreeMap::new(),
        Some(s) => serde_json::from_str(s).map_err(|e| {
            cast_err(
                11,
                anyhow::anyhow!(e).context("parse facts.tags JSON column"),
            )
        })?,
    };

    let kind = match kind_str.as_deref() {
        None | Some("") => crate::kind::Kind::default(),
        Some(s) => crate::kind::Kind::from(s.to_string()),
    };

    Ok(Fact {
        id,
        subject,
        predicate,
        object,
        confidence,
        valid_from,
        valid_to,
        recorded_at,
        retired_at,
        source_events,
        policy_id,
        tags,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::FactPayload;
    use serde_json::Map;
    use tempfile::tempdir;

    fn ts(epoch_secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(epoch_secs, 0).expect("valid ts")
    }

    fn sample_fact(subject: &str, object: &str, valid_from: DateTime<Utc>) -> Fact {
        Fact {
            id: EventId::new(),
            subject: subject.into(),
            predicate: "is".into(),
            object: object.into(),
            confidence: 0.7,
            valid_from,
            valid_to: None,
            recorded_at: valid_from,
            retired_at: None,
            source_events: vec![EventId::new()],
            policy_id: Some("rule:high_signal".into()),
            kind: Default::default(),
            tags: Default::default(),
        }
    }

    #[test]
    fn open_creates_file_and_applies_migration() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        assert!(store.path().exists(), "duckdb file should be created");
        assert!(store.migration_applied(1).unwrap());
    }

    #[test]
    fn schema_matches_architecture_md() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let cols = store.fact_columns().unwrap();
        let expected = [
            "id",
            "subject",
            "predicate",
            "object",
            "confidence",
            "valid_from",
            "valid_to",
            "recorded_at",
            "retired_at",
            "source_events",
            "policy_id",
            // T-51b: container tags inherited from the source capture.
            "tags",
            // T-52: kind taxonomy inherited from the source capture.
            "kind",
        ];
        assert_eq!(cols, expected);
    }

    #[test]
    fn migrations_are_idempotent() {
        let tmp = tempdir().unwrap();
        // First open applies.
        drop(FactsStore::open(tmp.path()).unwrap());
        // Reopen must not re-apply: schema_migrations stays at one row for id 1.
        let store = FactsStore::open(tmp.path()).unwrap();
        let n: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "migration row should not duplicate on reopen");
    }

    #[test]
    fn insert_and_count() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        assert_eq!(store.count().unwrap(), 0);
        store
            .insert(&sample_fact("user", "rust", ts(1_700_000_000)))
            .unwrap();
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn insert_persists_across_reopen() {
        let tmp = tempdir().unwrap();
        {
            let store = FactsStore::open(tmp.path()).unwrap();
            store
                .insert(&sample_fact("user", "rust", ts(1_700_000_000)))
                .unwrap();
        }
        let store = FactsStore::open(tmp.path()).unwrap();
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn insert_with_empty_source_events_works() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let mut f = sample_fact("user", "rust", ts(1_700_000_000));
        f.source_events.clear();
        store.insert(&f).unwrap();
        assert_eq!(store.count().unwrap(), 1);
    }

    // ---- T-12 bitemporal queries --------------------------------------

    /// Two contradicting facts about the same subject with overlapping
    /// validity. `facts_at_time` returns the one valid at the query time.
    #[test]
    fn facts_at_time_picks_correct_fact_across_supersession() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();

        // A: user lives_in Tokyo, valid [t0, t2). B supersedes A at t2.
        let t0 = ts(1_700_000_000);
        let t1 = ts(1_700_001_000);
        let t2 = ts(1_700_002_000);
        let t3 = ts(1_700_003_000);
        let mut a = sample_fact("user", "Tokyo", t0);
        a.predicate = "lives_in".into();
        a.valid_to = Some(t2);

        let mut b = sample_fact("user", "Berlin", t2);
        b.predicate = "lives_in".into();
        store.insert(&a).unwrap();
        store.insert(&b).unwrap();

        let at_t1 = store.facts_at_time("user", t1).unwrap();
        assert_eq!(at_t1.len(), 1);
        assert_eq!(at_t1[0].object, "Tokyo");

        // valid_to is exclusive: at t2 exactly, A is out, B is in.
        let at_t2 = store.facts_at_time("user", t2).unwrap();
        assert_eq!(at_t2.len(), 1);
        assert_eq!(at_t2[0].object, "Berlin");

        let at_t3 = store.facts_at_time("user", t3).unwrap();
        assert_eq!(at_t3.len(), 1);
        assert_eq!(at_t3[0].object, "Berlin");

        let before = store.facts_at_time("user", ts(1_699_999_999)).unwrap();
        assert!(before.is_empty());
    }

    #[test]
    fn facts_at_time_hides_retired_rows_at_or_after_retirement() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();

        let t0 = ts(1_700_000_000);
        let retire = ts(1_700_000_500);
        let mut f = sample_fact("user", "rust", t0);
        f.retired_at = Some(retire);
        store.insert(&f).unwrap();

        let before = store.facts_at_time("user", ts(1_700_000_100)).unwrap();
        assert_eq!(before.len(), 1);
        let after = store.facts_at_time("user", ts(1_700_001_000)).unwrap();
        assert!(after.is_empty(), "retired fact should not surface");
    }

    #[test]
    fn facts_at_time_filters_by_subject() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let t0 = ts(1_700_000_000);
        store.insert(&sample_fact("user", "rust", t0)).unwrap();
        store.insert(&sample_fact("other", "python", t0)).unwrap();

        let hits = store.facts_at_time("user", ts(1_700_000_001)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject, "user");
    }

    #[test]
    fn insert_round_trips_through_query() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let f = sample_fact("user", "rust", ts(1_700_000_000));
        store.insert(&f).unwrap();
        let read = store.facts_for_subject("user").unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0], f);
    }

    #[test]
    fn facts_for_subject_returns_retired_rows_for_audit() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let t0 = ts(1_700_000_000);
        let mut f = sample_fact("user", "rust", t0);
        f.retired_at = Some(ts(1_700_000_500));
        store.insert(&f).unwrap();

        // facts_for_subject is audit-oriented; the retired row must surface.
        assert_eq!(store.facts_for_subject("user").unwrap().len(), 1);
        // facts_at_time after retirement hides it.
        assert!(store
            .facts_at_time("user", ts(1_700_001_000))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn empty_source_events_round_trip_through_query() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let mut f = sample_fact("user", "rust", ts(1_700_000_000));
        f.source_events.clear();
        store.insert(&f).unwrap();
        let read = store.facts_for_subject("user").unwrap();
        assert_eq!(read.len(), 1);
        assert!(read[0].source_events.is_empty());
    }

    // ---- T-25 retire_facts_for_target -----------------------------------

    #[test]
    fn retire_facts_for_target_retires_by_capture_id() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let capture = EventId::new();
        let mut f = sample_fact("user", "rust", ts(1_700_000_000));
        f.source_events = vec![capture];
        store.insert(&f).unwrap();

        let n = store
            .retire_facts_for_target(&capture.to_string(), ts(1_700_000_500))
            .unwrap();
        assert_eq!(n, 1);
        let rows = store.facts_for_subject("user").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].retired_at, Some(ts(1_700_000_500)));
    }

    #[test]
    fn retire_facts_for_target_retires_by_fact_id() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let f = sample_fact("user", "rust", ts(1_700_000_000));
        let fact_id = f.id;
        store.insert(&f).unwrap();

        let n = store
            .retire_facts_for_target(&fact_id.to_string(), ts(1_700_000_500))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            store.facts_for_subject("user").unwrap()[0].retired_at,
            Some(ts(1_700_000_500))
        );
    }

    #[test]
    fn retire_facts_for_target_is_idempotent_on_already_retired() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let capture = EventId::new();
        let mut f = sample_fact("user", "rust", ts(1_700_000_000));
        f.source_events = vec![capture];
        f.retired_at = Some(ts(1_700_000_500));
        store.insert(&f).unwrap();

        // Second forget attempt must not bump retired_at (idempotency under
        // repeated replay).
        let n = store
            .retire_facts_for_target(&capture.to_string(), ts(1_700_001_000))
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            store.facts_for_subject("user").unwrap()[0].retired_at,
            Some(ts(1_700_000_500))
        );
    }

    #[test]
    fn retire_facts_for_target_no_match_returns_zero() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let n = store
            .retire_facts_for_target(&EventId::new().to_string(), ts(1_700_000_000))
            .unwrap();
        assert_eq!(n, 0);
    }

    // ---- T-23 is_event_valid_at -----------------------------------------

    #[test]
    fn is_event_valid_at_returns_true_when_capture_has_no_fact() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        // No row at all references `cap_id`: case (no facts) → keep.
        let cap_id = EventId::new().to_string();
        assert!(store.is_event_valid_at(&cap_id, ts(1_700_000_000)).unwrap());
    }

    #[test]
    fn is_event_valid_at_returns_true_inside_validity_window() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let cap_id = EventId::new();
        let mut f = sample_fact("user", "rust", ts(1_700_000_000));
        f.source_events = vec![cap_id];
        store.insert(&f).unwrap();

        assert!(store
            .is_event_valid_at(&cap_id.to_string(), ts(1_700_000_500))
            .unwrap());
    }

    #[test]
    fn is_event_valid_at_returns_false_when_only_retired_facts_exist() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let cap_id = EventId::new();
        let mut f = sample_fact("user", "rust", ts(1_700_000_000));
        f.source_events = vec![cap_id];
        f.retired_at = Some(ts(1_700_000_500));
        store.insert(&f).unwrap();

        // Before retirement: valid → keep.
        assert!(store
            .is_event_valid_at(&cap_id.to_string(), ts(1_700_000_100))
            .unwrap());
        // After retirement: every derived fact is retired → drop.
        assert!(!store
            .is_event_valid_at(&cap_id.to_string(), ts(1_700_001_000))
            .unwrap());
    }

    #[test]
    fn is_event_valid_at_respects_valid_to_window() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let cap_id = EventId::new();
        let mut f = sample_fact("user", "rust", ts(1_700_000_000));
        f.source_events = vec![cap_id];
        f.valid_to = Some(ts(1_700_000_500));
        store.insert(&f).unwrap();

        // Before valid_to: keep.
        assert!(store
            .is_event_valid_at(&cap_id.to_string(), ts(1_700_000_100))
            .unwrap());
        // At valid_to (exclusive upper bound): drop.
        assert!(!store
            .is_event_valid_at(&cap_id.to_string(), ts(1_700_000_500))
            .unwrap());
        // After valid_to: drop.
        assert!(!store
            .is_event_valid_at(&cap_id.to_string(), ts(1_700_001_000))
            .unwrap());
    }

    #[test]
    fn is_event_valid_at_keeps_capture_when_one_of_many_facts_is_valid() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let cap_id = EventId::new();
        // Two facts share the same source capture. One is retired, one is
        // still valid. The capture must be kept.
        let mut retired = sample_fact("user", "rust", ts(1_700_000_000));
        retired.source_events = vec![cap_id];
        retired.retired_at = Some(ts(1_700_000_500));
        let mut alive = sample_fact("user", "haskell", ts(1_700_000_000));
        alive.source_events = vec![cap_id];
        store.insert(&retired).unwrap();
        store.insert(&alive).unwrap();

        assert!(store
            .is_event_valid_at(&cap_id.to_string(), ts(1_700_001_000))
            .unwrap());
    }

    #[test]
    fn from_event_carries_payload_fields() {
        let id = EventId::new();
        let derived = EventId::new();
        let payload = FactPayload {
            subject: "user".into(),
            predicate: "prefers".into(),
            object: "functional rust".into(),
            confidence: 0.8,
            valid_from: ts(1_700_000_000),
            valid_to: None,
            derived_from: vec![derived],
            kind: Default::default(),
            tags: Default::default(),
            extra: Map::new(),
        };
        let recorded = ts(1_700_000_010);
        let fact = Fact::from_event(id, &payload, recorded, Some("p1".into()));
        assert_eq!(fact.id, id);
        assert_eq!(fact.subject, "user");
        assert_eq!(fact.predicate, "prefers");
        assert_eq!(fact.object, "functional rust");
        assert_eq!(fact.source_events, vec![derived]);
        assert_eq!(fact.recorded_at, recorded);
        assert_eq!(fact.retired_at, None);
        assert_eq!(fact.policy_id.as_deref(), Some("p1"));
    }

    // ---- T-51b: tag inheritance + filtered queries --------------------

    fn fact_with_tags(
        subject: &str,
        object: &str,
        valid_from: DateTime<Utc>,
        pairs: &[(&str, &str)],
    ) -> Fact {
        let mut f = sample_fact(subject, object, valid_from);
        for (k, v) in pairs {
            f.tags.insert((*k).to_string(), (*v).to_string());
        }
        f
    }

    #[test]
    fn fact_tags_round_trip_through_insert_and_select() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let inserted = fact_with_tags(
            "user",
            "rust",
            ts(1_700_000_000),
            &[("project", "localmem")],
        );
        store.insert(&inserted).unwrap();
        let read = store.facts_for_subject("user").unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(
            read[0].tags.get("project").map(String::as_str),
            Some("localmem"),
        );
    }

    #[test]
    fn facts_for_subject_filtered_returns_only_matching_tags() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let lm = fact_with_tags(
            "user",
            "rust",
            ts(1_700_000_000),
            &[("project", "localmem")],
        );
        let other = fact_with_tags("user", "go", ts(1_700_000_000), &[("project", "other")]);
        let untagged = sample_fact("user", "haskell", ts(1_700_000_000));
        store.insert(&lm).unwrap();
        store.insert(&other).unwrap();
        store.insert(&untagged).unwrap();

        let mut filter = BTreeMap::new();
        filter.insert("project".into(), "localmem".into());
        let hits = store
            .facts_for_subject_filtered(
                "user",
                Some(&filter),
                crate::reserved_tags::Visibility::Default,
                Utc::now(),
            )
            .unwrap();
        let ids: Vec<_> = hits.iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![lm.id]);
    }

    #[test]
    fn facts_for_subject_filtered_with_none_returns_all_rows() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let a = fact_with_tags("user", "rust", ts(1_700_000_000), &[("project", "lm")]);
        let b = sample_fact("user", "go", ts(1_700_000_000));
        store.insert(&a).unwrap();
        store.insert(&b).unwrap();
        let hits = store
            .facts_for_subject_filtered(
                "user",
                None,
                crate::reserved_tags::Visibility::Default,
                Utc::now(),
            )
            .unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn all_live_facts_filtered_applies_subset_match() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let lm = fact_with_tags(
            "user",
            "rust",
            ts(1_700_000_000),
            &[("project", "localmem"), ("topic", "retrieval")],
        );
        let lm_other_topic = fact_with_tags(
            "alice",
            "rust",
            ts(1_700_000_000),
            &[("project", "localmem"), ("topic", "auth")],
        );
        store.insert(&lm).unwrap();
        store.insert(&lm_other_topic).unwrap();

        let mut filter = BTreeMap::new();
        filter.insert("project".into(), "localmem".into());
        filter.insert("topic".into(), "retrieval".into());
        let now = Utc::now();
        let hits = store
            .all_live_facts_filtered(
                now,
                None,
                Some(&filter),
                crate::reserved_tags::Visibility::Default,
                now,
            )
            .unwrap();
        let ids: Vec<_> = hits.iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![lm.id]);
    }

    // ---- T-51c: reserved-tag visibility/retention on facts ----------

    #[test]
    fn private_fact_is_hidden_under_default_visibility() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let priv_fact = fact_with_tags(
            "user",
            "rust",
            ts(1_700_000_000),
            &[("visibility", "private")],
        );
        store.insert(&priv_fact).unwrap();

        let default = store
            .facts_for_subject_filtered(
                "user",
                None,
                crate::reserved_tags::Visibility::Default,
                Utc::now(),
            )
            .unwrap();
        assert!(default.is_empty(), "private fact must be hidden by default");

        let audit = store
            .facts_for_subject_filtered(
                "user",
                None,
                crate::reserved_tags::Visibility::IncludePrivate,
                Utc::now(),
            )
            .unwrap();
        assert_eq!(audit.len(), 1, "private fact must surface on audit recall");
    }

    #[test]
    fn ephemeral_fact_drops_after_ttl() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let capture_ts = ts(1_700_000_000);
        let eph = fact_with_tags("user", "rust", capture_ts, &[("retention", "ephemeral:1h")]);
        store.insert(&eph).unwrap();

        // Inside TTL: surfaces.
        let inside = store
            .facts_for_subject_filtered(
                "user",
                None,
                crate::reserved_tags::Visibility::Default,
                capture_ts + chrono::Duration::minutes(30),
            )
            .unwrap();
        assert_eq!(inside.len(), 1);

        // Past TTL: dropped, even on audit recall (retention is
        // unconditional).
        let after = store
            .facts_for_subject_filtered(
                "user",
                None,
                crate::reserved_tags::Visibility::IncludePrivate,
                capture_ts + chrono::Duration::hours(2),
            )
            .unwrap();
        assert!(after.is_empty(), "ephemeral fact must drop after TTL");
    }

    // ---- T-56: smart-forgetting / active contradiction resolution ----

    fn fact_with_kind_and_confidence(
        subject: &str,
        predicate: &str,
        object: &str,
        valid_from: DateTime<Utc>,
        kind: crate::kind::Kind,
        confidence: f64,
    ) -> Fact {
        let mut f = sample_fact(subject, object, valid_from);
        f.predicate = predicate.into();
        f.kind = kind;
        f.confidence = confidence;
        f
    }

    #[test]
    fn resolve_contradiction_retires_prior_live_match_at_high_confidence() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        // Prior live fact about user/lives_in/Tokyo.
        let old = fact_with_kind_and_confidence(
            "user",
            "lives_in",
            "Tokyo",
            ts(1_700_000_000),
            crate::kind::Kind::Fact,
            0.8,
        );
        store.insert(&old).unwrap();
        // New fact about user/lives_in/Berlin at high confidence
        // arrives later. The new fact's row is NOT inserted yet;
        // resolve_contradiction returns the ids that *would* be
        // retired so the caller can emit an Update event.
        let new = fact_with_kind_and_confidence(
            "user",
            "lives_in",
            "Berlin",
            ts(1_700_010_000),
            crate::kind::Kind::Fact,
            0.9,
        );
        let retired = store.resolve_contradiction(&new).unwrap();
        assert_eq!(retired, vec![old.id]);
        // The prior row is now retired in the table. We assert via
        // `facts_at_time` (which honours retired_at) rather than
        // facts_for_subject_filtered (audit view, returns retired
        // rows too).
        let live = store.facts_at_time("user", ts(1_700_011_000)).unwrap();
        assert!(
            live.is_empty(),
            "Tokyo row should be retired post-resolution (Berlin not yet inserted)"
        );
    }

    #[test]
    fn resolve_contradiction_skips_when_new_fact_confidence_below_threshold() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let old = fact_with_kind_and_confidence(
            "user",
            "lives_in",
            "Tokyo",
            ts(1_700_000_000),
            crate::kind::Kind::Fact,
            0.9,
        );
        store.insert(&old).unwrap();
        // Low-confidence new fact: append-only, must NOT retire prior.
        let new = fact_with_kind_and_confidence(
            "user",
            "lives_in",
            "Berlin",
            ts(1_700_010_000),
            crate::kind::Kind::Fact,
            0.5,
        );
        let retired = store.resolve_contradiction(&new).unwrap();
        assert!(
            retired.is_empty(),
            "low-confidence facts must not retire prior beliefs"
        );
        // Old row is still live (retired_at IS NULL).
        let live = store.facts_at_time("user", ts(1_700_011_000)).unwrap();
        assert_eq!(live.len(), 1);
    }

    #[test]
    fn resolve_contradiction_skips_when_new_fact_is_a_decision() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let old = fact_with_kind_and_confidence(
            "team",
            "chose",
            "Postgres",
            ts(1_700_000_000),
            crate::kind::Kind::Fact,
            0.9,
        );
        store.insert(&old).unwrap();
        // New fact is itself a Decision: per spec decisions are
        // append-only, so the new decision doesn't invalidate the
        // prior choice.
        let new = fact_with_kind_and_confidence(
            "team",
            "chose",
            "DuckDB",
            ts(1_700_010_000),
            crate::kind::Kind::Decision,
            0.9,
        );
        let retired = store.resolve_contradiction(&new).unwrap();
        assert!(
            retired.is_empty(),
            "Decision new facts must not retire prior facts"
        );
    }

    #[test]
    fn resolve_contradiction_skips_prior_decisions() {
        // The asymmetric case: new fact IS retire-eligible (Fact +
        // high confidence) but the prior row is a Decision. Per
        // spec "Decision kind is append-only" we must NOT retire
        // the decision even when a non-decision new fact would.
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let prior_decision = fact_with_kind_and_confidence(
            "team",
            "chose",
            "Postgres",
            ts(1_700_000_000),
            crate::kind::Kind::Decision,
            0.9,
        );
        store.insert(&prior_decision).unwrap();
        let new = fact_with_kind_and_confidence(
            "team",
            "chose",
            "DuckDB",
            ts(1_700_010_000),
            crate::kind::Kind::Fact,
            0.9,
        );
        let retired = store.resolve_contradiction(&new).unwrap();
        assert!(
            retired.is_empty(),
            "prior Decision rows must stay live regardless of new fact"
        );
    }

    #[test]
    fn resolve_contradiction_no_match_returns_empty() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let new = fact_with_kind_and_confidence(
            "fresh_subject",
            "fresh_predicate",
            "x",
            ts(1_700_000_000),
            crate::kind::Kind::Fact,
            0.9,
        );
        let retired = store.resolve_contradiction(&new).unwrap();
        assert!(retired.is_empty());
    }

    #[test]
    fn resolve_contradiction_does_not_retire_already_retired_rows() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let mut old = fact_with_kind_and_confidence(
            "user",
            "lives_in",
            "Tokyo",
            ts(1_700_000_000),
            crate::kind::Kind::Fact,
            0.9,
        );
        old.retired_at = Some(ts(1_700_000_500));
        store.insert(&old).unwrap();
        let new = fact_with_kind_and_confidence(
            "user",
            "lives_in",
            "Berlin",
            ts(1_700_010_000),
            crate::kind::Kind::Fact,
            0.9,
        );
        let retired = store.resolve_contradiction(&new).unwrap();
        assert!(
            retired.is_empty(),
            "already-retired rows must not surface again as retire candidates"
        );
    }

    #[test]
    fn resolve_contradiction_retires_multiple_matches() {
        // Stretch case but the SQL retires all eligible matches in
        // one call. (Multi-retire after T-56 ships is rare; the
        // caller may only emit ONE Update event per call, so the
        // event log loses subsequent retirements unless we add a
        // multi-supersedes_id field later. This test pins the
        // store-level behavior so a future refactor doesn't drop
        // it.)
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let a = fact_with_kind_and_confidence(
            "user",
            "lives_in",
            "Tokyo",
            ts(1_700_000_000),
            crate::kind::Kind::Fact,
            0.9,
        );
        let b = fact_with_kind_and_confidence(
            "user",
            "lives_in",
            "Osaka",
            ts(1_700_001_000),
            crate::kind::Kind::Fact,
            0.9,
        );
        store.insert(&a).unwrap();
        store.insert(&b).unwrap();
        let new = fact_with_kind_and_confidence(
            "user",
            "lives_in",
            "Berlin",
            ts(1_700_010_000),
            crate::kind::Kind::Fact,
            0.9,
        );
        let retired = store.resolve_contradiction(&new).unwrap();
        let mut sorted = retired.clone();
        sorted.sort();
        let mut expected = vec![a.id, b.id];
        expected.sort();
        assert_eq!(sorted, expected);
    }

    // ---- T-53: discovery primitives -----------------------------------

    #[test]
    fn subjects_groups_and_counts() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        store
            .insert(&sample_fact("user", "rust", ts(1_700_000_000)))
            .unwrap();
        store
            .insert(&sample_fact("user", "haskell", ts(1_700_000_001)))
            .unwrap();
        store
            .insert(&sample_fact("alice", "go", ts(1_700_000_002)))
            .unwrap();
        let rows = store.subjects().unwrap();
        // user comes first (count desc), alice second.
        assert_eq!(rows, vec![("user".into(), 2u64), ("alice".into(), 1u64)]);
    }

    #[test]
    fn subjects_breaks_ties_alphabetically() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        store
            .insert(&sample_fact("zeta", "x", ts(1_700_000_000)))
            .unwrap();
        store
            .insert(&sample_fact("alpha", "x", ts(1_700_000_001)))
            .unwrap();
        let rows = store.subjects().unwrap();
        // Both have count 1, so alpha < zeta lexicographically.
        assert_eq!(rows[0].0, "alpha");
        assert_eq!(rows[1].0, "zeta");
    }

    #[test]
    fn subjects_includes_retired_rows() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let mut f = sample_fact("user", "rust", ts(1_700_000_000));
        f.retired_at = Some(ts(1_700_000_500));
        store.insert(&f).unwrap();
        let rows = store.subjects().unwrap();
        assert_eq!(rows, vec![("user".into(), 1u64)]);
    }

    #[test]
    fn find_by_id_returns_row_when_present() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let f = sample_fact("user", "rust", ts(1_700_000_000));
        let id = f.id;
        store.insert(&f).unwrap();
        let got = store.find_by_id(&id).unwrap();
        assert_eq!(got, Some(f));
    }

    #[test]
    fn find_by_id_returns_none_for_unknown_id() {
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let got = store.find_by_id(&EventId::new()).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn find_by_id_returns_retired_row() {
        // Audit traces need to see retired rows so the lineage walk
        // works even after smart forgetting.
        let tmp = tempdir().unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let mut f = sample_fact("user", "rust", ts(1_700_000_000));
        f.retired_at = Some(ts(1_700_000_500));
        let id = f.id;
        store.insert(&f).unwrap();
        let got = store.find_by_id(&id).unwrap().expect("row present");
        assert_eq!(got.retired_at, Some(ts(1_700_000_500)));
    }
}
