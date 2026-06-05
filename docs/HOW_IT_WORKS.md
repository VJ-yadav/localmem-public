# How localmem works

A complete user-facing guide to localmem v0.2. What it does, why it works the
way it does, every command, every concept, every workflow.

**Audience:** anyone installing localmem for the first time, plus existing
users wanting a single reference. Reads top-to-bottom in ~20 minutes. Skip to
"Commands" if you just want the CLI surface.

---

## Table of contents

1. [The problem localmem solves](#1-the-problem-localmem-solves)
2. [The big idea](#2-the-big-idea)
3. [Install](#3-install)
4. [First-run walkthrough](#4-first-run-walkthrough)
5. [Concepts](#5-concepts)
6. [Commands](#6-commands)
7. [The MCP surface](#7-the-mcp-surface)
8. [Agent usage patterns](#8-agent-usage-patterns)
9. [Per-project memory](#9-per-project-memory)
10. [Trust, recovery, and the moat](#10-trust-recovery-and-the-moat)
11. [How it compares](#11-how-it-compares)
12. [Troubleshooting](#12-troubleshooting)
13. [What's free vs paid](#13-whats-free-vs-paid)
14. [Roadmap and contributing](#14-roadmap-and-contributing)

---

## 1. The problem localmem solves

Every AI tool ships with its own siloed memory. ChatGPT forgets what you told
Claude. Cursor forgets what you discussed in ChatGPT. Even within one tool,
memory is project-scoped or session-scoped, with no semantic search, no
temporal model, no audit trail.

The cloud memory layer (Supermemory, mem0, Zep, Letta) exists and is
well-funded, but it holds your data in their cloud, in their format, behind
their pricing power. **The local-first option has not been built. localmem is
that option.**

### Concrete pains localmem addresses

| Pain | Today | With localmem |
|---|---|---|
| Repeating preferences to every new agent | "I prefer Rust for systems work" said 50 times | Stated once, surfaced in every session via `memory_recall` |
| Losing the rationale behind past decisions | "Why did we pick DuckDB again?" — search Slack | `localmem search "why duckdb"` returns the captured decision |
| Per-project facts polluting personal memory | One ChatGPT memory store, mixed signal | `--home <project>/.localmem` per repo |
| Audit trail when memory is wrong | "ChatGPT thinks I work at X but I quit" — no way to fix cleanly | `localmem forget <fact-id>` emits a forget event; the prior fact stays in the log |
| Time-aware queries | "What did we believe about feature X last quarter?" — impossible | `localmem search "feature X" --at-time 2026-01-15T00:00:00Z` |

---

## 2. The big idea

localmem is built around one architectural commitment that everything else
flows from:

> **The event log is the source of truth. Every derived store is recomputable.**

Concretely: `~/.localmem/events.jsonl` is an append-only file. Every memory
operation (write, fact extraction, forget, supersede) is an event in that
file. Everything else — the DuckDB facts table, the LanceDB vectors, the
Tantivy lexical index — is a cache rebuilt from the events.

This sounds simple. The implications are not:

- **`localmem replay` rebuilds every store from `events.jsonl` deterministically.** Delete `~/.localmem/derived/` and run replay; the system reconstitutes identically.
- **Schema changes don't break old data.** A v0.5 binary reading a v0.1 events.jsonl applies forward migrations at read time.
- **"Delete" is an event, not a mutation.** When you forget something, a `forget` event lands in the log. The history is intact and replayable.
- **Audit is free.** `localmem audit <fact-id>` walks the event log to show every event, journal entry, and follow-up that touched that fact.
- **Backups are trivial.** Copy `events.jsonl` to a backup location. That's the whole backup. Derived stores rebuild on first use.

No other memory product in the category offers this. Their database is the
source of truth. If it corrupts, the memory is gone.

### Three derived stores, three retrieval strategies

| Store | What it does | Used for |
|---|---|---|
| **DuckDB** (`derived/facts.duckdb`) | Bitemporal fact rows: `(subject, predicate, object, valid_from, valid_to, retired_at, confidence)` | Entity recall, profile generation, bitemporal queries |
| **LanceDB** (`derived/vectors.lance/`) | 384-dim BGE-small-en vector embeddings | Semantic search (ANN) |
| **Tantivy** (`derived/lexical.tantivy/`) | BM25 inverted index over capture content | Exact-term lexical search |

Hybrid retrieval combines lex + vector via reciprocal-rank-fusion with
per-kind recency decay. You can also request a single mode (`--mode lex`,
`--mode vec`) for diagnostic or special cases.

---

## 3. Install

### macOS + Linux (one-line)

```bash
curl -fsSL https://github.com/VJ-yadav/localmem-public/releases/latest/download/install.sh | sh
```

This script:
1. Detects your OS and architecture.
2. Downloads the matching tarball from GitHub Releases.
3. Verifies the SHA-256 against the published `SHA256SUMS`.
4. Drops the binary at `~/.local/bin/localmem`.
5. On macOS, strips the `com.apple.quarantine` xattr so first run isn't blocked by Gatekeeper.

The installer refuses to run as root. The binary lives in your `$HOME`; it
never asks for sudo.

### Build from source

If your platform isn't in the release tarballs (e.g., Windows, BSD, exotic
Linux), build from source:

```bash
git clone https://github.com/VJ-yadav/localmem-public
cd localmem/core
cargo build --release            # needs Rust 1.83+, ~5 min on M1
./target/release/localmem doctor # verify install
```

The MCP server (Node side) is optional unless you're wiring AI tools:

```bash
cd ../mcp-server
bun install && bun run build
```

### Initial setup

```bash
# Scaffold ~/.localmem/ (default home; pass --home to put it elsewhere)
localmem init

# Download the BGE-small embedding model (~44 MB) for semantic search
localmem fetch-model

# Sanity-check the install
localmem doctor
```

`doctor` reports PASS/WARN/FAIL across binary, home dir, embedder model,
server reachability, macOS Gatekeeper, and MCP wiring per client. Run it
anytime something feels off.

---

## 4. First-run walkthrough

The fastest "do I get it?" loop. Copy-paste into a terminal:

```bash
# 1. Capture three memories of different kinds
localmem write --kind preference --content "I prefer Rust for systems-level code"
localmem write --kind fact --content "localmem stores its event log at ~/.localmem/events.jsonl"
localmem write --kind decision --content "Picked DuckDB + Tantivy + LanceDB for v0.1 derived stores"

# 2. Search across all three
localmem search "what language do I prefer"
# [1] I prefer Rust for systems-level code  score=0.412  id=01K...

# 3. Entity-centric recall
localmem recall localmem

# 4. Synthesized profile
localmem profile localmem

# 5. The receipts — every memory is in events.jsonl
wc -l ~/.localmem/events.jsonl
tail -3 ~/.localmem/events.jsonl

# 6. The moat — nuke derived stores and replay rebuilds everything
rm -rf ~/.localmem/derived
localmem replay
localmem search "what language do I prefer"   # same result, fully recomputed
```

If you got through all six commands in under two minutes, you've seen the
whole product.

---

## 5. Concepts

### Captures, facts, and the event log

A **capture** is the raw text you ingested ("I prefer Rust for systems-level
code"). A **fact** is a structured tuple extracted from it (`subject=user,
predicate=prefers, object=Rust, kind=preference, confidence=0.92`).

Captures land as `capture` events. Facts land as `fact` events. Both live in
`events.jsonl`. The DuckDB facts table is materialized from `fact` events.

### Kinds (closed-core taxonomy)

Every capture has a `kind`. The closed-core kinds are:

| Kind | When to use | Default decay half-life |
|---|---|---|
| `fact` | Verifiable, source-of-truth statements | 90 days |
| `preference` | Personal taste, working style | 180 days |
| `decision` | A choice with rationale | 365 days |
| `constraint` | A hard rule (regulatory, technical) | 365 days |
| `todo` | Pending work item | 14 days |
| `note` | General memory; default kind | 30 days |

The half-life controls how recency decay influences ranking — preferences age
slower than todos because they're more stable. You can override per-kind
half-lives in `~/.localmem/config.toml` under `[retriever].decay_half_life`.

Extension kinds (anything other than the six above) round-trip cleanly but
behave as `note` for ranking.

### Container tags

Every capture can carry tags via `--tags k=v,k=v`:

```bash
localmem write --kind decision --content "Use bun, not npm, for the MCP server" \
  --tags project=localmem,topic=tooling
```

Tags scope search, recall, and profile to a subset:

```bash
localmem search "package manager" --tags project=localmem
localmem profile --tags project=localmem
```

**Reserved tag keys** have semantic effects:

| Tag | Effect |
|---|---|
| `retention=ephemeral` | The capture auto-hides after a short TTL (good for working memory) |
| `visibility=private` | Excluded from default search; needs explicit `--include-private` to surface |
| `project=<name>` | First-class project scoping (used by profile + session_context) |

### Bitemporal facts

Facts have `valid_from`, `valid_to`, and `retired_at`. This means you can ask:

```bash
# What did we believe on this date?
localmem search "user preferences" --at-time 2026-03-01T00:00:00Z
```

The retriever returns only facts that were *not yet retired* as of that
timestamp. When a higher-confidence fact supersedes an old one, the old fact
gets `retired_at = <new fact's valid_from>`; it stays in the log forever, but
the default view hides it.

### Smart forgetting

When a higher-confidence fact lands on the same `(subject, predicate)`,
localmem auto-retires the prior fact and emits an `Update` event. Confidence
threshold is 0.7; the `decision` kind opts out (decisions are append-only,
never auto-superseded).

You can see the retirement chain via `localmem audit <fact-id>`:

```
fact 01K... | created 2026-05-10 | subject=user predicate=prefers
              object="Rust"      | retired 2026-06-04 by 01K... (smart_forgetting)
fact 01K... | created 2026-06-04 | subject=user predicate=prefers
              object="Rust and Zig" | LIVE
```

### Write policy

Every capture goes through a YAML-defined write policy before commit. The
policy decides one of:

- **COMMIT** — index it everywhere (lex + vec + facts)
- **DEDUP** — refuse; an equivalent capture exists
- **SKIP** — refuse; the content matches a skip rule (e.g., too short)
- **FORGET** — refuse and emit a `forget` event for the matching prior capture

Every decision lands in `~/.localmem/derived/journal.log`. Inspect with
`localmem journal`. Override the policy by editing
`~/.localmem/policies/default.yaml` or adding `~/.localmem/policies/user.yaml`
(loaded after default).

### Context rewriting at ingest

When a capture contains ambiguous pronouns ("they prefer X"), the rewriter
substitutes the canonical subject before indexing ("Vijay prefers X"). This
makes the indexed chunk self-contained — the model can lift it out of context
during search and it still makes sense. Disabled by default for v0.2; enable
in `~/.localmem/config.toml` under `[rewriter].enabled = true`.

---

## 6. Commands

The CLI surface is intentionally small. Every command runs against
`$LOCALMEM_HOME` or `~/.localmem` (override with `--home`).

### Core operations

```bash
localmem init                              # scaffold a fresh home
localmem write --kind <k> --content "..."  # ingest a capture (or via stdin)
localmem search "<query>"                  # hybrid (default) / lex / vec
localmem recall <subject>                  # entity-centric fact view
localmem profile [--scope <subject>]       # synthesized markdown profile
localmem forget --target <id>              # soft-delete
```

### Discovery (v0.2)

```bash
localmem subjects        # list distinct entity subjects with counts
localmem tags            # list container tags in use
localmem recent          # last N captures, newest first
localmem summarize [--tags k=v]   # synthesized brief over a slice
localmem audit <id>      # trace a fact back to its source + follow-ups
```

### State + recovery

```bash
localmem journal --since 24h     # policy decision log
localmem replay                  # rebuild every derived store from events.jsonl
localmem reindex                 # re-embed all captures with the current embedder
localmem export <out.tar.gz>     # portable single-file archive
localmem import <archive.tar.gz> # round-trip the export
localmem doctor [--fix]          # per-check diagnostic
localmem todo <id> --done        # flip todo done/open via UpdateCapture event
```

### Setup helpers

```bash
localmem fetch-model                          # download BGE-small (44 MB)
localmem mcp install --client claude          # wire into Claude Desktop
localmem mcp install --client claude-code     # wire into Claude Code CLI
localmem mcp install --client cursor          # wire into Cursor
localmem mcp install --client cline           # wire into Cline
localmem mcp install --client windsurf        # wire into Windsurf
localmem mcp list                             # list configured clients
localmem mcp uninstall --client <name>        # remove the localmem entry
localmem import-wizard [--apply]              # scan ~/Downloads for AI export ZIPs
localmem serve [--addr 127.0.0.1:7788]        # run the HTTP daemon for the MCP server
```

### Global flags

| Flag | Effect |
|---|---|
| `--home <path>` | Override the home dir for this command |
| `--quiet` | Suppress all log output on stderr |
| `--json` (per-subcommand) | Emit machine-parseable JSON |

Stderr auto-suppresses when piped (non-TTY), so `localmem search "x" --json | jq .` works without `--quiet`. To force logs even when piped, set `RUST_LOG=localmem=info`.

---

## 7. The MCP surface

localmem exposes six tools, two prompts, and four resources to any
MCP-compatible AI tool.

### Tools

| Tool | Purpose | Read or write |
|---|---|---|
| `memory_write` | Append a capture; runs policy + extraction + indexing | write |
| `memory_search` | Hybrid retrieval with optional `at_time` bitemporal filter | read |
| `memory_recall` | Entity-centric fact view (audit by default) | read |
| `memory_profile` | Synthesized markdown profile (scope or global) | read |
| `memory_forget` | Soft-delete via `forget` event | write |
| `memory_journal` | Policy decision log | read |

The MCP surface is **deliberately narrow**. The full CLI surface has ~20
commands; the MCP surface has 6 because every additional tool is a new
agent failure mode. The shape of "what an agent can do to your memory" stays
auditable.

### Prompts

Server-rendered markdown blobs the agent fetches via `prompts/get`:

| Prompt | What it returns |
|---|---|
| `session_context` | Synthesized profile + active project tags + last 5 captures, as a single markdown brief |
| `summarize_tag` (takes `tag` arg) | Tag-scoped summary |

`session_context` is the recommended **first turn of every session**. See
[AGENT_BOOTSTRAP.md](AGENT_BOOTSTRAP.md).

### Resources

MCP Resources URIs the agent can fetch directly:

| Resource | What |
|---|---|
| `localmem://profile` | Live profile markdown |
| `localmem://subjects` | List of distinct subjects with counts |
| `localmem://tags` | List of container tags with counts |
| `localmem://recent?limit=N` | Last N captures |

---

## 8. Agent usage patterns

### The session-start bootstrap

In any project that uses localmem, drop this line in your `CLAUDE.md` (or
`.cursorrules`, or system prompt):

> On the first turn of any session, call `prompts/get session_context` to
> orient yourself in this user's memory. Use `memory_search` for anything
> relevant to the current task before answering.

That single instruction turns localmem from "a thing the user can query" into
"a thing the agent reaches for automatically." See
[AGENT_BOOTSTRAP.md](AGENT_BOOTSTRAP.md) for the full pattern.

### Hot vs cold memory tiers

CLAUDE.md is the **hot tier** — always loaded, costs tokens every turn. Put
conventions, identity facts, operating rules there.

localmem is the **cold tier** — queryable on demand, costs tokens only when
queried. Put decisions, per-project facts, time-sensitive context there.

A fact earns promotion from cold to hot when:
- It's been referenced 3+ times across sessions, OR
- It shapes every output (a formatting rule, a hard constraint).

See [MEMORY_TIERS.md](MEMORY_TIERS.md) for the full promotion/demotion story.

### Per-task patterns

| Task | Pattern |
|---|---|
| Starting a new feature | Search localmem for prior decisions on similar features; write a fresh `decision` capture for the new direction |
| Onboarding a new project | `localmem init --home <repo>/.localmem`; agent inherits the empty project home, builds context organically |
| Reviewing a PR | Search for related decisions / constraints; cite them in the review |
| Debugging an incident | `localmem recall <service-name>` for known facts; write a `fact` capture for the post-mortem |
| Cross-tool continuity | Write in Claude Desktop, read from Cursor: they hit the same `~/.localmem` |

---

## 9. Per-project memory

The `--home` flag (and `LOCALMEM_HOME` env var) lets you scope memory to a
project instead of using the global `~/.localmem`. This is the
single-most-valuable pattern for agent use, per the field-feedback agents
who tested v0.2.

### Recommended layout

```
~/                         <- your home dir
├── .localmem/             <- personal/global memory (default)
│   ├── events.jsonl
│   ├── derived/
│   └── models/bge-small-en-v1.5/
│
└── projects/
    ├── my-saas/
    │   ├── .localmem/     <- per-project memory
    │   │   ├── events.jsonl
    │   │   ├── derived/
    │   │   └── models/    <- symlink to ~/.localmem/models/ (auto via `init`)
    │   └── .mcp.json      <- wires Claude Code to use this home
    │
    └── client-work/
        ├── .localmem/     <- separate per-project memory
        └── .mcp.json
```

### Wiring per-project memory to Claude Code

`<repo>/.mcp.json`:

```json
{
  "mcpServers": {
    "localmem": {
      "command": "npx",
      "args": ["-y", "localmem-mcp"],
      "env": {
        "LOCALMEM_HOME": "/Users/you/projects/my-saas/.localmem",
        "LOCALMEM_CORE_URL": "http://127.0.0.1:7788"
      }
    }
  }
}
```

Run `localmem serve --home /Users/you/projects/my-saas/.localmem` in a
terminal; Claude Code now sees only that project's memory.

### Sharing the embedder model

When you run `localmem init --home <new>/.localmem`, the new home gets a
symlink to `~/.localmem/models/bge-small-en-v1.5/` automatically. No need to
re-download the 44 MB model per project. If the global model isn't installed
yet, init prints a hint to run `localmem fetch-model` first.

---

## 10. Trust, recovery, and the moat

### The promises

These promises are non-negotiable. Every change in the codebase respects
them, every release ships with them intact:

1. **The event log is the source of truth.** Never mutated. "Delete" emits a `forget` event.
2. **All derived stores are recomputable.** If `localmem replay` cannot rebuild a file from `events.jsonl`, it does not belong in `derived/`.
3. **Schemas are versioned, never broken.** A 5-year-old `events.jsonl` must remain readable by today's binary.
4. **Local-first by default.** No code path may require a network call to complete a `memory_*` operation.
5. **No plaintext leaves the machine.** Even the future paid sync stores ciphertext only.
6. **Apache-2.0 forever.** The core binary and MCP server are not relicensable.

### Recovery scenarios

| Scenario | Fix |
|---|---|
| Tantivy lexical index corrupted | `localmem replay` (rebuilds from events.jsonl) |
| Lex schema mismatch after upgrade | Tool prints "Run: localmem replay" → run it |
| DuckDB facts table out of sync | `localmem replay` |
| Vector embeddings drifted vs new model | `localmem reindex` |
| Lost laptop, restore from backup | Copy `events.jsonl` back to `~/.localmem/`, run `localmem replay` |
| Switched to a new machine entirely | `localmem export → import`, then `localmem replay` |
| Want to inspect a fact's history | `localmem audit <fact-id>` |
| Confused about a recent forget | `localmem journal --since 24h` |

### What localmem will never do

- Phone home with content (no telemetry on captures, ever)
- Refuse to start because a license check failed
- Require a cloud account to use the core features
- Mutate or rewrite `events.jsonl` in place

---

## 11. How it compares

The honest competitive picture as of mid-2026:

| | localmem | Agentmemory | Memento | Supermemory / mem0 / Zep |
|---|---|---|---|---|
| GitHub stars | (early) | 10.5K | <100 (1.3K npm/wk) | 22.5K / mid / high |
| Where your data lives | Your machine | Your machine | Your machine | Their cloud |
| Runtime | **Single Rust binary** | Node + iii framework | Node | Cloud (SaaS) |
| Plaintext leaving your machine | **Never** | **Telemetry on by default** | Never | **Always** |
| `forget` is auditable | **Event in the log** | App-level delete | App-level delete | Trust them |
| Recoverable from a plain text file | **Yes (`localmem replay`)** | No | No | No |
| Bitemporal facts | **Yes** | No | No | No |
| Active contradiction resolution | **Yes (T-56)** | No | "Memento never decides" (docs) | varies |
| MCP tool count | **6 (narrow)** | 51 (wide) | ~26 | varies |
| License | Apache-2.0 (non-relicensable) | Apache-2.0 | Apache-2.0 | Mixed |
| If the company dies | **Your memory works** | Your memory works | Your memory works | **Your memory is gone** |

### When NOT to use localmem

Be honest about the fit:

- **You want cloud sync across devices right now.** v0.2.1 ships Personal Cloud; until then, you'd need to rsync `events.jsonl` yourself.
- **You want a polished web dashboard.** Memento has one; we don't yet.
- **You want 50+ MCP tools.** Agentmemory has them. We deliberately ship 6.
- **You want auto-capture hooks on every shell command.** Agentmemory has 12 of them. We capture only what you explicitly write or what the policy commits via the extractor.

---

## 12. Troubleshooting

### `localmem search` says "lexical index schema is stale"

Your derived store was written by an older binary. Fix:

```bash
localmem replay
```

This rebuilds from `events.jsonl`. Safe; no data loss possible (events.jsonl
is the source of truth).

### "embedder unavailable; search degraded to lex-only"

The BGE-small ONNX model isn't installed. Fix:

```bash
localmem fetch-model
localmem replay   # backfill vectors for prior captures
```

Hybrid search resumes. Until then, lex search still works.

### macOS: `cannot be opened because the developer cannot be verified`

For v0.2 we don't ship a notarized binary. The install script strips the
quarantine xattr automatically, but if you copy the binary manually or get
it from another source:

```bash
xattr -d com.apple.quarantine /path/to/localmem
```

### `localmem serve` fails: address already in use

Another `localmem serve` is running. Either kill it or use a different port:

```bash
localmem serve --addr 127.0.0.1:7789
```

Update your MCP config (`LOCALMEM_CORE_URL`) to match.

### MCP client doesn't see localmem

Run the diagnostic:

```bash
localmem doctor
```

It checks `binary on PATH`, `home initialised`, `embedder model`, `server reachable`, `macOS Gatekeeper`, `MCP wiring per client`. Anything in FAIL or WARN is your fix.

If `mcp wiring` is FAIL, re-run the install:

```bash
localmem mcp install --client claude   # or whatever client
```

### Lost track of what's where

```bash
localmem doctor             # health check
localmem subjects           # what entities are in memory
localmem tags               # what tags are in use
localmem recent             # last N captures
localmem journal --since 24h    # policy decisions in last 24h
```

### Want to start over

```bash
rm -rf ~/.localmem
localmem init
```

That's a full reset. If you want to keep history, `localmem export
~/lm-backup.tar.gz` first.

---

## 13. What's free vs paid

### Free, forever (Apache-2.0)

- The Rust core binary
- The TypeScript MCP server
- The event log schema
- All importers (ChatGPT, Claude export, mem0)
- All write policies
- Hybrid retrieval (lex + vec + facts)
- Bitemporal facts + smart forgetting
- Per-project scoping
- MCP integration

Your data lives in a folder you own. The company can disappear tomorrow and
your memory still works.

### Paid (opt-in, shipping in v0.2.1+)

- **Personal Cloud sync** — E2E encrypted relay across devices. Ciphertext only; no plaintext key material on the server.
- **Hosted Intelligence** — Cloud LLM endpoint for heavy ingestion (audio transcription, OCR, large PDFs). Local extractor still works without it.
- **Team contexts** — Shared memory namespaces for teams.
- **Enterprise audit + retention** — Compliance-grade logging beyond what the journal already provides.

The OSS core continues to work without any paid feature enabled. The paid
tier is opt-in by design.

The OSS core works without any paid feature; the paid tier is opt-in.

---

## 14. Roadmap and contributing

### Next-up after v0.2

- **v0.2.1** — Personal Cloud sync (E2E encrypted), Hosted Intelligence endpoint for heavy multimodal ingestion, MCP registry listing.
- **Beyond:** MMR re-ranker, scope hierarchy on container tags, `localmem supersede` as a clean primitive, local web dashboard, multimodal (audio / OCR / PDF) ingestion.

### Contributing

Issues welcome. Pull requests welcome — for larger changes, open an
issue first so we can discuss the approach.

The most valuable thing you can do is **install, use it for a week, and write
up the friction** in `docs/feedback/`. The two field-feedback reports that
shipped in v0.2 (`docs/feedback/2026-06-04-agent-field-notes.md`) drove an
8-bundle improvement (T-81) that touched ~1300 lines.

---

## Appendix: file layout

```
~/.localmem/
├── events.jsonl              <- source of truth (append-only)
├── config.toml               <- user config (retriever, policy, embedder)
├── derived/                  <- caches (rebuildable via localmem replay)
│   ├── facts.duckdb          <- bitemporal fact rows
│   ├── lexical.tantivy/      <- BM25 inverted index
│   ├── lexical.tantivy.version <- schema version sidecar (T-81 Bundle A)
│   ├── vectors.lance/        <- ANN vector index
│   └── journal.log           <- policy decision log
├── models/
│   └── bge-small-en-v1.5/    <- BGE-small ONNX (44 MB, fetched on demand)
├── policies/
│   ├── default.yaml          <- bundled default
│   └── user.yaml             <- your overrides (created on demand)
├── keys/                     <- E2E sync keys (v0.2.1+)
├── cache/                    <- transient state
└── logs/                     <- diagnostic logs
```

---

*localmem v0.2 — built by Vijay Yadav. Apache-2.0. Repo:
[github.com/VJ-yadav/localmem-public](https://github.com/VJ-yadav/localmem-public).*
