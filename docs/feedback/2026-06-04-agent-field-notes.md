# localmem field notes from an agent session (2026-06-04)

Field feedback from a Claude Code agent (Opus) that used localmem v0.1 via the CLI on macOS
(darwin 25.2.0), across two homes: global `~/.localmem` and a fresh project store
`/Users/vjsnapp/DATA_LAB/simplestub/.localmem`. Each item has a repro and acceptance criteria.

Do not modify the Foundation files (event.rs, event_id.rs, event_log.rs, lib.rs, main.rs) without
explicit instruction; the event log is append-only and is the source of truth.

## P1 - Self-healing lexical index on schema mismatch
Repro: `localmem search "anything"` against `~/.localmem` failed hard with:
```
Error: open lexical index for reading
Caused by: open or create tantivy index -> Schema error: 'An index exists but the schema does not match.'
```
Cause hypothesis: binary upgraded with a changed tantivy schema; on-disk `derived/lexical.tantivy`
was not migrated. Workaround that fixed it: `localmem replay` rebuilt the index from `events.jsonl`.

Fix: stamp a schema version on the lexical index; on open, if versions differ, auto-rebuild from the
event log, or at minimum return an error whose message says exactly `run: localmem replay`.

Acceptance: after a schema bump the first search either self-heals or prints the actionable command;
a regression test simulates a stale-schema index and asserts recovery. Never dead-ends a read.

## P1 - Make the MCP server the default agent path + recall-at-session-start
Observation: the MCP server exists (`~/Library/Logs/Claude/mcp-server-localmem.log`) but was NOT
connected to the session, so the agent used the CLI via shell. CLAUDE.md and the harness file-memory
get auto-injected into agent context; localmem does not, so it is opt-in/manual today. That is the
single biggest disadvantage versus static memory files.

Fix: ship a documented, reliable MCP integration that an agent reaches for naturally, plus a
"recall top-K relevant facts at session start" pattern (a tool the agent calls, or a documented
bootstrap).

Acceptance: a fresh agent session can retrieve relevant memories without the user telling it to shell
out; `.mcp.json` setup is documented and verified end-to-end.

## P2 - Provision the embedder model on `localmem init`
Repro: `localmem init --home <proj>/.localmem` then `localmem write ...` warned:
```
embedder unavailable; capture indexed in lex+facts only. Run `localmem replay` after installing the model
missing model file: <proj>/.localmem/models/bge-small-en-v1.5/model.onnx
```
A new project store is semantically blind out of the box (lex+facts only, no vectors). Workaround:
symlinked `~/.localmem/models/bge-small-en-v1.5` into the project home, then `localmem replay`
backfilled vectors and hybrid search worked.

Fix: on init, share/symlink/copy the global model, or fetch it, or have writes resolve a shared model
path from config. At minimum document the symlink workaround in `localmem init` output.

Acceptance: a freshly-initialized project home produces vector-backed hybrid search on first write,
with no manual model step.

## P2 - CLI ergonomics: consistent query input and clean output
Repro: `localmem search --content "x"` errored (search wants a positional `QUERY`), while
`localmem write` uses `--content`. The inconsistency cost a round-trip.

Fix: accept both forms on both commands, or standardize. Add a `--quiet` flag, and/or guarantee that
`--json` emits only the JSON payload, so INFO logs on stderr never interleave with results. Right now
every CLI call needs `grep -v INFO` to be machine-parseable.

Acceptance: `--json` output is parseable with zero log contamination; `search`/`write` input flags are
consistent or aliased; help text documents it.

## P3 - Promotion/tiering story (docs)
localmem coexists with always-in-context CLAUDE.md and harness file-memory. Define when a fact belongs
in the hot tier (CLAUDE.md / file-memory, always loaded) versus the cold queryable tier (localmem),
and whether/how a localmem fact can be promoted. Add this to docs as guidance for agents and users.

## What worked well (keep, do not regress)
- Event log as source of truth let the agent read every memory straight from `events.jsonl` when the
  derived index was broken. Excellent resilience.
- Per-home `--home` scoping is the right primitive and made a clean project-vs-global split trivial.
- Hybrid BM25 + vector RRF recall and subject/predicate/object fact extraction were solid after repair.
