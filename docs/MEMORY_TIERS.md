# Memory tiers: CLAUDE.md vs localmem

Every AI agent has at least two memory tiers, whether explicitly designed
or not:

| Tier | Mechanism | Cost per session | Latency |
|---|---|---|---|
| **Hot** (always loaded) | CLAUDE.md, harness file-memory, system prompt | Pays full token cost on every turn | Zero — already in context |
| **Cold** (queryable) | localmem, RAG store, vector DB | Pays only for the queries made | One round trip per query |

The trap most "memory" products fall into: they treat everything as hot.
The token bill grows linearly with what you remember, the context window
gets crowded with facts the model never references, and at some scale the
system starts to outweigh the agent.

localmem is **the cold tier**. CLAUDE.md (or your project's equivalent
always-loaded file) is **the hot tier**. They are not redundant; they are
two stages of a single memory hierarchy.

## What belongs in the hot tier (CLAUDE.md)

Things the agent must know *to function correctly on every turn*.
Roughly:

- **Conventions and constraints.** "Never use em dashes." "No
  `panic!` outside tests." "Use `anyhow` for app errors, `thiserror`
  for libraries." These shape how the agent generates output.
- **Operating rules.** "Don't push without explicit permission." "Run
  tests before claiming a task is complete." These prevent
  catastrophic actions.
- **Identity-of-the-project facts.** "This is a local-first memory
  layer." "Apache-2.0 forever." "The event log is the source of
  truth." These prevent the agent from drifting into wrong framing.

If you would say "but what if the agent doesn't see this on a given
turn?" the answer is "always loaded." That's the hot tier.

Budget: keep CLAUDE.md under ~2K tokens. Past that, every session pays a
tax for things the agent rarely needs.

## What belongs in the cold tier (localmem)

Things the agent should know *when relevant*:

- **Decisions and their rationale.** "We picked DuckDB over SQLite
  because of bitemporal columns." "Bundle the v0.2 release manually;
  CI is out of quota." Useful when the topic comes up. Pure waste when
  it doesn't.
- **Per-project facts.** Schemas, ATS coverage, employer metrics,
  benchmark scores. Project-scoped tags keep these from polluting
  each other.
- **Subject-centric profiles.** "Vijay prefers Rust for
  systems-level code." Surfaced by `memory_recall Vijay` or by a
  `memory_search` whose query mentions language preference.
- **Anything time-sensitive.** Bitemporal columns + the `as-of` query
  let the agent answer "what did we believe on date X" without
  rewriting history.

If you would say "the agent needs to be able to find this when asked,"
that's the cold tier.

## Promotion: cold → hot

A localmem fact earns a place in CLAUDE.md when:

1. **It is referenced ≥3 times across sessions.** `memory_journal`
   surfaces query patterns; repeated cold-tier hits on the same fact
   are a strong signal it should be hot.
2. **It is operationally load-bearing.** "Default model is BGE-small,
   slug `bge-small-en-v1.5`" gets referenced on every embedder
   debugging session. Promote it.
3. **It shapes every output.** A formatting rule that the agent
   keeps almost-but-not-quite getting right. Promote and reinforce.

Promotion is a manual edit of CLAUDE.md. localmem does not auto-promote
because the hot-tier budget is small and human-curated for a reason: the
cost of a wrong promotion is paid on every future session.

## Demotion: hot → cold (rare)

CLAUDE.md entries can age out. If a fact has not been referenced in a
quarter and was promoted in error, demote it: write it as a localmem
capture (so it stays findable) and delete it from CLAUDE.md. The hot
tier is for things the agent must see *every turn*; non-load-bearing
facts there are token waste.

## How this maps to other systems

| Tier | Cursor equivalent | Claude Desktop equivalent | Codex equivalent |
|---|---|---|---|
| Hot | `.cursorrules` | system prompt + project files | `instructions.md` |
| Cold | localmem (or any RAG store) | localmem | localmem |

The point is not that localmem is special. The point is that the
hot/cold distinction is real, agents work better when you respect it,
and pretending everything is one tier is how memory products grow
unbearable token costs.

## The bootstrap pattern

See [AGENT_BOOTSTRAP.md](AGENT_BOOTSTRAP.md) for the recommended
session-opener: call `prompts/get session_context` once at the start so
the agent knows what cold tier *exists*, then query specific facts via
`memory_search` / `memory_recall` as the task demands.
