# Agent Instructions — localmem

## Git Commit Attribution

When creating commits in this repo, use the current git user identity. Run
`git config user.name` and `git config user.email` to get the values.

**DO NOT** add Claude / Anthropic attribution. This is Vijay Yadav's product.
A `Co-Authored-By` trailer with the same identity as the author is redundant;
skip it unless a different human collaborator actually contributed.

## Project promise (non-negotiable)

These promises are the trust moat. Every change in this repo must respect them.
See `MOAT.md` for the rationale.

1. **The event log is the source of truth.** Never mutate `events.jsonl`. To
   "delete," emit a `forget` event.
2. **All derived stores are recomputable.** If `localmem replay` cannot rebuild
   a file from events.jsonl, that file does not belong in `derived/`.
3. **Schemas are versioned, never broken.** Migrations are forward functions
   applied at read time. A 5-year-old `events.jsonl` must remain readable.
4. **Local-first by default.** No code path may require a network call to
   complete a `memory_*` operation. Cloud is opt-in, always.
5. **No plaintext leaves the machine.** Even the future paid sync stores
   ciphertext only. There is no "telemetry includes content" path.
6. **Apache-2.0 forever.** The core binary and MCP server are not relicensable.

## Architectural conventions

- **No bandaids. Ever.** Fix root causes, not symptoms. If a workaround
  feels like routing around something that should just work, stop and fix
  the underlying thing. Examples of bandaids we will NOT accept:
  routing reads through HTTP because a store wrapper acquires unneeded
  write locks; adding retry loops to mask race conditions; wrapping
  panics in `Result` instead of removing the panic site; pinning a
  dependency to dodge a bug instead of upstreaming the fix. Each
  bandaid compounds; each root-cause fix compounds the other way.
- **MCP is the only public interface.** No "alternative" SDK, no REST surface
  exposed to non-localmem clients. The core HTTP server is private to the
  MCP server.
- **Errors:** Use `anyhow` for application errors, `thiserror` for library
  errors. No `String` errors. No `panic!` in non-test code.
- **Logging:** `tracing` macros. Structured fields, not interpolated strings.
  JSON output in production, pretty in dev.
- **No singletons for mutable state.** Use Arc + RwLock or a resource pool.
  Learned the hard way from rehearse.
- **No hardcoded enums or string constants.** If it looks like config, it
  belongs in `policies/*.yaml` or `config.toml`. Grep before adding.
- **CLI and server must be peers.** Either can run without the other.
  Neither can force the user into the other's lifecycle. They share the
  filesystem (events.jsonl, derived stores) using each store's native
  concurrency model: Tantivy = one writer + many readers, DuckDB =
  independent connections, LanceDB = native concurrent reads, journal
  + event log = append-only files. If a store wrapper breaks this
  model, fix the wrapper.

## Documentation style

- **No em dashes.** Use commas, periods, or restructure the sentence.
- **No emojis** unless explicitly asked.
- **No "WHAT the code does" comments.** Code is self-documenting. Comments
  explain non-obvious WHY: invariants, workarounds, hidden constraints.
- Don't create planning, decision, or analysis docs unless asked. Architectural
  changes go in `ARCHITECTURE.md`, never in scattered `.md` files.

## Key files

| Purpose | File |
|---|---|
| Locked design | `ARCHITECTURE.md` |
| Open-core boundary | `MOAT.md` |
| 90-day execution plan | `ROADMAP.md` |
| Learnings carried from rehearse + StudentSucceed | `docs/LEARNINGS.md` |
| Rust core binary | `core/` |
| TypeScript MCP server | `mcp-server/` |

## Verification checklist (before committing)

1. Any new code path: does it require a network call? If yes, the user must
   be able to disable it.
2. Any new event kind: documented in `ARCHITECTURE.md` event-log section?
3. Any new derived store: is it rebuildable by `localmem replay`?
4. Any new MCP tool: schema in `mcp-server/src/tools/*.ts`?
5. Any new dependency: license-compatible with Apache-2.0?

## What this project is NOT

- Not a notes app. Not a "second brain." Not a knowledge base.
- Not a wrapper around Supermemory / mem0 / Letta / Zep. They are competitors,
  not dependencies.
- Not multi-tenant. The single-user-on-their-machine assumption is load-bearing.
- Not a vector database. We use LanceDB as one of three retrieval primitives.
