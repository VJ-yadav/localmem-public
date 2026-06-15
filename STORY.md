# Why localmem exists

A Claude Code instance helped scaffold this repo. The same Claude Code
instance cannot remember anything its user discussed in Cursor yesterday,
or in Claude Desktop this morning, or in ChatGPT last week. Every AI chat
the user starts begins from zero.

This is not a peculiar bug. It is the default state of every AI tool
shipped in 2026. ChatGPT remembers only what is inside ChatGPT. Claude
remembers only what is inside Claude. Cursor's memory is per-project,
scoped to one directory. The user's "AI self" does not transfer.

## The gaps, today

Here are the concrete failures of a Claude Code instance's memory layer,
as observed by the Claude Code instance that built this repo:

1. **Project silos.** Memory inside one repo is invisible to memory inside
   another, even though both share the same user, the same conventions,
   and many of the same patterns.
2. **No semantic search.** To find a past memory, the agent reads an index
   file and does mental grep over filenames. There is no
   `search("stripe webhook patterns")`.
3. **No temporal model.** A memory written 90 days ago about how some API
   works looks as authoritative as one written today, even after the API
   has changed.
4. **No contradiction handling.** If two memories conflict, both persist.
   The agent has to reconcile them on every read.
5. **No journal.** The agent cannot answer "why do I believe X?" because
   there is no audit trail behind any committed fact.
6. **No cross-tool sharing.** What Claude Code learns in one session does
   not carry to Claude Desktop, Cursor, ChatGPT, or any other tool the
   same user opens tomorrow.

Multiply this by 100 million daily AI users and the cost is staggering.
Every chat re-explains context. Every tool starts blind. The work of
building a useful AI assistant happens fresh every morning.

## What everyone else is building (updated 2026-05-16)

The AI memory landscape changed materially in the months around v0.1.
When we first scoped this project, no one had shipped a sovereign
memory layer. That is no longer true.

### The cloud-first incumbents

**Supermemory** raised $2.6M from Cloudflare's CTO and Jeff Dean. They
shipped a polished cloud-first product with 22.5K GitHub stars in four
months. Their MCP server lives at `mcp.supermemory.ai/mcp` behind OAuth.
Their `npx install-mcp` one-liner auto-configures Claude Desktop, Cursor,
and Windsurf in 30 seconds. They published `memorybench`, an MIT-licensed
public benchmark, on which they score 81.6% on LongMemEval_s (vs Zep
71.2%, full context 60.2%). They are the real competitive threat.

**mem0** raised $24M Series A on the same cloud-first thesis. **Zep** ships
a hosted bitemporal knowledge graph. **Letta** runs in their cloud or
yours, but the gravity is theirs.

Their existence validates the market. Their architecture also defines
the seat we cannot occupy if we follow them: holding your memory in
their cloud, in their format, behind their pricing power. Their revenue
model and their architecture are the same fact.

### The local-first peers (real, but limited)

**Memento** is an Apache-2.0 npm-installed MCP server backed by a single
SQLite file. It validates the local-first wedge: you can ship a sovereign
memory layer and get traction. But its SQLite-only stack cannot prove
recomputability (no event log + replay) and has no bitemporal facts.
Memento's PR #43 to memorybench is open as of 2026-05-15, so its benchmark
numbers will land soon.

**Contynu** is a coding-tool-specific OSS memory layer for Claude + Codex
+ Gemini. Useful as a reference for scope and kind taxonomy design.

**ArcBrain** is a closed-source paid desktop app with 56 MCP tools and a
visual memory browser. Validates that breadth of MCP surface area lands
with users.

### The vendor-locked memories (not our segment)

ChatGPT Memory, Claude Memory, Gemini Memory — each is proprietary and
locked to its vendor. Apple Memories will be the same when it ships. None
of them solve the cross-tool problem. They make it worse.

### The empty seat

The local-first option that is **also**:
- A single binary you own, in a folder you own, in an open format
- Recomputable (every derived store rebuildable from `events.jsonl`)
- Bitemporal (facts have valid_from/valid_to + recorded_at/retired_at)
- Cross-tool by protocol (MCP for any AI client, day 1)
- Apache-2.0 / FOSS forever (no relicensing risk)
- With an optional E2E encrypted sync that holds only ciphertext

That seat is open. We are taking it.

## What localmem is

localmem is the memory layer for the human, not the developer.

A single Rust binary turns a folder you own into the memory backend. A
TypeScript MCP server exposes that memory to every MCP-compatible AI tool:
Claude Desktop, Cursor, Codex, Claude Code, Windsurf, OpenCode, Cline,
Aider, custom GPTs, anything that speaks MCP. Write once. Read
everywhere.

The substrate is open: append-only JSONL events, recomputable derived
stores, bitemporal facts, an audit journal of every policy decision.
Apache-2.0 today, FOSS forever. When sync arrives in a future release, it
is end-to-end encrypted, opt-in, ciphertext only. We never see plaintext.

## Why the cloud incumbents cannot follow

Supermemory and mem0 cannot pivot here without undercutting their own
revenue. Their entire pricing power rests on holding the data. The day
they promise "your folder, your format, sovereignty forever," they hand
power back to the user and lose the only thing they sell.

This is not a temporary advantage. It is structural. The same reason
Notion cannot ship a local-first markdown vault without competing with
itself. The same reason Cloudflare cannot ship a peer-to-peer mesh
without undercutting Zero Trust. Cloud incumbents do not become
local-first companies. They acquire them, after they have already won.

Apple Memories cannot fill this seat because it would lock you to Apple.
ChatGPT Memory cannot because it would lock you to OpenAI. Anthropic
could but their incentive is to keep you inside claude.ai. The
Switzerland-of-AI-memory position is structurally available exactly
once, and only to a team that builds it open from day one.

## Why Memento doesn't close this seat either

Memento exists. Memento is Apache-2.0. Memento is MCP-native. So why
does the wedge still hold?

Three structural differentiations that Memento cannot match without
rewriting their core:

1. **Recomputable trust.** Memento is SQLite. There is no event log, no
   replay command, no journal. If you suspect their derived state is
   corrupt, you cannot prove it; you cannot rebuild it from first
   principles. Our `events.jsonl + localmem replay + journal.log` is
   the trust substrate they don't have.
2. **True bitemporal facts.** Memento uses decay-based forgetting. We
   carry valid_from/valid_to/recorded_at/retired_at on every fact. The
   difference shows up when you ask "what was true on March 5th?" —
   theirs gives a soft probabilistic answer; ours gives a definitive
   one.
3. **Single Rust binary.** Memento ships as npm/Node. We ship as a
   150 MB single executable you can `cp` between machines. No Node
   install dance, no version mismatches, no global npm namespace
   pollution.

These differentiations matter to the slice of users who care about
trust the most: privacy-conscious developers, enterprises with
compliance teams, anyone who has been burned by an AI tool's data
practices. That slice is small but high-LTV and high-influence. It is
the seed.

## The execution thesis: monetize at launch, not after

Tailscale gave away the free tier for 5 years before serious monetization.
That playbook required VC backing we do not have. We are bootstrapping,
which means revenue and adoption have to compound together from day 1
of v0.2.

The plan:
- **Free tier** is the complete local experience, forever. This is the
  viral loop. It is also our marketing budget.
- **GitHub Sponsors** opens at v0.2 launch. Believers fund early.
- **Personal Cloud** at $5/mo (E2E encrypted sync, hosted web UI, mobile
  app) opens at v0.2 launch. Even 1% conversion at 10K users is $500/mo
  — not the business, but it signals "this is a real product."
- **Hosted Intelligence** ($0.50 per 1000 LLM-assisted extractions) opens
  at v0.2 launch. Pay-per-use for users who want GPT-4-grade extraction
  without running a local LLM. Run on NVIDIA Inception compute credits in
  year 1.
- **B2B consulting** (Vijay-led, $5K-$50K per engagement) is available
  from day 1. Real revenue while the user base is still small.
- **Team and Enterprise tiers** ship once a reference customer lands
  (target: month 3-6 post-launch).

Target: $5K-$10K MRR within 90 days of v0.2 public release. $1M+ ARR by
month 18. These are not VC numbers; they are bootstrapping numbers,
matched to a single founder's runway from prior earnings + the
consulting revenue stream.

## NVIDIA Inception: the leverage we didn't build

localmem is in NVIDIA Inception. Concrete asset:
- DGX Cloud / B200 GPU credits cover our Hosted Intelligence compute for
  year 1 — Nemotron Nano 3 Omni runs on NVIDIA infrastructure for free.
- NVIDIA AI Enterprise license enables NeMo Curator + Retriever for
  document/audio/video ingestion at production grade.
- Co-marketing channels (NVIDIA developer blog, GTC presentations, "Built
  on NVIDIA" badge) give us a credibility signal that punches above our
  funding weight.
- Technical advisory from NVIDIA AI engineers compresses our
  model-deployment learning curve.

This partnership does not affect the trust commitments. No equity, no
exclusivity, no data sharing. It is sponsorship + resources, plain and
useful.

## The dogfood promise (now active, not future)

The Claude Code instance that helped build this repo was the first
user. As of 2026-05-15 the install is wired:
- `~/.local/bin/localmem` running locally
- `~/.localmem/` initialized with real captures about Vijay, the
  project, the competitive landscape, and the strategic plays
- MCP server connected to Cursor, Claude Desktop, and Claude Code
- 230+ lib tests passing, v0.1 acceptance script green end-to-end

This was the original promise of the project: "If localmem cannot
survive being a Claude Code instance's memory, it does not ship." It
ships.

Every Claude Code session, every Cursor session, every ChatGPT chat in
the world has the same gaps. The proof that the same solution works for
all of them is running right now on Vijay's MacBook.

## Where we are today (updated 2026-06-15)

A month after the landscape snapshot above, the substrate is real and
the differentiation has held. This section catalogs what shipped
between May and mid-June, and what changed in the broader picture.

**Substrate shipped and battle-tested:**

- **Active contradiction resolution.** A new high-confidence fact
  about the same `(subject, predicate)` automatically retires the
  prior conflicting one, with the supersession logged in the
  journal. Memento explicitly refuses to do this — their docs:
  *"Memento never decides which side of a conflict is right."* We
  decided, and we made the audit trail bulletproof.
- **Per-kind decay half-life.** `fact` decays over 90 days,
  `preference` over 180, `decision` over 365, `todo` over 14. The
  same numbers Memento ships. Matched on substance; we own them
  natively, not as a config bolt-on.
- **MMR re-ranker + cross-encoder rerank.** The hybrid retriever
  now diversifies top-k results (no near-duplicates) and rescore
  with a real cross-encoder. Memento ships MMR; the cross-encoder
  is ours.
- **Smart forgetting + todo lifecycle.** A new `UpdateCapture`
  event flips done/open on todo captures; the event log stays
  append-only, replay reconstructs the latest state.
- **Discovery API.** `localmem subjects` / `tags` / `summarize` /
  `recent` / `audit` — every fact traces back to its source
  capture, every retired fact lists why.
- **MCP Resources + Prompts.** `localmem://profile`, `://subjects`,
  `://tags`, `://recent` for live state. `session_context` and
  `summarize_tag` prompts auto-inject at session start.
- **Multi-tab local dashboard.** Timeline, Graph, Replay views on
  127.0.0.1:8088, served by the Rust core directly.
- **Five MCP clients auto-install:** Claude Desktop, Claude Code,
  Cursor, Windsurf, Cline. One command per client; revertible.
- **Lifecycle commands** (`setup`, `service`, `doctor`, `status`)
  for one-command install and OS-native auto-start.
- **Memorybench provider working** end-to-end against the public
  `supermemoryai/memorybench` benchmark. Private smoke runs
  underway; numbers go public when they clear our threshold.

**Distribution shipped:**

- `localmem-mcp` published to npm (`npx localmem-mcp install ...`)
- One-command install: `curl -fsSL https://localmem.org/install | sh`
- Community Edition repo (this one) under Apache-2.0

**The competitive picture, one month later:**

- **Supermemory** still leads on cloud-first benchmark scores.
  Their architectural choice (cloud holds the data) hasn't
  changed; the wedge we're building against them hasn't moved
  either.
- **Memento** continues to ship — they remain the closest direct
  OSS comparison. Where they're ahead today: per-git-branch scope
  grammar, curated YAML "memory packs" for cold-start onboarding,
  longer time in market.
- **agentmemory** has continued its star-curve growth and shipped
  the auto-capture-hooks pattern we don't have yet. We watch their
  releases; we won't copy the telemetry-by-default choice.
- **mem0** raised more money. Their thesis remains cloud-first.

**What's still pending before public launch:**

- Apple Developer ID + notarized macOS binary (so first-run
  doesn't trip Gatekeeper)
- Private LongMemEval / LoCoMo numbers ≥75% before any upstream
  PR to memorybench
- Branch-scope grammar (Memento has it; we want it)
- Curated YAML packs (engineering-simplicity, pragmatic-programmer,
  twelve-factor, google-sre — cold-start onboarding play)
- Typed conflicts view (`localmem conflicts list`) — surfaces
  smart-forgetting outcomes as queryable rows
- One-liner install hardening (notarization, prebuilt binaries
  for all four targets, hosted install script)

**What's not changing:**

The five hard rules from `EDITIONS.md` hold. Community Edition stays
Apache-2.0 forever. Enterprise and Cloud are separate repos, sold
to teams + companies that need multi-user identity, compliance, and
managed hosting. No reverse-degradation: anything that ships
Apache-2.0 stays Apache-2.0.

## The horizon

In year one, localmem is a developer tool with thousands of daily users
running it across Claude Code, Cursor, Claude Desktop, Windsurf, Codex,
and Cline. Revenue: $50K-$300K ARR from Personal Cloud + Hosted
Intelligence + B2B consulting.

In year three, it is the default memory MCP server in every AI agent
that ships, and the protocol position compounds. Revenue: $5M-$15M ARR
from sync + intelligence + Team tier + first enterprise customers.

In year five, it is the canonical answer to "where does your AI memory
live?" the way Obsidian became the answer to "where do your notes live?"
and Tailscale became the answer to "how do your devices reach each
other?" Public company trajectory: $50M-$200M ARR.

The product that wins this category will not look like an enterprise
SDK. It will look like a tool the user installs once and forgets, while
every AI tool they own quietly gets better at remembering them.

That is what we are building.
