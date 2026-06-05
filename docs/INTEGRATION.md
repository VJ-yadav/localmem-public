# Integration Phase (T-23 to T-25)

Sequential follow-up to Phase 2. Each task is a separate PR. Runs in the
main localmem worktree (`/Users/vjsnapp/DATA_LAB/localmem`), not in a
sibling — Integration is sequential, not parallel.

Read first: [SPEC.md](../SPEC.md), [ARCHITECTURE.md](../ARCHITECTURE.md),
[TASKS.md](../TASKS.md) tasks T-23, T-24, T-25, and the existing module
files for `lexical`, `vectors`, `embed`, `facts`, `event_log`.

---

## T-23 — Hybrid retriever (~3h)

**File to fill in:** `core/src/retriever.rs` (stub already in lib.rs).

### Design

A query produces ranked results by blending three signals:

1. **Lexical** (`LexicalIndex::search`): BM25 over capture content. Catches
   exact terms (URLs, function names, error codes, ULIDs, dates) that
   embeddings miss.
2. **Vector** (`VectorStore::search` after embedding the query via
   `Embedder::embed`): semantic similarity. Catches paraphrases and
   conceptual matches.
3. **Temporal** (optional, gated by `at_time`): if the caller wants
   "what was true at time T", drop hits whose derived facts have been
   retired by then.

### Why Reciprocal Rank Fusion (RRF)

Vector scores live in `[0, 2]` (cosine distance) and BM25 scores are
unbounded floats centered around `0.5 to 20+`. Direct weighted sum needs
calibration per-corpus and is brittle.

RRF avoids the scale problem entirely by ranking each retriever's output
and combining by **rank position, not raw score**:

```
rrf_score(hit, retriever) = 1.0 / (k_rrf + rank_in_retriever)
final_score(hit) = sum(rrf_score(hit, retriever) for retriever in {lex, vec})
```

`k_rrf = 60` is the folklore default from the original RRF paper. Hits
that appear in both retrievers' top-N get summed scores and naturally
rise to the top. Hits in only one retriever's top-N still surface.

### Sketch

```rust
pub struct HybridRetriever {
    embedder: Embedder,
    vectors: VectorStore,
    lexical: LexicalIndex,
    facts: FactsStore,
}

const RRF_K: f32 = 60.0;
const OVERFETCH: usize = 3; // pull 3*k from each retriever before merge

#[derive(Debug, Serialize)]
pub struct HybridHit {
    pub event_id: String,
    pub content: String,
    pub score: f32,
    pub sources: Vec<&'static str>, // ["lex"], ["vec"], or both
}

impl HybridRetriever {
    pub async fn search(
        &mut self,
        query: &str,
        k: usize,
        at_time: Option<DateTime<Utc>>,
    ) -> Result<Vec<HybridHit>> {
        if k == 0 { return Ok(Vec::new()); }

        // 1. Pull top-N from each retriever in parallel.
        let lex_hits = self.lexical.search(query, OVERFETCH * k)?;
        let query_vec = self.embedder.embed(query)?;
        let vec_hits = self.vectors.search(&query_vec, OVERFETCH * k).await?;

        // 2. RRF merge by event_id.
        let mut merged: HashMap<String, HybridHit> = HashMap::new();
        for (rank, h) in lex_hits.iter().enumerate() {
            merged.entry(h.event_id.clone())
                .and_modify(|m| {
                    m.score += 1.0 / (RRF_K + rank as f32 + 1.0);
                    m.sources.push("lex");
                })
                .or_insert(HybridHit { /* from lex */ });
        }
        // ... same for vec_hits ...

        // 3. Apply temporal filter if at_time given.
        if let Some(t) = at_time {
            merged.retain(|event_id, _| self.is_valid_at(event_id, t).ok().unwrap_or(true));
        }

        // 4. Sort by score desc, truncate to k.
        let mut out: Vec<HybridHit> = merged.into_values().collect();
        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        out.truncate(k);
        Ok(out)
    }

    /// Temporal filter: returns true if the hit is still considered valid
    /// at `at_time`. A hit is invalid if it has derived facts AND all of
    /// them have been retired by that time. A hit with no derived facts
    /// (a raw capture we never extracted from) passes through.
    fn is_valid_at(&self, event_id: &str, at_time: DateTime<Utc>) -> Result<bool> {
        // SQL below.
    }
}
```

### Bitemporal temporal filter (DuckDB)

The `facts` table from T-11 has these bitemporal columns:

```
valid_from    TIMESTAMPTZ NOT NULL    -- when the fact was true in reality
valid_to      TIMESTAMPTZ              -- when it stopped being true (NULL = still true)
recorded_at   TIMESTAMPTZ NOT NULL    -- when we wrote it
retired_at    TIMESTAMPTZ              -- when we superseded it
source_events TEXT[]                   -- the capture event(s) this fact came from
```

To find facts derived from a specific capture event AND still valid at `at_time`:

```sql
SELECT 1
FROM facts
WHERE $1 = ANY(source_events)
  AND valid_from <= $2::TIMESTAMPTZ
  AND (valid_to IS NULL OR valid_to > $2::TIMESTAMPTZ)
  AND (retired_at IS NULL OR retired_at > $2::TIMESTAMPTZ)
LIMIT 1;
```

If this returns a row, the capture has a still-valid fact → **keep the hit**.
If it returns no rows but there ARE rows in `facts` for `source_events @> [event_id]`
(any retirement state), the capture's fact has been retired → **drop the hit**.
If there are NO rows at all (the capture never produced a fact), → **keep the hit**.

A single query covers all three cases:

```sql
WITH has_facts AS (
    SELECT 1 AS x FROM facts WHERE $1 = ANY(source_events) LIMIT 1
),
has_valid AS (
    SELECT 1 AS x FROM facts
    WHERE $1 = ANY(source_events)
      AND valid_from <= $2::TIMESTAMPTZ
      AND (valid_to IS NULL OR valid_to > $2::TIMESTAMPTZ)
      AND (retired_at IS NULL OR retired_at > $2::TIMESTAMPTZ)
    LIMIT 1
)
SELECT
    CASE
        WHEN (SELECT x FROM has_facts) IS NULL THEN true   -- no fact ⇒ keep
        WHEN (SELECT x FROM has_valid) IS NOT NULL THEN true -- valid fact ⇒ keep
        ELSE false                                          -- only retired facts ⇒ drop
    END AS keep;
```

Wrap this in a `FactsStore::is_event_valid_at(&self, event_id: &str, at_time: DateTime<Utc>) -> Result<bool>` method to keep the SQL out of the retriever.

### Tests for T-23

| Test | What it proves |
|---|---|
| `empty_query_returns_empty` | k=0 short-circuit |
| `lex_only_hit_surfaces` | RRF returns hits that match only lexically |
| `vec_only_hit_surfaces` | RRF returns hits that match only semantically |
| `both_retrievers_rank_higher` | Hits in both retrievers score above hits in one |
| `temporal_filter_drops_retired_fact` | at_time=now drops a capture whose fact was retired yesterday |
| `temporal_filter_keeps_capture_without_facts` | At-time filter doesn't drop a never-extracted capture |
| `k_limits_output_size` | output.len() <= k |
| `same_event_id_does_not_dedupe_to_zero_score` | Aggregation accumulates, not min/max |

---

## T-24 — `localmem search` defaults to hybrid (~1h)

**File:** `core/src/cli/search.rs`. Already has `Mode::Lex|Vec|Hybrid` from
Group A; `Vec` and `Hybrid` currently error with "lands with T-23".

Replace those error branches with calls into `HybridRetriever`. Flip the
default `Mode` to `Hybrid`. Keep `Lex` as an explicit override for users
who want exact-term-only results.

```rust
pub async fn run(/* args */, mode: Mode, /* ... */) -> Result<()> {
    // ... open stores ...
    let hits = match mode {
        Mode::Lex => {
            // existing path
        }
        Mode::Vec | Mode::Hybrid => {
            let mut retriever = HybridRetriever::open(&home).await?;
            retriever.search(query, k, at_time).await?
        }
    };
    // ... emit human or JSON output ...
}
```

Update the existing CLI tests so `--mode hybrid` works end-to-end.

---

## T-25 — `localmem replay` (~4h)

**File to create:** `core/src/cli/replay.rs`.

### Goal

Delete `~/.localmem/derived/` and recompute every derived store from
`events.jsonl` alone. This proves the trust property from
[MOAT.md](../MOAT.md): the data is portable because the derived state is
recomputable.

### Sketch

```rust
pub async fn run(home: PathBuf) -> Result<()> {
    let event_log = EventLog::open(&home)?;
    let event_count = event_log.iter()?.count();
    info!(event_count, "starting replay");

    // 1. Drop existing derived/ — atomic via rename-then-rm so a crash
    //    mid-replay leaves the old state intact.
    let derived = home.join("derived");
    let stash = home.join("derived.old");
    if derived.exists() {
        std::fs::rename(&derived, &stash)?;
    }
    std::fs::create_dir_all(&derived)?;

    // 2. Re-open every store (creates empty derived files).
    let mut indexer = Indexer::new(
        Embedder::load(&model_dir)?,
        VectorStore::open(&home, EMBEDDING_DIM).await?,
        LexicalIndex::open(&home)?,
        Extractor::new(),
        FactsStore::open(&home)?,
    );
    let journal = Journal::open(&home)?;
    let policy = Policy::load_default()?;

    // 3. Walk events.jsonl, dispatch by kind.
    let mut stats = ReplayStats::default();
    for event_result in event_log.iter()? {
        let event = event_result?;
        match &event.kind {
            EventKind::Capture(_) => {
                // Full write pipeline: policy decides COMMIT/SKIP, then
                // index_event + process_capture_facts if committed.
                let decision = policy.evaluate(&event, /* context */)?;
                journal.append(&JournalEntry::from_decision(&decision, event.id, event.ts))?;
                if decision.action == PolicyAction::Commit {
                    indexer.index_event(&event).await?;
                    indexer.process_capture_facts(&event, &event_log)?;
                    stats.committed += 1;
                }
            }
            EventKind::Fact(_) => {
                // Fact event from the original session — re-insert into
                // facts table. (process_capture_facts above also emits
                // fact events, so handle the dedup case: skip if id exists.)
            }
            EventKind::Forget(payload) => {
                // Mark the target's facts retired at this event's ts.
            }
            EventKind::Update(_) | EventKind::Policy(_) | EventKind::Import(_) => {
                // Reapply each kind's effect.
            }
        }
    }

    // 4. Drop the stashed old derived/.
    if stash.exists() {
        std::fs::remove_dir_all(stash)?;
    }
    println!("replay: {stats:?}");
    Ok(())
}
```

### Idempotency

Running `localmem replay` twice on the same `events.jsonl` MUST produce
byte-identical derived stores (per ARCHITECTURE.md invariant 3). Test
this by:

1. Replay once, hash every file under `derived/`.
2. Replay again.
3. Hash again. Compare. Must match.

Caveats:
- LanceDB row order may not be byte-stable; use **content hash of sorted
  rows** for the vector store comparison, not file-byte hash.
- Tantivy segment names include random IDs; same story. Compare via
  `LexicalIndex::doc_count` + a roundtrip of every doc.

### Tests for T-25

| Test | What it proves |
|---|---|
| `empty_log_replays_to_empty_stores` | Bare-minimum case |
| `replay_recreates_facts_byte_identical_for_simple_log` | Determinism |
| `replay_with_forget_marks_fact_retired` | Tombstone semantics |
| `replay_twice_is_idempotent` | Invariant 3 |
| `replay_after_partial_corruption_recovers` | Mid-replay error doesn't lose old state (the `derived.old` stash) |
| `replay_emits_stats_with_correct_counts` | Observability |

---

## Acceptance criteria for Integration

After T-23 + T-24 + T-25 all merge:

1. `localmem search "stripe webhook"` (no `--mode`) returns hybrid results.
2. `localmem search "01HXYZ..."` returns the exact-id match via the lexical path.
3. `localmem search "preferences" --at-time 2026-03-01T00:00Z` filters out facts that have been retired since then.
4. `rm -rf ~/.localmem/derived && localmem replay` recreates the derived stores and `localmem search` returns equivalent results.

When these pass, the localmem v0.1 backend is complete. Next is T-26+
(MCP server in TypeScript) which is mechanical wiring on top.

---

## Suggested execution prompt

Paste this into a Claude Code session in `/Users/vjsnapp/DATA_LAB/localmem`:

> You are executing Integration phase (T-23, T-24, T-25) for localmem.
> Foundation + all 5 parallel groups have shipped to main. Read
> [docs/INTEGRATION.md](docs/INTEGRATION.md), [SPEC.md](SPEC.md),
> [ARCHITECTURE.md](ARCHITECTURE.md), and [CLAUDE.md](CLAUDE.md) before
> doing anything.
>
> Implement T-23 (hybrid retriever) first. Branch as `integration-t23`,
> commit per the sketch in INTEGRATION.md, push, open a PR to main, wait
> for CI. After it merges, do T-24 (~1h) and T-25 (~4h) the same way,
> one PR each.
>
> Do not modify Foundation or any of the parallel-group files (event*,
> lexical, vectors, embed, facts, extractor, indexer, policy, journal,
> server, cli/search, cli/journal) except where INTEGRATION.md
> explicitly calls for it (the `Vec`/`Hybrid` branches in cli/search.rs
> for T-24).
>
> The bitemporal filter SQL in INTEGRATION.md belongs in a new
> `FactsStore::is_event_valid_at` method, not inlined in the retriever.
