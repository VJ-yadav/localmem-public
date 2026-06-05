# localmem

> Claude Code can't remember what you told Cursor.
> Cursor can't remember what you told ChatGPT.
> Every AI chat starts from zero.

**localmem** is the memory layer that follows you across every AI tool you use. Local-first. Open format. Owned by you. MCP-native. No content ever leaves your machine.

**License:** Apache-2.0. **Status:** v0.2 — usable for daily AI work.

---

## Install

```bash
# 1. Install the Rust core binary (macOS + Linux)
curl -fsSL https://github.com/VJ-yadav/localmem-public/releases/latest/download/install.sh | sh

# 2. Wire it into your AI tool of choice
localmem mcp install --client claude          # Claude Desktop
localmem mcp install --client claude-code     # Claude Code CLI
localmem mcp install --client cursor          # Cursor
localmem mcp install --client cline           # Cline (VS Code)
localmem mcp install --client windsurf        # Windsurf
```

Restart the client. Your agent can now read and write `memory_*` tools. That's it.

**Per-project memory** instead of global? Add `--home /path/to/project/.localmem` to any command, or set `LOCALMEM_HOME` in your shell. Project homes share the global embedder model automatically.

---

## The 30-second demo (this is the moat)

Every other memory product is a database. We're an event log with caches.

```bash
# Write a memory
localmem write --kind preference --content "I prefer Rust for systems work"

# Search for it
localmem search "what language do I prefer"
# [1] I prefer Rust for systems work  score=0.412  id=01K...

# Now blow away every derived store
rm -rf ~/.localmem/derived

# Rebuild everything from the event log alone
localmem replay

# Search again — same result, fully recomputed
localmem search "what language do I prefer"
```

`~/.localmem/events.jsonl` is the **single source of truth**. DuckDB, LanceDB, Tantivy — all caches. If your DB corrupts, your memory is intact. If the company disappears, your memory still works. If a future version changes the schema, your memory replays clean. Nothing else in the category offers this.

---

## How it compares

| | localmem | Agentmemory | Memento | Supermemory / mem0 / Zep |
|---|---|---|---|---|
| Where your data lives | Your machine | Your machine | Your machine | Their cloud |
| Runtime | Single Rust binary | Node + iii framework | Node | Cloud (SaaS) |
| Plaintext leaving your box | Never | **Telemetry on by default** | Never | Always |
| `forget` is auditable | Event in the log | App-level delete | App-level delete | Trust them |
| Recoverable from a plain text file | Yes (`localmem replay`) | No | No | No |
| License | Apache-2.0 (non-relicensable) | Apache-2.0 | Apache-2.0 | Mixed |
| If the company dies | Your memory works | Your memory works | Your memory works | Your memory is gone |
| MCP tool count | 6 (narrow, auditable) | 51 (wide) | ~26 | varies |

We are deliberately the narrowest MCP surface in the category. The full power lives in the CLI and is auditable. The agent surface stays minimal.

---

## What it does

- **Captures** — `localmem write` ingests text. The write policy decides commit/dedup/skip/forget and records every decision in `journal.log`.
- **Recall** — `localmem search "query"` runs hybrid BM25 + ANN with per-kind recency decay and reciprocal-rank-fusion. `--at-time RFC3339` for bitemporal queries (what did we believe last Tuesday?).
- **Entity profiles** — `localmem recall <subject>` returns a fact-by-fact view of everything ever said about a subject. `localmem profile <subject>` synthesizes it into markdown.
- **Container tags** — every capture can carry `--tags project=X,topic=Y`. Reserved tags include `retention=ephemeral` (auto-expire) and `visibility=private` (excluded from default search).
- **Smart forgetting** — active contradiction resolution: a higher-confidence fact on the same `(subject, predicate)` retires the prior live fact and emits an `Update` event, fully auditable via `localmem audit`.
- **Closed-core kinds** — `fact`, `preference`, `decision`, `constraint`, `todo`, `note`. Profile generation groups by kind. Per-kind recency decay (preferences age slower than todos).
- **Import wizard** — `localmem import-wizard` scans `~/Downloads` for ChatGPT/Claude export ZIPs and migrates them in.
- **MCP server** — 6 tools: `memory_write`, `memory_search`, `memory_recall`, `memory_profile`, `memory_forget`, `memory_journal`. Plus Prompts (`session_context`, `summarize_tag`) and Resources (`localmem://profile`, `localmem://tags`, `localmem://recent`).

Full surface: [HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md).

---

## Architecture in one paragraph

Append-only JSONL event log is the source of truth. Derived stores (DuckDB for bitemporal facts, LanceDB for vectors via ONNX BGE-small embeddings, Tantivy for BM25) are fully recomputable from the event log. Hybrid retrieval combines vector similarity, BM25 lexical match, and per-kind recency decay via RRF. A write-policy layer decides what to commit, update, dedup, or forget, with every decision recorded in `journal.log`. The Rust core binary owns the engine; the TypeScript MCP server is a thin adapter that exposes 6 tools, 2 prompts, and 4 resources to any MCP-compatible AI tool. CLI and server are peers; both can run without the other. **`localmem replay` rebuilds every derived store from `events.jsonl` deterministically.**

Full design: [ARCHITECTURE.md](ARCHITECTURE.md). Trust boundary: [MOAT.md](MOAT.md).

---

## Daily use patterns

### Start a session with `session_context`

Any MCP-aware agent can call `prompts/get session_context` on its first turn to get a markdown brief: synthesized profile + active project tags + last 5 captures. ~200 tokens, surfaces "what does my memory know about me" without dumping the whole store into context. See [AGENT_BOOTSTRAP.md](docs/AGENT_BOOTSTRAP.md).

### Hot tier vs cold tier

- **CLAUDE.md / file-memory** = always loaded, conventions and identity facts. Hot tier.
- **localmem** = queryable on demand. Cold tier.

Decisions, project facts, time-sensitive context belong in localmem. Format rules and operating guardrails belong in CLAUDE.md. Full guidance: [MEMORY_TIERS.md](docs/MEMORY_TIERS.md).

### Per-project scoping

Drop a `.mcp.json` in any repo with `LOCALMEM_HOME` set to `<repo>/.localmem`. Personal memory stays in `~/.localmem`; project memory stays scoped to the project. No leaks, no mental tax. This single design choice (per the field-feedback agent who tested it) outweighs most of the competition's auto-capture features.

---

## What's free, forever

The core binary, MCP server, event log schema, all importers, all under Apache-2.0. Your data lives in a folder you own (`~/.localmem/` by default). The company can disappear tomorrow and your memory still works. The OSS core never requires a network call to complete a `memory_*` operation.

## What's paid (opt-in, v0.2.1+)

- E2E encrypted sync across devices (Personal Cloud)
- Cloud compute for heavy ingestion (audio, video, large PDFs)
- Team contexts
- Enterprise audit + retention

The relay stores ciphertext only. There is no path in which plaintext leaves your machine. See [MOAT.md](MOAT.md) for the full open-core boundary and what we will never put behind a paywall.

---

## Build from source

```bash
git clone https://github.com/VJ-yadav/localmem-public
cd localmem/core
cargo build --release           # needs Rust 1.83+
./target/release/localmem doctor    # confirm install
```

The MCP server is TypeScript on bun:

```bash
cd ../mcp-server
bun install && bun run build
```

---

## Docs

| Doc | What it covers |
|---|---|
| [HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md) | Full user guide: every command, every concept, with examples |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Locked technical design: event schema, derived stores, replay semantics |
| [MOAT.md](MOAT.md) | What's defensible. Why local-first wins. Open-core boundary |
| [AGENT_BOOTSTRAP.md](docs/AGENT_BOOTSTRAP.md) | How an agent should reach localmem at session start |
| [MEMORY_TIERS.md](docs/MEMORY_TIERS.md) | Hot (CLAUDE.md) vs cold (localmem). Promotion rules |
| [COMPETITORS.md](docs/COMPETITORS.md) | Point-in-time competitive snapshot |
| [INSTALL.md](docs/INSTALL.md) | Per-platform install, troubleshooting, manual setup |

---

## License

Apache-2.0 for everything in this repo. Cloud services (sync, hosted intelligence, teams) live in a separate repo under a commercial license. The core binary and MCP server are not relicensable per [MOAT.md](MOAT.md) #6.

## Built by

Vijay Yadav. Two-person team for the first year (one human, one Claude Code instance dogfooding itself).

## Contributing

Issues welcome. PRs welcome for documented bugs and tasks already filed in TASKS.md. Larger changes: open an issue first to discuss. See `docs/feedback/` for field reports — the most valuable thing you can do is install, use it for a week, and write up the friction.
