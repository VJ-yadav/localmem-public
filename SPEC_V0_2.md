# localmem v0.2 — Product Spec (DRAFT)

**Status:** DRAFT. Locks before Phase 5 implementation begins. Changes
during build cause cascade rework; route them to v0.3.

This spec defines the v0.2 contract: what changes from
[SPEC.md](SPEC.md) (the locked v0.1 contract), what stays the same,
and what new capabilities ship. The *how* lives in
[ARCHITECTURE.md](ARCHITECTURE.md). The *when* lives in
[TASKS.md](TASKS.md). The *why* lives in
[STORY.md](STORY.md) and [MOAT.md](MOAT.md).

## What v0.2 must do (in addition to everything v0.1 already does)

A developer who installs localmem v0.2 on a fresh macOS or Linux
machine gets:

1. **One-line install with auto-MCP-wiring** to Claude Desktop, Cursor,
   Codex, Claude Code, Windsurf — every supported client picked up
   automatically.
2. **Discovery-first MCP surface** so any AI client can answer "what
   do you know about this user/project?" without trial-and-error
   searching.
3. **First-run import wizard**: on `localmem init`, detect existing
   memory sources (ChatGPT export ZIP, Claude export ZIP, Obsidian
   vault, Notion export, Memento SQLite, Mem0 / Supermemory export
   if available) and offer to import. Onboarding is rich from
   minute one, not empty.
4. **Context-rewritten captures** so each chunk is self-contained
   ("they prefer X" becomes "Vijay prefers X") and retrieval surfaces
   meaningful results instead of pronoun-laden fragments.
5. **Active contradiction resolution** so a new fact about
   `(subject, predicate)` automatically retires the prior conflicting
   one, without requiring the user to call `forget`.
6. **Recency-biased retrieval** so recent memories are surfaced
   preferentially when scoring is otherwise tied.
7. **Container-tag scoping** so memories can be tagged by project,
   client, context, and filtered at query time.
8. **Closed-core kind taxonomy** (fact, preference, decision,
   constraint, todo, note) + free-form extension so AI tools share a
   common vocabulary while keeping flexibility.
9. **Personal Cloud sync** (paid, opt-in, E2E encrypted) for users
   with multiple devices.
10. **Hosted Intelligence** (paid, opt-in, usage-priced) for users who
    want GPT-4-grade extraction without running a local LLM.
11. **MCP registry submission** — listed at
    `modelcontextprotocol.io`.
12. **Memorybench score published** — `LocalmemProvider` for
    `supermemoryai/memorybench`, with private benchmark runs first
    and public PR submission only when results meet the threshold
    (≥75% on LongMemEval).

If these twelve things ship and the v0.1 backend continues working
unchanged, v0.2 launches.

## Backward compatibility (load-bearing)

**Every `events.jsonl` written by v0.1 must be readable and replayable
by v0.2.** This is not optional. Schema migrations are forward
functions applied at read time per ARCHITECTURE.md invariant 6.

Specifically:
- `Event` envelope shape stays the same
- All six existing `EventKind` variants stay supported
- All six v0.1 MCP tool calls (`memory_write`, `memory_search`,
  `memory_recall`, `memory_profile`, `memory_forget`,
  `memory_journal`) continue to work with their existing request /
  response shapes
- v0.2 tools are *additions*, not replacements

## CLI surface (additions and changes)

All v0.1 commands remain. New commands:

### `localmem mcp install <client>` (NEW)

Auto-edits the named client's MCP config to register localmem. Mirrors
Supermemory's `npx install-mcp` UX for local stdio MCP.

| Behavior | |
|---|---|
| Supported clients | `claude` (Claude Desktop), `claude-code`, `cursor`, `codex`, `windsurf`, `cline`, `aider` |
| Side effects | Edits the client's MCP config file (with backup), adds `localmem` server entry with auto-detected paths |
| Output | Path to the updated config + path to the backup |
| Failure modes | Unknown client → error with list of supported. Config file not found → error with manual-edit instructions. |

### `localmem mcp list` (NEW)

Lists which clients have localmem wired up.

### `localmem mcp uninstall <client>` (NEW)

Removes the localmem entry from the named client's MCP config.

### `localmem doctor` (NEW)

Detects and fixes common first-run issues.

| Behavior | |
|---|---|
| Checks | binary on PATH, `~/.localmem/` initialized, model present, server reachable, Gatekeeper quarantine, MCP wiring per client |
| Output | List of checks with PASS/FAIL/FIX. FAIL entries include the one-line command to fix. |
| `--fix` | Applies the fixes automatically (with confirmation) |

### `localmem fetch-model [name]` (NEW)

Downloads a local LLM model into `~/.localmem/models/` for the text
extractor + context rewriter pipeline. v0.2 ships text-only.

| Behavior | |
|---|---|
| Default (no name) | Fetches `llama3.2:3b` (~2 GB quantized; works on any laptop with 8 GB RAM) |
| Named options | `llama3.2:3b` (default), `qwen2.5:7b` (power users), custom HuggingFace URL |
| Caching | Re-running on existing model is a no-op |
| Disk-space check | Refuses to fetch if free disk < 2x the model size; prints clear error with required vs available |
| `--dry-run` | Prints what would be fetched + total size without downloading |

Multi-modal models (vision, audio, video) ship in v0.3.

### `localmem extract <event-id>` (NEW)

Re-runs the LLM extractor on a specific capture event. Useful when
upgrading the extractor model or rule set.

### `localmem rewrite <event-id>` (NEW)

Applies context rewriting to a specific capture event. Outputs the
rewritten text alongside the original for review.

### `localmem audit <fact-id>` (NEW)

Traces a fact back through the event log + journal to its source.
Answers "why does the AI know this?"

### `localmem benchmark` (NEW)

Runs the local LoCoMo / LongMemEval / ConvoMem benchmarks via the
embedded `LocalmemProvider` harness. Reports numbers locally.

### Changes to existing commands

| Command | Change |
|---|---|
| `localmem write` | `--tags key=value,key=value` accepts arbitrary container tags. `--kind` is now meaningful (drives extraction patterns + profile rendering). |
| `localmem search` | `--tags`, `--kind`, `--source` filters added. |
| `localmem recall` | `--tags` filter added. |
| `localmem profile` | `--tags`, `--kind` scoping added. |
| `localmem forget` | `--tags` criteria added (forget all memories with given tag). |
| `localmem replay` | Becomes idempotent on extracted facts (re-runs context rewriting + contradiction resolution deterministically). |

## MCP surface (compressed and reorganized)

v0.1 ships 6 flat tools. v0.2 reorganizes around all three MCP
primitives: 5 Tools + 4 Resources + 2 Prompts.

### Tools (5)

| Tool | Args | What |
|---|---|---|
| `memory` | `content: string, action?: "save"\|"forget", kind?: string, tags?: object` | Save or forget a memory. Replaces v0.1's `memory_write` + part of `memory_forget`. |
| `recall` | `query?: string, entity?: string, tags?: object, kind?: string, at_time?: string, k?: number, include_profile?: boolean` | Unified retrieval. Replaces `memory_search` + `memory_recall` + part of `memory_profile`. |
| `journal` | `since?: string, action?: string, fact_id?: string` | Audit trail. `fact_id` triggers `localmem audit` behavior (trace this fact). |
| `update` | `id: string, content?: string, kind?: string, tags?: object` | Refine an existing memory in place. New in v0.2. |
| `import_inline` | `format: string, data: string` | One-shot bulk import via tool call (vs CLI). Supports `archive`, `chatgpt`, `claude`. |

**The `memory` and `recall` consolidations follow Supermemory's
pattern.** A single `memory` tool with an `action` parameter is more
discoverable than separate `write` + `forget` tools. Same for `recall`
unifying search + recall + profile preview.

### Resources (4) — NEW

MCP Resources are URIs the client can subscribe to. Updates propagate
automatically.

| URI | What |
|---|---|
| `localmem://profile` | Current synthesized markdown profile (auto-refreshes when facts change) |
| `localmem://subjects` | List of all entity subjects we know about + their fact counts |
| `localmem://tags` | List of all tags in use + how many memories carry each |
| `localmem://recent` | Last N captures (default 20) ordered by recorded_at desc |

### Prompts (2) — NEW

MCP Prompts are templates the server provides for the client to inject
at session start or on user request.

| Prompt | What |
|---|---|
| `session_context` | Server-provided opening brief for any AI session. Renders a concise summary of the user's identity + active projects + recent context. AI clients inject this at session start automatically. |
| `summarize_tag` | Server-provided template for "give me a brief on tag X". Takes a tag, returns a markdown summary. |

### Why this matches MCP idiom

- **Tools = actions** (save, recall, update — verbs)
- **Resources = subscribable state** (profile, recent — nouns)
- **Prompts = templates the server defines** (session opening, tag brief — composable)

Supermemory uses all three. We were using only tools. v0.2 fixes that.

## The container-tag model (simpler than scope hierarchy)

v0.2 adopts Supermemory's pattern: scopes are just **tags** (arbitrary
`key=value` metadata).

```jsonc
// example capture with tags
{
  "kind": "capture",
  "payload": {
    "text": "Auth in StudentHousing uses Auth0 with hybrid frontend forms",
    "extra": {
      "kind": "constraint",
      "tags": {
        "project": "studenthousing",
        "topic": "auth",
        "client": "internal"
      }
    }
  }
}
```

**Two reserved tag keys with semantic behavior:**

| Tag key | Values | Behavior |
|---|---|---|
| `retention` | `permanent` (default) / `ephemeral:<TTL>` (e.g. `ephemeral:24h`) | Ephemeral memories auto-expire and disappear from search after TTL. |
| `visibility` | `surfaced` (default) / `private` | Private memories returned ONLY by explicit `recall(entity=X)` with no query — never by default search or profile. |

**All other tags are user-defined and arbitrary.** Tools self-organize
by tag conventions. Common patterns: `project=<name>`, `topic=<name>`,
`client=<name>`, `repo=<path>`.

**No hierarchical scope. No special-case session/user/project.** Just
tags + two reserved keys for behavior.

Project auto-detection: when the MCP server is spawned by a client
within a directory (e.g. Cursor opens a project), the server reads
`LOCALMEM_PROJECT_TAG` env var (set by the client config) and adds
`project=<tag>` to every write.

## Kind taxonomy

Closed core of 6 kinds. Anything else round-trips through `extra` but
gets no special semantic treatment.

| Kind | What | Lifecycle | Extractor pattern |
|---|---|---|---|
| `fact` | Bitemporal claim, S-P-O triple | Permanent, can be retired via update/forget | "X is Y", "X = Y" |
| `preference` | "I prefer X" / "I like X" / "I avoid X" | Permanent; new pref about same subject triggers smart forgetting | "I prefer X", "I like X", "I avoid X", "X over Y" |
| `decision` | A choice made + rationale, append-only audit | Permanent, never retired (decisions are historical) | "we chose X because Y", "decided to X due to Y" |
| `constraint` | A rule that bounds future work | Permanent; new constraint about same predicate triggers smart forgetting | "always X", "never X", "must X", "do not X" |
| `todo` | Actionable item with `done` state | Until `done=true` flag set | "TODO: X", "need to X", "should X" |
| `note` | Freeform, no extraction applied | Permanent; the catch-all | (catch-all default) |

**Recall returns multiple kinds at once by default.** Filter via
`recall(kind="preference")` or similar.

**Profile groups by kind.** Preferences listed separately from
decisions; todos shown with `done` state; constraints in their own
section.

**Extensions:** any other `kind` value (e.g. `recipe`, `meeting_note`,
`code_snippet`) is stored verbatim, treated as `note` for behavior,
and surfaced under "other" in profile output.

## Active behaviors (the "use everything actively" doctrine)

### Context Rewriting at ingest

Every capture text is rewritten to be self-contained before lexical
+ vector indexing. "they prefer X" → "Vijay prefers X". This makes
retrieved chunks meaningful in isolation, matching Supermemory's
contextual retrieval approach.

| Mode | When |
|---|---|
| `none` (default if no LLM available) | Capture is indexed verbatim |
| `regex` (default if `LOCALMEM_REWRITE=regex`) | Pronoun substitution via deterministic rules — fast, no LLM |
| `local-llm` (default if `llm_assist = true` in config + Ollama running) | Llama 3.2 3B / Qwen 2.5 7B rewrites the chunk |
| `hosted` (paid Hosted Intelligence tier) | GPT-4o-mini via our hosted endpoint |

The rewrite happens at capture time and is stored in a new
`rewritten_text` field on the capture event (additive; original text
preserved). Retrieval indexes both. Search hits return the rewritten
text by default; the original is available via the resource URI.

### Smart Forgetting (active contradiction resolution)

When a new `fact` or `preference` lands with `(subject, predicate)`
that matches an existing live (non-retired) fact:

1. The old fact's `retired_at` is set to the new fact's `valid_from`
2. The old fact's `valid_to` is set to the same instant
3. An `Update` event is emitted in the log with `supersedes_id`
   pointing at the old fact
4. The journal records the contradiction with reasoning

The old fact is NOT deleted; bitemporal queries can still see it
"as of" an earlier time. This uses the bitemporal substrate we built
in v0.1 but didn't actively use.

**Confidence threshold:** contradiction resolution only fires when the
new fact's `confidence >= 0.7`. Lower-confidence facts are appended
without retiring others; the journal flags the contradiction for the
user.

### Recency Bias in retriever

Hybrid retriever scoring (v0.1: RRF over BM25 + ANN + temporal
filter) gains a recency boost:

```
final_score = rrf_score + recency_boost
recency_boost = w * exp(-age_days / 30)
```

Default `w = 0.01` (small enough not to dominate, large enough to
break ties toward recent memories). Configurable via
`[retriever].recency_weight` in `config.toml`.

### Dual-layer timestamps (refined)

v0.1 already has `valid_from` (when fact was true in reality) and
`recorded_at` (when we wrote it). v0.2 surfaces both in UX:

- `memory_recall` returns both timestamps
- Profile shows `valid_from` (the meaningful time)
- Journal shows `recorded_at` (the audit time)
- New CLI flag `--show-recorded-at` adds both columns to output

## Pluggable extractor system

v0.2 generalizes the v0.1 rule-based extractor into a trait + plugin
system.

```rust
pub trait Extractor: Send + Sync {
    fn extract(&self, text: &str, kind_hint: Option<&str>) -> Vec<ExtractedFact>;
    fn name(&self) -> &str;
}
```

Built-in extractors:

| Name | Patterns covered | Cost | Default |
|---|---|---|---|
| `rules` | `prefer`, `is`, `has_email`, `lives_in`, etc. (v0.1 + expanded) | Free | Always on |
| `local-llm` | Any pattern; Llama 3.2 3B / Qwen 2.5 7B / Hermes-4-14B | CPU + RAM | On if `llm_assist = true` in config |
| `hosted` | Highest quality; GPT-4o-mini via our endpoint | $0.50 / 1K extractions | On for paid Hosted Intelligence subscribers |

User-defined extractors via `policies/extractors/*.yaml`:

```yaml
# policies/extractors/code_patterns.yaml
id: rust_patterns
patterns:
  - match: "use the (.+) crate for (.+)"
    fact:
      subject: "{{capture[1]}}"
      predicate: "used_for"
      object: "{{capture[0]}}"
      confidence: 0.6
```

All extractors run in parallel; results are deduplicated by
`(subject, predicate, object)`. Higher-confidence facts win on
collision.

## Pluggable retriever system

Similar trait-based design:

```rust
pub trait Retriever: Send + Sync {
    async fn search(
        &self,
        query: &str,
        k: usize,
        filters: &Filters,
    ) -> Result<Vec<Hit>>;
    fn name(&self) -> &str;
}
```

Built-in:

| Name | When | New in v0.2 |
|---|---|---|
| `hybrid` | Default | v0.1 + recency bias |
| `entity-graph` | When query mentions a known subject | NEW |
| `rerank-wrapper` | Wraps another retriever with LLM-based reranking of top-K | NEW |

`Filters` carry the v0.2 tag + kind + visibility filters.

## Local LLM tier (for the text extractor only)

The only tier model that lives in v0.2 is for the **text extractor**
itself: rules → local LLM → hosted LLM. Hardware variation is a
real concern here because extraction quality matters and local LLMs
have real RAM costs.

| Tier | Hardware | What it does | Default |
|---|---|---|---|
| **rules** | Any machine | Regex-pattern fact extraction (v0.1 logic, expanded) | ✅ Always on. Zero install, instant. |
| **local-llm** | 8 GB+ RAM | Llama 3.2 3B via Ollama runs extraction + context rewriting | Opt-in (`llm_assist = true` in config) |
| **hosted** | Any machine + network | GPT-4o-mini-grade extraction via our endpoint | Opt-in (Hosted Intelligence subscriber) |

Extractors compose in parallel via the registry (T-58); results
deduplicate by `(subject, predicate, object)` with highest-confidence
winning collisions. So users with both `rules` and `local-llm`
enabled get the union, not a choice.

`localmem doctor` reports current extractor status and suggests
upgrades:

```
$ localmem doctor
  ✓ Extractor (rules): enabled
  ⚠ Extractor (local-llm): disabled — recommend `localmem fetch-model llama3.2:3b` for higher-quality extraction
  - Extractor (hosted): not subscribed — sign up at localmem.io/account
```

**Why not deeper tiers?** Because anything beyond Llama 3.2 3B
trades RAM for marginal extraction quality wins, and users who want
the marginal wins (~7B models or hosted) are a small minority. We
serve the 80% with rules + 3B, the 20% with hosted.

## What v0.2 does NOT do (explicit scope cut)

- **No multi-modal ingestion in v0.2.** PDFs, images, audio, and video
  are deferred to v0.3 as a marquee release. Rationale: ~95% of what
  AI tools write to memory is text. Multi-modal needs proper
  hardware-tier UX, model evaluation against domain benchmarks, and
  user research we have not done yet. Adding it to v0.2 would
  fragment the install experience (multi-GB model downloads on
  diverse hardware), delay launch by weeks, and answer a "could we"
  question instead of a "do users need this" question. v0.3 ships it
  right, with proper benchmarks and per-format model choices, after
  v0.2's text foundation is solid in the wild.
- No knowledge-graph database. Entity-graph retriever uses DuckDB
  recursive CTEs over the bitemporal facts table.
- No team / shared contexts. Personal Cloud is single-user. Team tier
  ships in v0.3.
- No mobile app. Ships in v0.3 after sync proves out.
- No web UI in the OSS core. Hosted dashboard is paid, separate repo.
- No Windows binaries. macOS + Linux only.
- No automatic capture from AI tools beyond what they choose to write
  via MCP. Capture-on-conversation is v0.3.
- No streaming / WebSocket MCP. Stdio + HTTP transports only.

## Discovery API

The new MCP Resources are the discovery primitives. CLI equivalents:

| Command | Output |
|---|---|
| `localmem subjects` | List of all known entities with fact counts |
| `localmem tags` | List of all tags with memory counts |
| `localmem summarize [tag=X]` | Synthesized brief (markdown) |
| `localmem recent [--limit N]` | Last N captures, newest first |

All of these are derived state (rebuildable via `localmem replay`),
not new event kinds.

## Configuration

`config.toml` gains new sections:

```toml
[extractor]
default = "rules"             # rules | local-llm | hosted
llm_assist = false            # opt-in; uses Ollama if true
llm_model = "llama3.2:3b"     # Ollama model tag for local-llm
custom_extractors = "policies/extractors/*.yaml"

[rewriter]
mode = "none"                 # none | regex | local-llm | hosted
llm_model = "llama3.2:3b"

[retriever]
recency_weight = 0.01
plugins = ["hybrid"]          # add "entity-graph", "rerank-wrapper" to enable

# Multi-modal ingestion is v0.3 — no [multimodal] section in v0.2.

[sync]
enabled = false
endpoint = "https://sync.localmem.io/v1"  # or self-hosted
key_file = "keys/master.key"

[hosted_intelligence]
enabled = false                # opt-in
endpoint = "https://intel.localmem.io/v1"
api_key = ""                   # set via env: LOCALMEM_HOSTED_API_KEY

[telemetry]
enabled = false                # opt-in always
events_only = true             # never user content; aggregate counts only
```

All cloud features are opt-in. The `enabled = false` defaults preserve
the local-first promise.

## Scale targets (measurable)

| Operation | Target | Measurement |
|---|---|---|
| `memory_write` p95 (committing capture, indexing, extracting) | <200 ms at 100K events | Latency benchmark via `localmem benchmark write` |
| `memory_search` (hybrid, k=10) warm | <100 ms at 100K events | Latency benchmark via `localmem benchmark search` |
| `memory_search` cold (first query after restart) | <500 ms at 100K events | Same |
| `localmem replay` over 100K events | <30 s on M-series Mac | Existing benchmark, gated by extractor |
| LongMemEval_s score (private) | >75% before public PR to memorybench | `localmem benchmark longmemeval` |
| LoCoMo score (private) | >75% before public PR | `localmem benchmark locomo` |
| Storage growth per user per month (typical) | <100 MB (events + derived) | Telemetry (opt-in) |

These are aspirations that gate v0.2 launch. If we don't hit
the benchmark thresholds, we don't ship publicly.

## Monetization surface

### Personal Cloud sync ($5/mo)

User sets up an account, runs `localmem sync init --account <email>`.
Server stores E2E encrypted blobs of user's `events.jsonl` and derived
stores. Sync runs every N minutes (configurable).

Sign-up flow: `localmem account create <email>` → email verification →
`localmem sync init`. Account state lives at `keys/account.json`.

**The localmem core continues to work without an account.** Sync is
opt-in convenience.

### Hosted Intelligence ($0.50 per 1000 extractions)

Set `[hosted_intelligence].enabled = true` + API key. The extractor +
rewriter dispatch to our endpoint instead of local LLM. Higher quality
(GPT-4-grade); usage-priced.

Usage tracked locally + on server; user can see spend via
`localmem account spend`.

### GitHub Sponsors

`localmem account` command surfaces a "Support the project" link to
GitHub Sponsors page. No code changes; pure marketing.

## Behavioral invariants (extends v0.1's list)

In addition to v0.1's invariants:

7. **Backward compatibility:** every `events.jsonl` written by any
   v0.1.x is readable and replayable by v0.2.x.
8. **Active substrate use:** every feature has a consumer.
   Bitemporal facts have contradiction resolution. Tags drive search
   filters. Kinds drive UX. Journal drives audit.
9. **No required network call:** every `memory_*` MCP call works
   offline. Cloud features (sync, hosted intelligence) are opt-in
   and gracefully degrade.
10. **Local-first license:** core stays FOSS (Apache or AGPL, see
    docs/LICENSING.md). Never proprietary.
11. **MCP tool surface compresses, not grows:** new capability
    expressed via Resources + Prompts when possible, Tools only when
    necessary.

(Scope cut list lives in "What v0.2 does NOT do" above.)

## Acceptance criteria for v0.2 (demoable end-to-end)

The following sequence works on a fresh machine in under 3 minutes:

```bash
# Install
$ curl -fsSL https://localmem.co/install | bash
$ localmem init

# Wire up MCP clients (auto-config)
$ localmem mcp install claude
$ localmem mcp install cursor

# Fetch local LLM for extraction + rewriting
$ localmem fetch-model llama3.2:3b

# Health check
$ localmem doctor
  ✓ Binary on PATH
  ✓ Home initialized at ~/.localmem/
  ✓ Model present: llama3.2:3b
  ✓ Server reachable: 127.0.0.1:7788
  ✓ MCP wired: claude, cursor

# Use it (these are MCP tool calls Claude/Cursor make)
> use memory tool to remember "I prefer functional Rust" with tag project=localmem
[ commits, context-rewritten to "Vijay prefers functional Rust", extracted as preference fact ]

> use recall tool to summarize what you know about project=localmem
[ returns synthesized markdown profile filtered to tag ]

# Bulk import from an existing source (caught at first-run wizard too)
$ localmem import chatgpt ~/Downloads/chatgpt-export/conversations.json
  Imported 247 events (89 conversations, 12 skipped) into ~/.localmem
  Next: run `localmem replay` to rebuild derived stores.

# Audit trail
$ localmem audit 01HXYZ...
  Fact 01HXYZ derived from:
    capture event 01HABC (2026-05-15T14:22)
    extractor: rules (rule: prefer)
    confidence: 0.7
    contradicted by: 01HMNO (retired at 2026-06-01)

# Personal Cloud sync (paid, opt-in)
$ localmem account create vj@example.com
$ localmem sync init

# In Cursor on a different machine:
> use recall tool: what are my project=localmem preferences?
[ returns same memories, synced via cloud, E2E encrypted ]
```

If those steps produce the expected outputs on a fresh macOS or
Linux machine in under 3 minutes, v0.2 ships.

## Public launch checklist (gates v0.2 release announcement)

- [ ] All v0.2 acceptance steps above pass on a clean machine
- [ ] LongMemEval_s score ≥75% (private benchmark via
      `LocalmemProvider`)
- [ ] LoCoMo score ≥75% (private benchmark)
- [ ] `npx-style` install via `curl | bash` works on macOS arm64 +
      x86_64 + Linux x86_64 + aarch64
- [ ] Code-signed and notarized binaries published (requires Apple
      Developer ID — $99/yr decision)
- [ ] MCP registry listing live at `modelcontextprotocol.io`
- [ ] memorybench PR submitted with `LocalmemProvider`
- [ ] Personal Cloud sync infrastructure deployed
- [ ] Hosted Intelligence endpoint deployed (on NVIDIA Inception
      credits)
- [ ] `docs/CLAUDE_DESKTOP_SETUP.md`, `INSTALL.md`, `DOGFOOD_LOG.md`
      reviewed and current
- [ ] README front page rewritten for the v0.2 narrative
- [ ] Demo video recorded showing the acceptance sequence end-to-end
- [ ] Landing page at localmem.co live
- [ ] GitHub Sponsors profile set up
- [ ] First two B2B consulting engagements lined up

Then ship.

## Locking the spec

This spec stays DRAFT until the design questions in
[docs/](docs/) are resolved:

- [ ] Final decision on licensing (Apache vs AGPL+commercial) per
      `docs/LICENSING.md`
- [ ] Account / sync protocol design (separate doc, not yet written)
- [ ] Hosted Intelligence endpoint contract (separate doc, not yet
      written)
- [ ] Pricing of Hosted Intelligence ($0.50/1K is placeholder)

Once those resolve, this spec locks. Then Phase 5 implementation
(TASKS.md T-47+) begins, working strictly against the locked
contract.

Changes during build cause cascade rework. Route to v0.3.
