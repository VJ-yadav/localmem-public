<p align="center">
  <strong>
    Memory that follows you across every AI tool. Local-first. Open format. Yours.
  </strong><br/>
  Single static Rust binary. Apache-2.0. MCP-native. <strong>No content ever leaves your machine.</strong>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/localmem-mcp"><img src="https://img.shields.io/npm/v/localmem-mcp?color=CB3837&label=npm&style=for-the-badge&logo=npm" alt="npm version" /></a>
  <a href="https://github.com/VJ-yadav/localmem-community/releases/latest"><img src="https://img.shields.io/github/v/release/VJ-yadav/localmem-community?label=release&style=for-the-badge&logo=github" alt="GitHub release" /></a>
  <a href="https://github.com/VJ-yadav/localmem-community/blob/main/LICENSE"><img src="https://img.shields.io/github/license/VJ-yadav/localmem-community?color=blue&style=for-the-badge" alt="License" /></a>
  <a href="https://github.com/VJ-yadav/localmem-community/stargazers"><img src="https://img.shields.io/github/stars/VJ-yadav/localmem-community?style=for-the-badge&color=yellow&logo=github" alt="Stars" /></a>
  <a href="https://modelcontextprotocol.io/"><img src="https://img.shields.io/badge/MCP-native-purple?style=for-the-badge" alt="MCP-native" /></a>
</p>

<p align="center">
  <img alt="event log = source of truth" src="https://img.shields.io/badge/event_log-source_of_truth-0a0a0a?style=for-the-badge" />
  <img alt="zero content telemetry" src="https://img.shields.io/badge/content_telemetry-zero-2ea043?style=for-the-badge" />
  <img alt="bitemporal facts" src="https://img.shields.io/badge/facts-bitemporal-1f6feb?style=for-the-badge" />
  <img alt="single Rust binary" src="https://img.shields.io/badge/runtime-single_Rust_binary-orange?style=for-the-badge&logo=rust" />
  <img alt="MCP tools" src="https://img.shields.io/badge/MCP-6_tools_(narrow_+_auditable)-purple?style=for-the-badge" />
</p>

<p align="center">
  <a href="#install">Install</a> &bull;
  <a href="#the-30-second-demo">Demo</a> &bull;
  <a href="#what-it-does">What it does</a> &bull;
  <a href="#how-it-compares">How it compares</a> &bull;
  <a href="#works-with-every-mcp-aware-agent">Agents</a> &bull;
  <a href="#docs">Docs</a> &bull;
  <a href="https://github.com/VJ-yadav/localmem-community/discussions">Discussions</a>
</p>

---

> Claude Code can't remember what you told Cursor.
> Cursor can't remember what you told ChatGPT.
> Every AI chat starts from zero.

**localmem** is the memory layer that follows you across every AI tool you use. **Status:** v0.2 — usable for daily AI work.

---

## Install

```bash
# 1. Install the Rust core binary
curl -fsSL https://localmem.org/install | sh

# 2. Initialize + fetch the embedding model (~44 MB)
localmem init && localmem fetch-model

# 3. Start the local HTTP daemon (leave running; MCP clients talk to it)
localmem serve &

# 4. Wire it into the AI tools you use
localmem mcp install --client claude          # Claude Desktop
localmem mcp install --client claude-code     # Claude Code CLI
localmem mcp install --client cursor          # Cursor
localmem mcp install --client cline           # Cline (VS Code)
localmem mcp install --client windsurf        # Windsurf
```

Restart the AI client. The agent can now read and write `memory_*` tools. **That's it.**

**Or via npx** (no Rust binary needed, MCP shim only — useful if a teammate already has the core running):

```bash
npx -y localmem-mcp install --client claude
```

**Have two agents that keep losing context and need re-instruction?** See [docs/SHARED_MEMORY_FOR_AGENTS.md](docs/SHARED_MEMORY_FOR_AGENTS.md) — the 60-second walkthrough that wires multiple agents into one shared memory store.

---

## The 30-second demo

Every other memory product is a database. We're an **event log with caches.**

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

# Search again — same result, fully recomputed from events.jsonl
localmem search "what language do I prefer"
```

`~/.localmem/events.jsonl` is the **single source of truth**. DuckDB, LanceDB, Tantivy — all recomputable caches. If your DB corrupts, your memory is intact. If this project disappears, your memory still works. If a future version changes the schema, your memory replays clean.

**Nothing else in the category offers this.**

---

## What it does

- **Captures** — `localmem write --kind preference --content "..."` ingests text. The write policy decides commit / dedup / skip / forget and records every decision in `journal.log`.
- **Recall** — `localmem search "query"` runs hybrid BM25 + ANN with per-kind recency decay and reciprocal-rank fusion. `--at-time RFC3339` for bitemporal queries ("what did we believe last Tuesday?").
- **Entity profiles** — `localmem recall <subject>` returns a fact-by-fact view of a subject. `localmem profile <subject>` synthesizes it as markdown.
- **Container tags** — every capture can carry `--tags project=X,topic=Y`. Reserved tags include `retention=ephemeral` (auto-expire) and `visibility=private`.
- **Smart forgetting** — active contradiction resolution: a higher-confidence fact on the same `(subject, predicate)` retires the prior live fact and emits an `Update` event, fully auditable via `localmem audit`.
- **Closed-core kinds** — `fact`, `preference`, `decision`, `constraint`, `todo`, `note`. Per-kind recency decay (preferences age slower than todos).
- **Import wizard** — `localmem import-wizard` scans `~/Downloads` for ChatGPT / Claude export ZIPs and migrates them in.
- **MCP server** — 6 tools (`memory_write`, `memory_search`, `memory_recall`, `memory_profile`, `memory_forget`, `memory_journal`), 2 prompts (`session_context`, `summarize_tag`), 4 resources (`localmem://profile`, `localmem://subjects`, `localmem://tags`, `localmem://recent`).

Full surface: [docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md).

---

## How it compares

|  | localmem | Local-first peers | Cloud SaaS (Supermemory / mem0 / Zep) |
|---|---|---|---|
| Where your data lives | Your machine | Your machine | Their cloud |
| Plaintext leaving your machine | **Never** | Varies (some have on-by-default telemetry) | Always |
| `forget` is auditable | **Event in the log** | App-level delete | "Trust the vendor" |
| Recoverable from a plain-text file | **Yes (`localmem replay`)** | No | No |
| Runtime | **Single static Rust binary** | Node + framework deps | Cloud SaaS |
| MCP tool count | **6** (narrow, auditable) | 25–50+ (wide) | varies |
| License | Apache-2.0 (non-relicensable) | Mostly Apache-2.0 | Mixed |
| If the project dies | **Your memory works** | Your memory works | Your memory is gone |

We are deliberately the narrowest MCP surface in the category. The full power lives in the CLI and is auditable. The agent surface stays minimal.

---

## Works with every MCP-aware agent

| Wired via `localmem mcp install --client <name>` | Generic MCP config |
|---|---|
| Claude Desktop, Claude Code, Cursor, Cline, Windsurf | Continue, Zed, Codex, OpenCode, Aider, custom MCP clients — see [docs/HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md#9-per-project-memory) for the generic recipe |

After install, restart the AI client. Ask the agent "do you have memory tools?" — it should mention `memory_write`, `memory_search`, `memory_recall`, `memory_profile`, `memory_forget`, `memory_journal`.

---

## Architecture in one paragraph

Append-only JSONL event log is the source of truth. Derived stores (DuckDB for bitemporal facts, LanceDB for vectors via ONNX BGE-small embeddings, Tantivy for BM25) are fully recomputable from the event log. Hybrid retrieval combines vector similarity, BM25 lexical match, and per-kind recency decay via RRF. A write-policy layer decides what to commit, update, dedup, or forget, with every decision recorded in `journal.log`. The Rust core binary owns the engine; the TypeScript MCP server is a thin adapter that exposes 6 tools, 2 prompts, and 4 resources to any MCP-compatible AI tool. CLI and server are peers; both can run without the other. **`localmem replay` rebuilds every derived store from `events.jsonl` deterministically.**

Full design: [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Build from source

The released binary covers macOS arm64 (Apple Silicon). For Intel Mac, Linux, or anyone who'd rather build it themselves:

```bash
git clone https://github.com/VJ-yadav/localmem-community
cd localmem-community/core
cargo build --release           # needs Rust 1.83+; ~5–10 min on first build
./target/release/localmem doctor
```

Cross-compiled binaries for Intel Mac + Linux ship in a follow-up release.

---

## Docs

**Start here:**

| Doc | What it covers |
|---|---|
| [QUICKSTART.md](docs/QUICKSTART.md) | 5 minutes to first capture. Install → wire up one MCP client → confirm the round-trip. |
| [WHY_LOCALMEM.md](docs/WHY_LOCALMEM.md) | Honest comparison vs. mem0, Memento, agentmemory, and just MEMORY.md files. Pick the right tool for your case. |
| [INSTALL_PER_CLIENT.md](docs/INSTALL_PER_CLIENT.md) | Deep-dive install for each MCP client: Claude Desktop / Code / Cursor / Windsurf / Cline. Includes troubleshooting per client. |
| [MIGRATING.md](docs/MIGRATING.md) | Import from ChatGPT, Claude, MEMORY.md files, and other memory tools |
| [FOR_TEAMS.md](docs/FOR_TEAMS.md) | Using localmem at startups and small-to-mid-size companies. When to graduate to the Enterprise Edition. |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to land a PR. Conventions, setup, review process. |

**Reference:**

| Doc | What it covers |
|---|---|
| [HOW_IT_WORKS.md](docs/HOW_IT_WORKS.md) | Full user guide: every command, every concept, with examples |
| [SHARED_MEMORY_FOR_AGENTS.md](docs/SHARED_MEMORY_FOR_AGENTS.md) | Multi-agent shared memory: the "stop re-explaining" walkthrough |
| [INSTALL.md](docs/INSTALL.md) | Per-platform install, build-from-source, air-gapped deployment |
| [AGENT_BOOTSTRAP.md](docs/AGENT_BOOTSTRAP.md) | How an agent should reach localmem at session start |
| [MEMORY_TIERS.md](docs/MEMORY_TIERS.md) | Hot tier (CLAUDE.md) vs. cold tier (localmem). Promotion rules. |
| [CLAUDE_DESKTOP_SETUP.md](docs/CLAUDE_DESKTOP_SETUP.md) | Wire localmem into Claude Desktop step by step |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Locked technical design: event schema, derived stores, replay semantics |
| [EDITIONS.md](EDITIONS.md) | The Community / Enterprise / Cloud three-tier model + what's in each + when to pay |
| [STORY.md](STORY.md) | Why localmem exists |
| [docs/feedback/](docs/feedback/) | Field reports from agents that have used localmem in real work |

### Local web dashboard

A read-only visual browser for the memory in your `~/.localmem/`. Subjects, tags, recent captures, search, profile — all in your browser, no cloud. MVP today, served by the Rust core itself in v0.2.2. See [`dashboard/README.md`](dashboard/README.md).

### Printable summary

- [`landing/marketing/one-pager.html`](landing/marketing/one-pager.html) — 1-page Letter summary you can share as a PDF or print as a reference. Open it in any browser and Cmd-P → Save as PDF. ([live](https://localmem.org/marketing/one-pager.html))

---

## What's free, forever

The core binary, MCP server, event log schema, all importers — all under Apache-2.0. Your data lives in a folder you own (`~/.localmem/` by default). The OSS core never requires a network call to complete a `memory_*` operation. **Zero content telemetry, ever.**

## What's paid (opt-in, future releases)

E2E encrypted sync across devices, cloud compute for heavy multimodal ingestion (audio, video, large PDFs), team contexts, enterprise audit + retention. The relay stores ciphertext only — there is no code path in which plaintext leaves your machine.

The local-first OSS experience is the complete experience. Paid features are always optional.

---

## Get help / give feedback

- **[GitHub Discussions](https://github.com/VJ-yadav/localmem-community/discussions)** — questions, show-and-tell, ideas
- **[GitHub Issues](https://github.com/VJ-yadav/localmem-community/issues)** — bugs and feature requests
- **Field reports in [`docs/feedback/`](docs/feedback/)** — if you use localmem for a week, write up the friction. That is the highest-value contribution you can make right now.

---

## License

**This repo (Community Edition) is Apache-2.0 forever.** Every feature
shipped here — the Rust core, the MCP server, the hybrid retriever,
smart forgetting, the dashboard, every importer, every extractor —
stays Apache-2.0. We do not reverse-degrade.

localmem ships as three distinct products:

| Edition | Repo | License | What you get |
|---|---|---|---|
| **Community** (you are here) | `localmem-community` | **Apache-2.0** | Feature-complete for individual developers. Free forever. |
| **Enterprise** | `localmem-enterprise` (private) | Closed proprietary, annual contract | Multi-tenancy, SSO, RBAC, audit export, BYOK encryption, SIEM integration, compliance certifications (SOC 2, ISO 27001, HIPAA). |
| **Cloud** | `localmem-cloud` (private) | Closed proprietary SaaS | Enterprise Edition + hosting + multi-region + 99.9% SLA + automated backups + mobile app. |

The Community Edition is **complete on its own** for solo developers
and internal-tool teams. The paid tiers exist for organizations that
need multi-user identity, compliance, or managed hosting — they wrap
the Community core, they don't replace it. See [EDITIONS.md](EDITIONS.md)
for the full three-tier model, what's in each edition, and our hard
rules (the load-bearing one: anything that ships Apache-2.0 stays
Apache-2.0 forever).

Enterprise + Cloud inquiries: [localmem.org](https://localmem.org).

## Built by

Vijay Yadav. One human plus one Claude Code instance dogfooding itself as the first user — localmem is the memory layer for the agent that helped build it.

**Web:** [localmem.org](https://localmem.org) &middot; **npm:** [`localmem-mcp`](https://www.npmjs.com/package/localmem-mcp) &middot; **Repo:** [github.com/VJ-yadav/localmem-community](https://github.com/VJ-yadav/localmem-community)
