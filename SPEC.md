# localmem v0.1 — Product Spec

**Status:** Locked + Shipped (tagged v0.1.1). This is the historical
contract for the v0.1 release. Preserved here as the reference for
backward compatibility per ARCHITECTURE.md invariant 6 (any
`events.jsonl` written by any v0.1.x must be readable by any later
version).

**v0.2 draft contract:** see [SPEC_V0_2.md](SPEC_V0_2.md). v0.2 is in
design and locks before Phase 5 implementation begins.

The spec describes *what* the product does. The *how* lives in
[ARCHITECTURE.md](ARCHITECTURE.md). The *when* lives in [TASKS.md](TASKS.md).

## What v0.1 must do

A developer using Claude Code, Cursor, or Codex can:

1. Install localmem in one command (`curl ... | bash`).
2. Run `localmem init` and have a working memory directory in their HOME.
3. Connect Claude Desktop or Cursor to localmem via MCP (one config line).
4. Have memories captured automatically from AI tool conversations via the
   `memory_write` MCP tool.
5. Recall those memories semantically and lexically via the `memory_search`
   MCP tool, from any connected AI tool.
6. See an audit trail of every policy decision via `memory_journal`.
7. Recompute every derived store from `events.jsonl` via `localmem replay`,
   proving the trust promise.

If those seven things work end-to-end on macOS and Linux, v0.1 ships.

## CLI surface

All commands accept `--home <path>` and `--json` (machine-readable output).
Default home: `~/.localmem/`. Default output: human-readable.

### `localmem init`
Creates the localmem home directory with empty event log, derived store
directories, default policy, and config file.

| Behavior | |
|---|---|
| Idempotent | Yes. Running on an existing home is a no-op with a warning. |
| Side effects | Creates directory tree, writes config.toml, copies default policy. |
| Failure modes | Permission denied → error message + non-zero exit. Existing non-empty `events.jsonl` → refuses to clobber. |

### `localmem write [--content TEXT] [--source APP] [--kind KIND]`
Ingests a memory. Reads from stdin if `--content` is not provided.

| Behavior | |
|---|---|
| Side effects | Appends a `capture` event to `events.jsonl`. Runs the write policy. May append `fact` events and update derived stores. |
| Output | `{ event_id, action_taken, facts_extracted: [] }` |
| Idempotency | Two identical writes within 60s with identical source → deduped by policy. |

### `localmem search QUERY [--k N] [--at-time TIME]`
Hybrid search over memory.

| Behavior | |
|---|---|
| Side effects | None. Read-only. |
| Output | Ranked list of facts and capture chunks with scores and source event IDs. |
| Default k | 10. Max 100. |
| `--at-time` | Returns facts valid at that point in time (bitemporal). Default: now. |

### `localmem recall ENTITY [--at-time TIME]`
Entity-centric recall. Returns all facts about an entity, ordered by validity period.

### `localmem profile [--scope SCOPE]`
Synthesizes a markdown profile from facts. Default scope: all facts.

### `localmem forget --target ID | --criteria JSON`
Soft-deletes by emitting a `forget` event. Targets stay in `events.jsonl` but
are hidden from queries.

### `localmem journal [--since DURATION] [--action ACTION]`
Prints the policy decision log. Default: last 24 hours. `ACTION` filter:
`COMMIT`, `UPDATE`, `DEDUP`, `SKIP`, `FORGET`.

### `localmem replay`
Rebuilds every derived store from `events.jsonl`. Idempotent. Prints stats.

### `localmem reindex`
Re-embeds all content with the currently configured embedder. Use after
changing the embedding model. Idempotent.

### `localmem import FORMAT PATH`
Bulk-ingests another memory system's export.

Supported FORMATs in v0.1: `chatgpt`, `claude`, `mem0`. `files` (a directory
of text files) is a stretch goal.

### `localmem serve [--addr ADDR]`
Runs the local HTTP server that the MCP server talks to. Default: `127.0.0.1:7788`.

### `localmem export PATH`
Writes a portable archive (events + facts + journal) to `PATH`. The promise
is: any localmem instance can `import` this archive and produce an identical
memory state.

## MCP tool surface

Every tool returns `{ ok: true, ... }` on success, `{ ok: false, error: { code, message } }` on failure. No tool ever returns user content in an error message.

### `memory_write`

```ts
input:  { content: string, source?: string, kind?: string }
output: { ok: true, event_id: string, action: "COMMIT"|"DEDUP"|"SKIP", facts_extracted: number }
```

| Behavior | |
|---|---|
| Latency target | < 200ms p95 |
| Idempotency | Identical content + source within 60s → DEDUP |

### `memory_search`

```ts
input:  { query: string, k?: number, at_time?: string }
output: { ok: true, results: [{ fact: string, score: number, sources: string[], valid_from?: string, valid_to?: string }] }
```

| Behavior | |
|---|---|
| Latency target | < 100ms cold p95, < 30ms warm |
| `at_time` semantics | Returns facts where `valid_from <= at_time < valid_to` |
| Hybrid score | weighted blend of vector + BM25; rerank optional |

### `memory_recall`

```ts
input:  { entity: string, at_time?: string }
output: { ok: true, facts: [{ predicate: string, object: string, valid_from: string, valid_to?: string, sources: string[] }] }
```

### `memory_profile`

```ts
input:  { scope?: string }
output: { ok: true, profile_md: string, generated_at: string, fact_count: number }
```

### `memory_forget`

```ts
input:  { target_id?: string, criteria?: object }
output: { ok: true, forgotten_event_ids: string[] }
```

### `memory_journal`

```ts
input:  { since?: string, action?: string }
output: { ok: true, entries: [{ ts: string, action: string, rule: string, input_id: string, reasoning?: string }] }
```

## Configuration (`~/.localmem/config.toml`)

```toml
[home]
version = 1

[embedder]
model = "bge-small-en-v1.5"
backend = "onnx"                  # onnx | ollama | none

[policy]
default = "policies/default.yaml"
user    = "policies/user.yaml"
llm_assist = false                # true → calls local Ollama for ambiguous cases

[server]
addr = "127.0.0.1:7788"
unix_socket = ""                  # optional, takes precedence if set

[sync]
enabled = false                   # opt-in, never default true
endpoint = ""
key_file = "keys/master.key"

[telemetry]
enabled = false                   # opt-in always
```

Environment variable override pattern: `LOCALMEM_<SECTION>_<KEY>`. Example:
`LOCALMEM_SERVER_ADDR=127.0.0.1:9999`.

## Behavioral invariants (always true)

1. `events.jsonl` is append-only. No code path mutates or deletes lines.
2. Every `memory_*` call records its decision in `journal.log`.
3. `localmem replay` is deterministic given a fixed `events.jsonl`, policy,
   and embedder. Re-running produces byte-identical derived stores.
4. No code path requires a network call to satisfy a `memory_*` MCP call.
5. `localmem export` followed by `localmem import` on a fresh home produces
   the same memory state, within recomputable derived-store tolerance.
6. Schema versioning: an `events.jsonl` written by any v0.x is readable by
   any later v0.x or v1.x.

## What v0.1 does NOT do (explicit scope cut)

- No web UI, no menu bar app, no mobile app.
- No knowledge graph database (DuckDB recursive CTEs only).
- No cloud sync (Act 3 of ROADMAP.md).
- No team / shared contexts (Act 3+).
- No LLM-assisted fact extraction by default (rules only; LLM assist is opt-in flag).
- No Windows builds (Act 2+, after macOS + Linux stable).
- No installer beyond `curl | bash` (no homebrew formula yet).
- No automatic capture from AI tools (the user or the MCP tool must call
  `memory_write` explicitly; capture-on-conversation is a v0.2 polish).

## Acceptance criteria for v0.1 (demoable end-to-end)

The following sequence works on a clean machine in under 2 minutes:

```bash
$ curl -fsSL https://localmem.co/install | bash
$ localmem init
$ localmem serve &                       # background HTTP server

# Configure Claude Desktop MCP to point at the localmem-mcp binary
$ localmem write --content "I prefer functional Rust and avoid macros where possible." --source repl
$ localmem search "code style preferences"
[1] "I prefer functional Rust and avoid macros where possible." score=0.91 sources=[01HXY...]

$ localmem journal --since 1h
ts=...Z action=COMMIT rule=high_signal input=01HXY... reasoning="single declarative preference"

# In Claude Desktop:
> what are my code style preferences?
< Based on your memory, you prefer functional Rust and avoid macros where possible.

# Prove the trust promise
$ rm -rf ~/.localmem/derived
$ localmem replay
Rebuilt facts.duckdb (1 fact), vectors.lance (1 vector), lexical.tantivy (1 doc) in 0.3s
$ localmem search "code style preferences"
[1] "I prefer functional Rust..." score=0.91   # identical result
```

If those nine commands produce those nine outputs on a fresh macOS or Linux
machine, v0.1 ships.
