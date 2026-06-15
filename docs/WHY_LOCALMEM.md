# Why localmem?

A practical comparison: localmem vs. mem0, Memento, agentmemory, and
just using `MEMORY.md` files. We'll be specific about where each tool
fits and where localmem fits — pick whichever matches your situation.

## TL;DR

| You want | Pick |
|---|---|
| Memory that follows you across every AI tool, content never leaves your machine, you can audit every line of the code | **localmem** |
| Hosted SaaS, zero ops, OK with plaintext going to a vendor's cloud | **mem0** |
| Local-first, single SQLite file, the widest MCP surface, you don't mind passive conflict resolution | **Memento** |
| 12 auto-capture hooks, knowledge-graph retrieval, OK with telemetry-by-default and a Node + iii-framework runtime | **agentmemory** |
| The simplest possible thing, no daemon, just markdown files Claude Code reads | **`MEMORY.md`** |

If you're not sure: try localmem. It's free, the install is one
command, and `localmem export` gives you a portable archive of every
memory so you can leave any time.

## What every memory layer is trying to solve

The same problem: **your AI tools have amnesia between sessions.**
Claude Code can't remember what you told Cursor. Cursor can't remember
what you told ChatGPT. Every chat starts at zero.

The four real strategies for solving it:

1. **Local files** (the `MEMORY.md` approach). Claude Code already does
   this for a single project.
2. **Local daemon + MCP** (localmem, Memento, agentmemory). A small
   process on your machine; every AI tool connects via MCP and reads
   from the same store.
3. **Hosted SaaS** (mem0, Letta, Cognee, Supermemory). A cloud service;
   your AI tool sends each turn to the cloud and gets memories back.
4. **Vendor lock-in** (ChatGPT Memory, Claude Memory, Gemini Memory).
   Each vendor's own memory, only for their own model.

localmem is strategy #2 done with a specific opinion: **the source of
truth is a file you own, and nothing you say leaves your machine unless
you ask.**

## How localmem compares to each

### vs. mem0

**Architectural split:**

| | mem0 | localmem |
|---|---|---|
| Runtime | Hosted SaaS (api.mem0.ai) | Local Rust binary on 127.0.0.1 |
| Plaintext leaves the machine? | **Yes** — every `add()` ships to their cloud | **No** — `memory_*` calls hit localhost only |
| Auth | API key | None |
| Write semantics | Async (returns event ids, you poll for `SUCCEEDED`) | Sync (returns when committed) |
| Storage | Their managed store | `events.jsonl` on your disk, plain JSON, append-only |
| Pricing | Usage-based per user / per add | Apache-2.0, free |
| Audit trail | Not surfaced | `localmem journal` + `localmem audit <id>` |
| Recomputable from log | No | Yes (`localmem replay`) |

**When mem0 wins:** zero-ops shipped product, willing to send plaintext
to their cloud, your users want a SaaS integration they don't have to
install.

**When localmem wins:** you care about data residency, you want an
audit trail, you want to test locally without API keys, you want to
ship a memory layer to users who won't trust a cloud vendor.

### vs. Memento

Memento ([github.com/veerps57/memento](https://github.com/veerps57/memento))
is the closest direct OSS competitor. Honest comparison:

| | Memento | localmem |
|---|---|---|
| Runtime | Node.js (better-sqlite3) | Single static Rust binary |
| Storage | One SQLite file (FTS5 + brute-force cosine over embedding blobs) | events.jsonl (truth) + Tantivy + LanceDB + DuckDB (derived, recomputable) |
| Bitemporal facts | No | Yes (valid-time + transaction-time + temporal foundation) |
| Audit log | Informational | The event log IS the canonical store |
| Conflict resolution | **Passive** — user must call `resolve_conflict`. Docs: *"Memento never decides which side of a conflict is right."* | **Active** — high-confidence new fact retires the old one automatically, logged in the journal |
| MCP surface | Wide (~26 tools) | Narrow (6 tools + 4 resources + 2 prompts) |
| Kinds | 5 (fact, preference, decision, todo, snippet) | 6 (fact, preference, decision, constraint, todo, note) + extension |
| Scope hierarchy | 5 explicit scopes incl. `branch(remote, branch)` | Container tags (more flexible; less semantic) |
| Per-kind decay | Yes (fact 90d, pref 180d, decision 365d, todo 14d) | Yes (same numbers) |
| MMR re-rank | Yes | Yes + cross-encoder rerank (Memento doesn't have) |
| Web dashboard | Yes (Hono + React, token-gated) | Yes (multi-tab viewer at :8088) |
| Curated YAML packs | Yes | Planned |
| Auto-capture hooks | No | No |

**Where Memento is ahead:** scope hierarchy (per-git-branch memory is
real), curated packs (good onboarding), longer time in market.

**Where localmem is ahead:** bitemporal substrate (you can query "what
did I know on date X"), active contradiction resolution (no manual
triage), single static binary (no Node + C++ toolchain), event log as
source of truth (rebuild any derived store from the log).

**When Memento wins:** wide MCP surface, simpler SQLite model, you
prefer Node.js + npm install.

**When localmem wins:** you want to query memory at past instants,
you don't want the toolchain dependency, you want active conflict
resolution instead of manual triage.

### vs. agentmemory

Agentmemory ([github.com/rohitg00/agentmemory](https://github.com/rohitg00/agentmemory))
is the dominant local-first MCP memory project by stars (10.5K as of
mid-2026).

| | agentmemory | localmem |
|---|---|---|
| Telemetry | **Opt-out, on by default.** `telemetry.project_name = "agentmemory"` is pinned in releases. | **Zero.** No telemetry of any kind. Project promise. |
| Runtime | Node.js on top of the `iii` framework (iii.dev distributed runtime) | Single static Rust binary, no framework dependency |
| Auto-capture | 12 hooks across Claude Code's hook surface (we don't have these yet) | None (manual `memory_write` only) |
| Retrieval | BM25 + vector + knowledge graph (triple-stream) | BM25 + vector + facts + recency + per-kind decay + MMR + cross-encoder rerank |
| MCP surface | Claims 51 tools + 121 REST endpoints (REST proxy inflates the count) | 6 tools + 4 resources + 2 prompts |
| License | Apache-2.0 | Apache-2.0 (Community) |
| Stars | 10.5K | <100 (early days) |

**Where agentmemory is ahead today:** 12 auto-capture hooks, knowledge
graph retrieval surface, massive community.

**Where localmem is ahead:** zero telemetry (audit our code, we
genuinely don't phone home), single static binary, no framework
dependency, bitemporal facts.

**When agentmemory wins:** you want the auto-capture hooks today and
don't mind the telemetry opt-out flag.

**When localmem wins:** you take "your data, locally, forever" as
non-negotiable (privacy + audit are structural, not a setting).

### vs. just `MEMORY.md` files

This is what Claude Code does out of the box. It's not nothing.

| | MEMORY.md | localmem |
|---|---|---|
| Scope | One project | Cross-project, cross-tool |
| Cross-tool | No (Claude Code only) | Yes (Claude Desktop / Code / Cursor / Windsurf / Cline) |
| Search | grep over text | BM25 + vector + facts |
| Conflict resolution | None (you edit the file) | Active (smart forgetting) |
| Audit trail | Git history (if committed) | Journal + bitemporal replay |
| Setup | Zero — just write a markdown file | One command (`curl ... \| sh`) |

**When MEMORY.md wins:** single-project, single-developer, single AI
tool. The simplest possible thing.

**When localmem wins:** you use more than one AI tool, you want
memories that compose across projects, you want search beyond grep.

## What's structurally different about localmem

A few choices baked into the architecture, not bolt-on features:

1. **The event log is the source of truth.** `events.jsonl` is plain
   JSON, append-only, one event per line. Cat it, parse it, migrate it
   elsewhere. Every derived store (Tantivy, LanceDB, DuckDB) is
   recomputable from it via `localmem replay`. This means a 5-year-old
   `events.jsonl` will still be readable by a future localmem binary,
   and you're never locked in.
2. **Bitemporal facts.** Every fact has `valid_from` (when it became
   true in reality) and `recorded_at` (when we wrote it down). You can
   query "what did I know about X on date Y" — useful for audit and for
   training-data hygiene. No competitor we know of has this.
3. **Active contradiction resolution.** When a new high-confidence
   fact contradicts an old one, the old one is automatically retired
   (with the supersession logged in the journal). Most competitors
   either ignore conflicts or make you triage them manually.
4. **Zero content telemetry.** Hard rule, audited in code. We don't
   phone home with what's in your memory. Ever. The project promise is
   in `CLAUDE.md` and any PR that violates it gets
   rejected.
5. **Single Rust binary.** No Node, no Python, no toolchain. Copy the
   binary, run it, done. Survives a `cargo install` failing because
   it's a single artifact.

## When you shouldn't pick localmem

To stay honest:

- **You need hosted, zero-ops, sales-friendly memory and you're OK
  with cloud plaintext.** Use mem0 or Letta. localmem Cloud is
  on our roadmap but not yet shipped.
- **You need a 10K+ star community and a marketplace of plugins
  today.** Use agentmemory. localmem is early.
- **You don't run any AI tool that supports MCP.** localmem's
  surface is MCP-first. We have a CLI for everything, but the value
  multiplies via MCP.
- **You're shipping memory to enterprise customers with SSO + audit
  + compliance requirements.** Talk to us about
  [localmem.org/enterprise](https://localmem.org). The Community
  Edition alone doesn't have multi-user identity; the Enterprise
  Edition does.

## When you should pick localmem

- You use more than one AI tool and they don't share memory today.
- You care that the code touching your memory is auditable.
- You want a real audit trail of what your AI tools learned about you.
- You want to be able to leave any vendor, including us, with one
  `localmem export`.
- You want memory that's a file, not a service.

## What to do next

```bash
curl -fsSL https://localmem.org/install | sh
localmem init
localmem mcp install --client claude    # or claude-code, cursor, windsurf, cline
```

That's the 60-second test. If you don't like it, `rm -rf ~/.localmem`
and walk away. We won't know you tried it.
