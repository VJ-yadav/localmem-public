# localmem Architecture

**Status:** v0.1 contract locked. v0.2 additions documented in
"v0.2 additions" below. Both are in force; v0.2 only extends, never
breaks. Changes require a written rationale.

This is the contract every component in the repo follows. It is intentionally
small. The goal is for a competent engineer to read this in 20 minutes and
understand every moving part. Start with the v0.1 sections (the substrate);
the v0.2 additions sit on top of it.

## Design principles (read these first)

1. **The event log is the source of truth.** Everything else is recomputable.
2. **Local-first by default.** Cloud is opt-in, never required, never blocking.
3. **Open formats throughout.** No proprietary serialization. JSONL, DuckDB,
   LanceDB, Parquet, ONNX.
4. **Recomputability is the trust promise.** `localmem replay` rebuilds every
   derived store from the event log. If we lose your faith, you `cat events.jsonl`.
5. **Auditable, always.** Every write/update/forget decision is recorded in the
   journal with the rule or model that made it.
6. **Bitemporal, not last-write-wins.** Facts have `valid_from`/`valid_to` and
   `recorded_at`/`retired_at`. Time is first-class.
7. **MCP is the only public interface.** The binary exposes MCP tools. We do
   not invent our own protocol.

These principles were learned the expensive way from prior systems with
similar shape. Each one earned its place by avoiding a specific failure
mode in production.

## High-level shape

```
┌──────────────────────────────────────────────────────────────┐
│  AI Tool (Claude Desktop, Cursor, Codex, custom GPT, ...)    │
└──────────────────────────────┬───────────────────────────────┘
                               │ MCP (stdio or HTTP)
┌──────────────────────────────▼───────────────────────────────┐
│  mcp-server/   (TypeScript)                                   │
│    memory_write  memory_search  memory_recall                 │
│    memory_profile  memory_forget  memory_journal              │
└──────────────────────────────┬───────────────────────────────┘
                               │ local HTTP / unix socket
┌──────────────────────────────▼───────────────────────────────┐
│  core/   (Rust binary)                                        │
│  ┌───────────────┐   ┌───────────────┐   ┌────────────────┐   │
│  │ event_log     │──▶│ write_policy  │──▶│ derived stores │   │
│  │ (JSONL,       │   │ (rules + LLM) │   │ DuckDB facts   │   │
│  │  append-only) │   └───────────────┘   │ LanceDB vec    │   │
│  └───────────────┘                       │ Tantivy lex    │   │
│                                          │ journal.log    │   │
│                                          └────────────────┘   │
└───────────────────────────────────────────────────────────────┘
            │
            │ optional, opt-in
            ▼
   ┌─────────────────────┐         ┌──────────────────────┐
   │ localmem cloud      │         │ localmem intelligence│
   │ (E2E sync relay,    │         │ (hosted higher-      │
   │  ciphertext only)   │         │  quality extraction) │
   └─────────────────────┘         └──────────────────────┘
        commercial layer (separate repo, separate license)
```

## Storage layout

Everything lives under one directory the user owns. Default: `~/.localmem/`.
Override with `LOCALMEM_HOME` or `--home`.

```
~/.localmem/
├── config.toml                # User config (model choice, policy refs, sync URL)
├── events.jsonl               # SOURCE OF TRUTH (append-only)
├── derived/
│   ├── facts.duckdb           # Bitemporal facts
│   ├── vectors.lance/         # LanceDB ANN index
│   ├── lexical.tantivy/       # BM25 index
│   └── journal.log            # Audit of every policy decision
├── policies/
│   ├── default.yaml           # Shipped defaults
│   └── user.yaml              # User overrides
├── keys/
│   └── master.key             # For optional E2E sync (never leaves machine)
└── cache/
    └── embeddings.bin         # Persistent embedder cache
```

`derived/` and `cache/` are disposable. `events.jsonl` and `keys/master.key`
are the only files a user needs to back up.

## Event log schema

One JSON object per line. Sorted by insertion (append-only). No deletes,
ever. To "delete" a memory, emit a `forget` event.

```jsonc
{
  "id": "01HXYZA1B2C3D4E5F6G7H8J9K0",     // ULID, monotonic
  "ts": "2026-05-14T12:34:56.789Z",       // ISO-8601, UTC
  "kind": "capture",                      // see below
  "payload": { /* kind-specific */ },
  "source": {
    "app": "claude-code",                 // which tool emitted this
    "host": "studio.local",
    "user": "vjsnapp"
  },
  "version": 1                            // schema version
}
```

### Event kinds

| Kind | Purpose | Payload shape (sketch) |
|---|---|---|
| `capture` | Raw input from a tool or user | `{ text, mime, attachments[] }` |
| `fact` | A normalized claim derived from captures | `{ subject, predicate, object, confidence, valid_from, valid_to, derived_from[] }` |
| `update` | Supersede an earlier fact with new info | `{ supersedes_id, new_fact }` |
| `forget` | Soft-delete (still in log, hidden from queries) | `{ target_id, reason, scope }` |
| `policy` | A policy decision (commit/dedup/skip/forget) | `{ rule, input_id, action, reasoning }` |
| `import` | Bulk ingest from another system | `{ source_format, count, batch_id }` |

The `fact` kind is bitemporal. `valid_from`/`valid_to` is when the fact was
true in the real world. The event's own `ts` is when we recorded it. This
lets us answer "what did the system believe at time T about state at time S?"
which is the core capability mem0, Supermemory, and Letta all struggle with.

## Derived stores

### `facts.duckdb`

```sql
CREATE TABLE facts (
  id            TEXT PRIMARY KEY,        -- ULID
  subject       TEXT NOT NULL,
  predicate     TEXT NOT NULL,
  object        TEXT NOT NULL,
  confidence    DOUBLE NOT NULL,
  valid_from    TIMESTAMPTZ NOT NULL,
  valid_to      TIMESTAMPTZ,             -- NULL = currently true
  recorded_at   TIMESTAMPTZ NOT NULL,    -- when we wrote it
  retired_at    TIMESTAMPTZ,             -- when we superseded it
  source_events TEXT[],                  -- ULIDs in events.jsonl
  policy_id     TEXT                     -- which policy committed it
);
CREATE INDEX idx_facts_subject ON facts(subject);
CREATE INDEX idx_facts_valid   ON facts(valid_from, valid_to);
```

Bitemporal queries answer "what was true at time T" without losing the audit
trail. Contradiction resolution updates `valid_to` and `retired_at` instead
of deleting.

### `vectors.lance/`

LanceDB ANN index over fact embeddings and raw capture chunks.
Embedder: BGE-small-en (default, local via ONNX runtime). Swappable.
Re-embedding on model swap is a `localmem reindex` job.

### `lexical.tantivy/`

BM25 index for exact-term and phrase recall. Critical because vector search
alone misses literal identifiers (URLs, function names, dates, error codes).
The hybrid retriever weights vector and lexical scores at query time.

### `journal.log`

Append-only log of every policy decision:
```
ts=2026-05-14T12:34:57Z input=01HXYZ... action=COMMIT rule=high_confidence reasoning="..."
ts=2026-05-14T12:34:58Z input=01HABC... action=DEDUP rule=exact_match dup_of=01HXYZ...
ts=2026-05-14T12:34:59Z input=01HDEF... action=FORGET rule=contradicts retired=01HXYZ...
```

Users can `localmem journal --since "1 day ago"` and see exactly how the
system has been thinking. This is the trust feature no competitor offers.

## Write policy

Every `capture` event flows through the policy layer before any fact lands
in DuckDB. The policy chooses one of:

- `COMMIT` — extract one or more facts, write to DuckDB, embed, index.
- `UPDATE` — supersede an existing fact (emit `update` event).
- `DEDUP` — already seen, skip.
- `SKIP` — not worth remembering (low signal).
- `FORGET` — actively erase a target fact (emit `forget` event).

The policy is a small graph of rules in YAML plus an optional small LLM call
for ambiguous cases. Default rules ship in `policies/default.yaml`. Users
override in `policies/user.yaml`. Every decision lands in `journal.log` with
its reasoning.

The cloud `localmem intelligence` service can run a higher-quality extractor
+ classifier as a paid upgrade. The interface is identical so users can swap
freely.

## MCP tool surface (v0.1)

| Tool | Args | Returns |
|---|---|---|
| `memory_write` | `content: string, source?: string, kind?: string` | `{ event_id, action_taken, facts_extracted: [] }` |
| `memory_search` | `query: string, k?: number, at_time?: string` | `[ { fact, score, sources[] } ]` |
| `memory_recall` | `entity: string, at_time?: string` | `[ { fact, valid_from, valid_to } ]` |
| `memory_profile` | `scope?: string` | `{ profile_md: string, generated_at }` |
| `memory_forget` | `target_id?: string, criteria?: object` | `{ forgotten: [event_ids] }` |
| `memory_journal` | `since?: string, action?: string` | `[ journal_entry ]` |

All tools are idempotent where possible. All return audit-friendly responses.
Schema is validated with `zod` on the TS side and `serde` on the Rust side.

## Binary architecture (Rust)

| Module | Responsibility |
|---|---|
| `cli` | `clap`-based subcommands: `init`, `serve`, `write`, `search`, `replay`, `journal`, `reindex`, `import`, `export` |
| `event_log` | Append-only JSONL writer with crash-safe fsync, ULID generation |
| `policy` | Rule engine + optional LLM call, journal writer |
| `store` | DuckDB facts, LanceDB vectors, Tantivy lexical (each behind a trait) |
| `embed` | ONNX runtime via `ort` crate, BGE-small default, swappable |
| `search` | Hybrid retriever: vector + BM25 + temporal filter + rerank |
| `server` | HTTP server (`axum`) for the MCP server to talk to |
| `replay` | Recompute derived stores from event log |
| `import` | Connectors: ChatGPT export, Claude export, mem0 export, files dir |

Crates (locked at planning time):
`tokio`, `serde`, `serde_json`, `clap`, `anyhow`, `thiserror`, `tracing`,
`tracing-subscriber`, `ulid`, `duckdb`, `lancedb`, `tantivy`, `ort`,
`axum`, `tower`, `chrono`, `dashmap`.

## MCP server architecture (TypeScript)

| File | Responsibility |
|---|---|
| `src/index.ts` | MCP server entry, transport selection (stdio / http) |
| `src/tools/*.ts` | One file per MCP tool, schema + handler |
| `src/client.ts` | Talks to core binary over local HTTP / unix socket |
| `src/types.ts` | Shared zod schemas |

Stateless. The TS side does not store anything. Just translates MCP calls
into HTTP calls to the Rust binary.

Dependencies: `@modelcontextprotocol/sdk`, `zod`, `undici` (HTTP), `bun`
(runtime). We ship a single executable via `bun build --compile`.

## What is intentionally NOT here

- No knowledge graph database. We use foreign keys + DuckDB recursive CTEs.
  A real KG would be premature for v0.1 and locks us into a heavier
  storage layer.
- No streaming/real-time ingestion. v0.1 is request/response only. Streams come
  in v0.3 when we have a use case.
- No multi-tenant cloud storage. The cloud (when it lands) is a sync relay
  storing ciphertext blobs. We never see plaintext.
- No web UI in v0.1. The MCP tools are the UI. A menu bar app comes in v0.2.
- No mobile app. v0.3 at earliest, once sync is rock solid.

## Versioning

Schema version is in every event. Migrations are functions that translate
event payloads forward. The event log is never rewritten; migrations apply
at read time via `replay`. This means: even a 5-year-old `events.jsonl`
can be read by a current binary.

## Performance targets (v0.1)

| Operation | Target |
|---|---|
| `memory_write` end-to-end | < 200ms |
| `memory_search` (hybrid, k=10) | < 100ms cold, < 30ms warm |
| Embedding 1KB chunk locally | < 50ms |
| `localmem replay` over 100k events | < 30s on a M-series Mac |

These are aspirations, not contracts. They guide design choices (e.g., use
local ONNX embeddings, not cloud calls; use LanceDB, not Chroma).

# v0.2 additions

Everything above is the v0.1 contract and remains in force. v0.2 only
extends, never breaks: an `events.jsonl` written by v0.1 replays
identically under v0.2. Shipped capabilities are tagged inline below.

## What v0.2 adds (functional summary)

1. **Container tags** on captures + facts. `key=value` metadata
   scoped per memory, with two reserved keys (`retention`,
   `visibility`) carrying behavior. Tag-aware filtering on every
   read path. [`core/src/event.rs`](core/src/event.rs)
   `CapturePayload.tags` / `FactPayload.tags`,
   [`core/src/reserved_tags.rs`](core/src/reserved_tags.rs),
   [`core/src/tag_match.rs`](core/src/tag_match.rs).
2. **Closed-core kind taxonomy.** 6 canonical kinds (`fact`,
   `preference`, `decision`, `constraint`, `todo`, `note`) plus a
   forward-compatible `Other(String)` escape hatch.
   [`core/src/kind.rs`](core/src/kind.rs).
3. **Context rewriting at ingest.** Captures are rewritten to be
   self-contained before indexing ("they prefer X" → "Vijay prefers
   X"). Four modes (`none`, `regex`, `local-llm`, `hosted`); the
   original `text` is preserved verbatim for audit.
   [`core/src/rewriter.rs`](core/src/rewriter.rs).
4. **Active contradiction resolution (smart forgetting).** A new
   high-confidence fact about an existing `(subject, predicate)`
   atomically retires the prior fact and emits an `Update` event.
   Decisions are append-only and exempt.
   [`core/src/facts.rs`](core/src/facts.rs)
   `FactsStore::resolve_contradiction`.
5. **Recency bias on hybrid search.** RRF score gets a
   `weight * exp(-age_days / 30)` bonus before the final sort.
   [`core/src/retriever.rs`](core/src/retriever.rs)
   `apply_recency_bonus`.
6. **Discovery surface.** Five new read-only CLI commands —
   `subjects`, `tags`, `summarize`, `recent`, `audit` — plus four
   MCP Resource URIs (`localmem://profile`, `subjects`, `tags`,
   `recent`) so AI clients can browse memory before guessing
   queries. [`core/src/cli/`](core/src/cli/),
   [`mcp-server/src/resources.ts`](mcp-server/src/resources.ts).
7. **First-run import wizard.** `localmem init` scans `~/Downloads`,
   `~/Desktop`, and CWD for ChatGPT/Claude exports and points users
   at `localmem import-wizard`. Onboarding starts non-empty.
   [`core/src/cli/import_wizard.rs`](core/src/cli/import_wizard.rs).
8. **One-line install with auto-MCP-wiring.** `localmem mcp install
   <client>` writes the client's config for Claude Desktop, Cursor,
   Windsurf, Cline, Claude Code. `localmem doctor` reports
   PASS/WARN/FAIL across binary, home, model, server, Gatekeeper,
   per-client MCP wiring. [`core/src/cli/mcp_clients/`](core/src/cli/mcp_clients/),
   [`core/src/cli/doctor.rs`](core/src/cli/doctor.rs).

Personal Cloud sync and Hosted Intelligence (paid, opt-in) are
specified in SPEC_V0_2 but deferred to v0.2.1; nothing in this
addendum requires a network call.

## Event payload extensions

The six v0.1 event kinds are unchanged. v0.2 additive fields on
`CapturePayload` and `FactPayload`:

```jsonc
// CapturePayload (v0.2)
{
  "text": "I prefer functional Rust",
  "rewritten_text": "Vijay prefers functional Rust",   // T-55, optional
  "kind": "preference",                                  // T-52, default "note"
  "tags": { "project": "localmem", "topic": "lang" },   // T-51, optional
  "mime": null,
  "attachments": [],
  // forward-compat catch-all (every payload)
  // ...
}

// FactPayload (v0.2) — inherits kind + tags from source capture
{
  "subject": "user",
  "predicate": "prefers",
  "object": "functional rust",
  "confidence": 0.8,
  "valid_from": "2026-05-17T...",
  "derived_from": ["01HXY..."],
  "kind": "preference",                                  // T-52
  "tags": { "project": "localmem" }                      // T-51b
}
```

Wire-compat rules:

- `skip_serializing_if` keeps every new field absent on disk when
  it carries its default. A v0.2 binary writing a vanilla capture
  produces a line byte-identical to v0.1.
- A v0.1 binary reading a v0.2 event captures the unknown fields in
  its `extra` flatten map and re-emits them intact. Backward-compat
  is verified by serde round-trip tests in
  [`core/src/event.rs`](core/src/event.rs).
- New facts always inherit `kind` + `tags` from the source capture
  at extraction time (see `build_fact_event` in
  [`core/src/cli/write.rs`](core/src/cli/write.rs),
  [`core/src/server/routes.rs`](core/src/server/routes.rs),
  [`core/src/indexer.rs`](core/src/indexer.rs)). Same value lives on
  the event AND the DuckDB row so `replay` rebuilds without joining.

`indexable_text()` on `CapturePayload` centralises the "use
rewritten if present, else original" rule so the lex indexer, vec
embedder, and snippet path stay aligned.

## Derived-store schema changes

### `facts.duckdb`

Two migrations applied on open
([`core/src/facts.rs`](core/src/facts.rs)):

```sql
-- 0002: container tags inherited from the source capture (T-51b)
ALTER TABLE facts ADD COLUMN tags JSON;

-- 0003: closed-core kind inherited from the source capture (T-52)
ALTER TABLE facts ADD COLUMN kind TEXT;
```

Both columns are nullable: v0.1.x rows pre-date them, and the read
path treats `NULL` as "empty tags" / `Kind::Note`. `policy_id`,
`retired_at`, and the other v0.1 columns are unchanged.

Two new query primitives added in T-53:

- `FactsStore::subjects() -> Vec<(String, u64)>` — distinct
  subjects with row counts, includes retired rows for audit framing.
- `FactsStore::find_by_id(&EventId) -> Option<Fact>` — single-fact
  lookup, includes retired rows.

T-56's `resolve_contradiction(&new_fact) -> Vec<EventId>` is the
core of smart forgetting: returns ids of still-live prior facts
with matching `(subject, predicate)` and atomically sets their
`retired_at = new_fact.valid_from`. Gated by two predicates:
new fact's `confidence >= 0.7` AND
`new_fact.kind.allows_contradiction_resolution()` (false only for
`Decision`). Decisions in the prior set are also excluded — append-only.

### `lexical.tantivy/`

The schema gains a stored-only `tags` field
([`core/src/lexical.rs`](core/src/lexical.rs)) carrying the
capture's tag map as JSON. STORED-only because the filter is
applied post-search via overfetch + Rust-side subset match — keeps
the index format simple while preserving the v0.1 query path.

`LexicalHit` also carries `ts` and `tags` so the hybrid retriever
can run reserved-tag (visibility, retention) + recency-bias logic
without a second lookup. `meta_for(event_id)` returns the same pair
for vec-hit enrichment.

### `vectors.lance/`

Schema unchanged. Vec hits don't carry tag metadata; the retriever
looks them up via `LexicalIndex::meta_for` so both filters and
recency-bias `ts` are answerable.

### `journal.log`

Format unchanged. T-56 contradiction-resolution entries land with
`action=UPDATE, rule="smart_forgetting"` and reasoning naming the
retired fact id.

## Write pipeline (v0.2)

```
capture text ──▶ rewriter (T-55)             ──▶ rewritten_text? on CapturePayload
              ──▶ policy.evaluate            ──▶ Decision + journal entry
              ──▶ if COMMIT:
                    ├─ lex.index_event (uses indexable_text)
                    ├─ embedder.embed → vectors.add (uses indexable_text)
                    └─ extractor.extract → for each fact:
                          ├─ inherit kind + tags from capture
                          ├─ facts.resolve_contradiction      (T-56)
                          ├─ if contradiction:
                          │     emit Update event (supersedes_id)
                          │     journal "smart_forgetting" entry
                          │  else:
                          │     emit Fact event
                          └─ facts.insert(new_fact)
```

The CLI write path
([`core/src/cli/write.rs`](core/src/cli/write.rs)) and the HTTP
`/write` route ([`core/src/server/routes.rs`](core/src/server/routes.rs))
mirror each other. The duplication is intentional: lifting it into
a shared helper would force the CLI through the server's
Arc/Mutex synchronisation. We accept the ~50 lines of duplication
in exchange for two cleaner ownership stories. Tests in both files
cover the pipeline end-to-end.

## Search pipeline (v0.2)

```
query ──▶ overfetch fetch = k * OVERFETCH from each retriever
       ──▶ lex pass → lex_hits (carry ts + tags)
       ──▶ embed query → vec ANN → vec_hits
              ──▶ meta_for(vec.id) seeds tag filter + ts side map
       ──▶ apply tag subset filter (Filters.tags, T-51)
       ──▶ apply reserved-tag visibility/retention (T-51c)
       ──▶ RRF merge by event_id (score = Σ 1/(60 + rank + 1))
       ──▶ bitemporal filter via FactsStore::is_event_valid_at (T-23)
       ──▶ recency bonus: score += w * exp(-age_days / 30) (T-57)
       ──▶ sort desc, truncate to k
```

`w` defaults to `0.01` (≈ one RRF rank position for a fresh
capture); set `[retriever].recency_weight = 0.0` in `config.toml`
to disable. The CLI retriever
([`core/src/retriever.rs`](core/src/retriever.rs)) and the server's
duplicated `hybrid_search` both call the shared
`apply_recency_bonus(score, ts, now, weight)` helper to stay
aligned.

## Reserved tag semantics

Two `key=value` tags carry behavior; every other key is opaque
user data. See [`core/src/reserved_tags.rs`](core/src/reserved_tags.rs).

| Key | Values | Behavior |
|---|---|---|
| `retention` | `permanent` (default) / `ephemeral:<TTL>` (e.g. `ephemeral:24h`) | Ephemeral captures drop from every read path once `now - capture_ts > TTL`. Unconditional — applies even on audit recall. |
| `visibility` | `surfaced` (default) / `private` | Private captures hide from `search`, `profile`, and the discovery surface. Surfaced ONLY by entity-only `recall(entity=X)` audit calls. |

TTL grammar: integer + unit, units `m` / `h` / `d` / `w`. Parsing
is strict (malformed TTL → opaque tag, no expiry applied) so a
typo doesn't silently make memories evaporate.

## Discovery API + MCP Resources

Five CLI commands (T-53) sourced from derived state, never event
kinds. Each is rebuildable via `localmem replay`.

| CLI | Returns | Source |
|---|---|---|
| `localmem subjects` | Distinct fact subjects + counts | `FactsStore::subjects()` |
| `localmem tags` | `key=value` aggregate across captures | `EventLog` walk |
| `localmem summarize [--tags ... --kind ...]` | Markdown brief, filtered | wraps `profile::run_with_kind` |
| `localmem recent [--limit N]` | Last N captures, newest first | `EventLog` walk |
| `localmem audit <fact-id>` | Fact + source captures + journal + touches | `find_by_id` + log + journal scan |

Four MCP Resources (T-54) mirror those primitives with live state
the AI client can `resources/read` or subscribe to:

| URI | HTTP backing route |
|---|---|
| `localmem://profile` | `GET /resource/profile` |
| `localmem://subjects` | `GET /resource/subjects` |
| `localmem://tags` | `GET /resource/tags` |
| `localmem://recent[?limit=N]` | `GET /resource/recent?limit=N` (max 200) |

Subscription support
(`notifications/resources/list_changed`) is reserved for T-65; the
shape is in place.

## MCP tool surface (v0.2)

The v0.1 six tools all remain ([`mcp-server/src/tools.ts`](mcp-server/src/tools.ts)),
backward-compatible. v0.2 adds **Resources** (above) as a peer
surface; the SPEC_V0_2 tool redesign (`memory`, `recall`, `update`,
`import_inline`) is reserved for T-63 and ships alongside.

## Closed-core kind taxonomy

`Kind` is a closed-set enum + `Other(String)` escape hatch
([`core/src/kind.rs`](core/src/kind.rs)). Serde round-trips via
`From<String>` so unknown kinds preserve their value verbatim.

| Variant | `allows_contradiction_resolution()` | Profile section |
|---|---|---|
| `Fact` | yes | Facts |
| `Preference` | yes | Preferences |
| `Decision` | **no** (append-only audit) | Decisions |
| `Constraint` | yes | Constraints |
| `Todo` | yes | Todos |
| `Note` (default) / `Other(_)` | yes | Other |

Decision facts never retire prior beliefs AND prior decision facts
never get retired by an incoming fact. The asymmetry matches the
spec phrasing "decisions are historical."

## Context-rewriter trait

`Rewriter::rewrite(text, user_name) -> Result<String>`
([`core/src/rewriter.rs`](core/src/rewriter.rs)). Four
implementations dispatched by `[rewriter].mode` in `config.toml`:

| Mode | Status | Behavior |
|---|---|---|
| `none` (default) | shipped | Identity. Wire shape stays v0.1-compatible. |
| `regex` | shipped | Deterministic pronoun substitution via word-boundary regex. Fast, no LLM. |
| `local-llm` | stub | Bails loudly so a config typo surfaces. Real impl with T-58 + T-62. |
| `hosted` | stub | Reserved for v0.2.1 (T-68). |

Rewriter failures degrade gracefully: a regex/LLM error logs WARN
and the capture commits with `rewritten_text = None` rather than
losing the user's write. Test coverage: 16 rewriter unit tests + 4
config tests + 4 CLI integration tests + 2 server tests.

## Config additions (`config.toml`)

```toml
[home]
user_name = ""                    # rewriter substitution target; "" → $USER → "the user"

[rewriter]
mode = "none"                     # none | regex | local-llm | hosted

[retriever]
recency_weight = 0.01             # 0.0 disables; T-57

# inherited from v0.1: [embedder], [policy], [server], [sync], [telemetry]
```

Each section has a sensible default. Missing `config.toml` returns
`Config::default()` — the binary works on first run with no
configuration.

Env overrides honor the `LOCALMEM_<SECTION>_<KEY>` convention from
v0.1: `LOCALMEM_REWRITER_MODE`, `LOCALMEM_HOME_USER_NAME`,
`LOCALMEM_RETRIEVER_RECENCY_WEIGHT` are wired in v0.2. Empty values
are treated as "unset" so a stray `export VAR=""` doesn't clobber
the disk value.

## Binary architecture (v0.2 additions)

| Module | Responsibility |
|---|---|
| `cli::subjects` / `cli::tags` / `cli::summarize` / `cli::recent` / `cli::audit` | T-53 discovery CLI |
| `cli::import_wizard` | Capability #5 first-run scan + `--apply` |
| `cli::mcp` / `cli::mcp_clients` | T-50 auto-install per AI client |
| `cli::doctor` | T-48 install diagnostic |
| `kind` | T-52 closed-core taxonomy |
| `reserved_tags` | T-51c retention/visibility rules |
| `tag_match` | T-51 subset-match predicate |
| `rewriter` | T-55 context rewriter trait + 4 modes |

The MCP server gains
[`mcp-server/src/resources.ts`](mcp-server/src/resources.ts) and a
shared `handleResponse` helper on `CoreClient` for the new `GET`
codepath.

## Performance targets (v0.2 additions)

Inherits the v0.1 targets. New surfaces:

| Operation | Target | Measurement |
|---|---|---|
| `memory_write` (commit + rewriter + extractor + indexes) p95 | < 200 ms at 100K events | `localmem benchmark write` (planned, T-72) |
| `memory_search` warm | < 100 ms at 100K events | benchmark target |
| MCP `resources/read localmem://profile` | < 50 ms | bounded by facts query + markdown synth |
| `localmem replay` over 100K events | < 30 s on M-series | unchanged from v0.1 |
| LongMemEval / LoCoMo (private) | ≥ 75% before public publish | `localmem benchmark longmemeval` (planned) |

Numbers below the threshold gate the v0.2 launch.
