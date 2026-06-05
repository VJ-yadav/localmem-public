# localmem

[![License: Apache 2.0](https://img.shields.io/badge/license-Apache_2.0-blue.svg)](LICENSE)
[![GitHub release](https://img.shields.io/github/v/release/VJ-yadav/localmem-public)](https://github.com/VJ-yadav/localmem-public/releases/latest)
[![npm version](https://img.shields.io/npm/v/localmem-mcp.svg)](https://www.npmjs.com/package/localmem-mcp)
[![MCP-native](https://img.shields.io/badge/MCP-native-purple.svg)](https://modelcontextprotocol.io/)

> Claude Code can't remember what you told Cursor.
> Cursor can't remember what you told ChatGPT.
> Every AI chat starts from zero.

**localmem** is the memory layer that follows you across every AI tool you use. Local-first. Open format. Owned by you. MCP-native. **No content ever leaves your machine.**

**Status:** v0.2 — usable for daily AI work. Apache-2.0 forever.

---

## Install

```bash
# 1. Install the Rust core binary (macOS arm64; Intel/Linux build from source — see below)
curl -fsSL https://github.com/VJ-yadav/localmem-public/releases/latest/download/install.sh | sh
localmem init && localmem fetch-model

# 2. Start the local HTTP daemon (leave running; MCP clients talk to it)
localmem serve &

# 3. Wire it into the AI tools you use
localmem mcp install --client claude          # Claude Desktop
localmem mcp install --client claude-code     # Claude Code CLI
localmem mcp install --client cursor          # Cursor
localmem mcp install --client cline           # Cline (VS Code)
localmem mcp install --client windsurf        # Windsurf
```

Restart the client. Your agent can now read and write `memory_*` tools. **That's it.**

**Have two agents that keep losing context and need re-instruction?** Read [SHARED_MEMORY_FOR_AGENTS.md](docs/SHARED_MEMORY_FOR_AGENTS.md) — the 60-second walkthrough that wires multiple agents into one shared memory store.

**Want per-project memory** instead of global? Add `--home /path/to/project/.localmem` to any command, or set `LOCALMEM_HOME` in your shell. Project homes share the global embedder model automatically — no re-download.

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

`~/.localmem/events.jsonl` is the **single source of truth**. DuckDB, LanceDB, Tantivy — all caches. If your DB corrupts, your memory is intact. If this project disappears, your memory still works. If a future version changes the schema, your memory replays clean. Nothing else in the category offers this.

---

## How it compares

| | localmem | Other local-first peers | Cloud SaaS (Supermemory / mem0 / Zep) |
|---|---|---|---|
| Where your data lives | Your machine | Your machine | Their cloud |
| Plaintext leaving your machine | **Never** | Varies (some have on-by-default telemetry) | Always |
| `forget` is auditable | **Event in the log** | App-level delete | "Trust the vendor" |
| Recoverable from a plain-text file | **Yes (`localmem replay`)** | No | No |
| Runtime | Single Rust binary | Node + framework deps | Cloud SaaS |
| MCP tool count | **6** (narrow, auditable) | 25–50+ (wide) | varies |
| License | Apache-2.0 (non-relicensable) | Mostly Apache-2.0 | Mixed |
| If the project dies | **Your memory works** | Your memory works | Your memory is gone |

We are deliberately the narrowest MCP surface in the category. The full power lives in the CLI and is auditable. The agent surface stays minimal.

---

## What it does

- **Captures** — `localmem write` ingests text. The write policy decides commit / dedup / skip / forget and records every decision in `journal.log`.
- **Recall** — `localmem search "query"` runs hybrid BM25 + ANN with per-kind recency decay and reciprocal-rank fusion. `--at-time RFC3339` for bitemporal queries (what did we believe last Tuesday?).
- **Entity profiles** — `localmem recall <subject>` returns a fact-by-fact view of everything ever said about a subject. `localmem profile <subject>` synthesizes it into markdown.
- **Container tags** — every capture can carry `--tags project=X,topic=Y`. Reserved tags include `retention=ephemeral` (auto-expire) and `visibility=private` (excluded from default search).
- **Smart forgetting** — active contradiction resolution: a higher-confidence fact on the same `(subject, predicate)` retires the prior live fact and emits an `Update` event, fully auditable via `localmem audit`.
- **Closed-core kinds** — `fact`, `preference`, `decision`, `constraint`, `todo`, `note`. Profile generation groups by kind. Per-kind recency decay (preferences age slower than todos).
- **Import wizard** — `localmem import-wizard` scans `~/Downloads` for ChatGPT / Claude export ZIPs and migrates them in.
- **MCP server** — 6 tools (`memory_write`, `memory_search`, `memory_recall`, `memory_profile`, `memory_forget`, `memory_journal`), 2 prompts (`session_context`, `summarize_tag`), 4 resources (`localmem://profile`, `localmem://subjects`, `localmem://tags`, `localmem://recent`).

Full surface: [HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md).

---

## Architecture in one paragraph

Append-only JSONL event log is the source of truth. Derived stores (DuckDB for bitemporal facts, LanceDB for vectors via ONNX BGE-small embeddings, Tantivy for BM25) are fully recomputable from the event log. Hybrid retrieval combines vector similarity, BM25 lexical match, and per-kind recency decay via RRF. A write-policy layer decides what to commit, update, dedup, or forget, with every decision recorded in `journal.log`. The Rust core binary owns the engine; the TypeScript MCP server is a thin adapter that exposes 6 tools, 2 prompts, and 4 resources to any MCP-compatible AI tool. CLI and server are peers; both can run without the other. **`localmem replay` rebuilds every derived store from `events.jsonl` deterministically.**

Full design: [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Daily use patterns

### Multi-agent shared memory (the most-asked use case)

Two agents that keep losing context and need re-instruction every session? Install once, wire both via MCP, anything one agent learns is immediately available to the other. Step-by-step in [SHARED_MEMORY_FOR_AGENTS.md](docs/SHARED_MEMORY_FOR_AGENTS.md).

### Start a session with `session_context`

Any MCP-aware agent can call `prompts/get session_context` on its first turn to get a markdown brief: synthesized profile + active project tags + last 5 captures. ~200 tokens, surfaces "what does my memory know about me" without dumping the whole store into context. See [AGENT_BOOTSTRAP.md](docs/AGENT_BOOTSTRAP.md).

### Hot tier vs cold tier

- **CLAUDE.md / file-memory** = always loaded; conventions and identity facts. Hot tier.
- **localmem** = queryable on demand. Cold tier.

Decisions, project facts, time-sensitive context belong in localmem. Format rules and operating guardrails belong in CLAUDE.md. Full guidance: [MEMORY_TIERS.md](docs/MEMORY_TIERS.md).

### Per-project scoping

Drop a `.mcp.json` in any repo with `LOCALMEM_HOME` set to `<repo>/.localmem`. Personal memory stays in `~/.localmem`; project memory stays scoped to the project. No leaks, no mental tax. The field-feedback agent who tested it called this design choice "more valuable than most of the competition's auto-capture features."

---

## Build from source

The released binary covers macOS arm64 (Apple Silicon). For Intel Mac, Linux, or anyone who'd rather build it themselves:

```bash
git clone https://github.com/VJ-yadav/localmem-public
cd localmem-public/core
cargo build --release           # needs Rust 1.83+; ~5–10 min on first build
./target/release/localmem doctor
```

The MCP server is TypeScript on bun (the npm package handles this for you automatically):

```bash
cd ../mcp-server
bun install && bun run build
```

Cross-compiled binaries for Intel Mac + Linux ship in v0.2.1 via the CI release workflow.

---

## Docs

| Doc | What it covers |
|---|---|
| [HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md) | Full user guide: every command, every concept, with examples |
| [SHARED_MEMORY_FOR_AGENTS.md](docs/SHARED_MEMORY_FOR_AGENTS.md) | Multi-agent shared memory: the "stop re-explaining" walkthrough |
| [INSTALL.md](docs/INSTALL.md) | Per-platform install, troubleshooting, manual setup |
| [AGENT_BOOTSTRAP.md](docs/AGENT_BOOTSTRAP.md) | How an agent should reach localmem at session start |
| [MEMORY_TIERS.md](docs/MEMORY_TIERS.md) | Hot tier (CLAUDE.md) vs cold tier (localmem). Promotion rules |
| [CLAUDE_DESKTOP_SETUP.md](docs/CLAUDE_DESKTOP_SETUP.md) | Wire localmem into Claude Desktop step by step |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Locked technical design: event schema, derived stores, replay semantics |
| [STORY.md](STORY.md) | Why localmem exists |
| [docs/feedback/](docs/feedback/) | Field reports from agents that have used localmem in real work |

---

## What's free, forever

The core binary, MCP server, event log schema, all importers — all under Apache-2.0. Your data lives in a folder you own (`~/.localmem/` by default). The OSS core never requires a network call to complete a `memory_*` operation. **Zero content telemetry, ever.**

## What's paid (opt-in, future releases)

- E2E encrypted sync across devices (Personal Cloud)
- Cloud compute for heavy ingestion (audio, video, large PDFs)
- Team contexts
- Enterprise audit + retention

The relay stores ciphertext only. There is no code path in which plaintext leaves your machine. Paid features are always optional; the local-first OSS experience is the complete experience.

---

## Get help / give feedback

- **[GitHub Discussions](https://github.com/VJ-yadav/localmem-public/discussions)** — questions, show-and-tell, ideas
- **[GitHub Issues](https://github.com/VJ-yadav/localmem-public/issues)** — bugs and feature requests
- **Field reports in [`docs/feedback/`](docs/feedback/)** — if you use localmem for a week, write up the friction; that's the highest-value contribution you can make right now

---

## License

Apache-2.0 for everything in this repo. Cloud services (sync, hosted intelligence, teams) ship separately under a commercial license in a future release. The core binary and MCP server are committed to remain Apache-2.0 forever.

## Built by

Vijay Yadav. One human plus one Claude Code instance dogfooding itself as the first user — localmem is the memory layer for the agent that helped build it.

## Contributing

Issues and pull requests are welcome. For larger changes, please open an issue first so we can discuss the approach before you spend time on it.

The most valuable contribution you can make right now: install localmem, use it daily for a week, and add a `YYYY-MM-DD-your-name-field-notes.md` to [`docs/feedback/`](docs/feedback/) with what worked, what broke, and what felt wrong. Real usage drives the roadmap.
