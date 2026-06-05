# Shared memory for AI agents — the "stop re-explaining everything" pattern

You have two (or more) AI agents that lose context between sessions and need
to be re-instructed every time. localmem solves this. Below is the exact
setup that gets both agents reading from the same memory store, so anything
you tell one is immediately available to the other.

This works for any combination of Claude Desktop, Claude Code, Cursor,
Cline, Windsurf, Codex, Continue, Zed — any MCP-aware client.

---

## The problem you have today

```
                                                            ┌──────────────────┐
[ Session 1 — Agent A ]                                     │ Your project's    │
"Hey Agent A, remember: we use Rust 1.83+, ━━━ instructs ━▶│ tribal knowledge: │
 our test framework is XYZ, the API key is   (forgotten)    │ Rust 1.83+        │
 in env var FOO, never use feature X..."                    │ Test framework XYZ│
                                                            │ Env var FOO       │
[ Session 2 — Agent A, next day ]                           │ Never use X       │
"What's our test framework?"                                │ ...               │
"I don't know, can you tell me?" ━━━━━━━━━━━━━━━━━━━━━━━━━━│ (only in your     │
"...I already told you this YESTERDAY."                     │  head and Slack)  │
                                                            └──────────────────┘
[ Session 3 — Agent B ]
"Use Agent B for code review."
"Sure, what are your conventions?"
"...I told this to Agent A two days ago."

```

Each agent starts every session **from zero**. You become the
human bus, repeatedly transferring the same facts between sessions
and between agents. This costs hours every week and the explanation
quality degrades over time as you get tired of repeating yourself.

## The fix with localmem

```
                            ┌────────────────────────┐
                            │ ~/.localmem/           │
                            │   events.jsonl         │
                            │   (the source of truth)│
[ Agent A ] ◀━━━━ MCP ━━━▶ │   facts.duckdb         │
                            │   vectors.lance/       │
[ Agent B ] ◀━━━━ MCP ━━━▶ │   lexical.tantivy/     │
                            │                        │
[ Agent C ] ◀━━━━ MCP ━━━▶ │  Bring all your tribal │
                            │  knowledge into one    │
[ ...etc ]  ◀━━━━ MCP ━━━▶ │  place. Every agent    │
                            │  reads it.             │
                            └────────────────────────┘
```

Write the project's conventions / preferences / decisions ONCE.
Wire every agent into localmem via MCP. Each agent now starts every
session pre-loaded with what you would have told them anyway. Stop
re-explaining.

---

## Five-step setup (one machine, multiple agents)

### Step 1 — Install localmem (once per machine)

```bash
curl -fsSL https://localmem.org/install | sh
localmem init
localmem fetch-model   # ~44 MB download for semantic search
localmem doctor        # confirm everything's healthy
```

Expected output of `doctor`: mostly PASS, a few WARN for MCP clients
not wired yet (we fix those next).

### Step 2 — Start the core HTTP daemon (keeps running)

```bash
localmem serve &
```

Leave this running. Every MCP-aware agent will talk to it. If you
restart your machine, run `localmem serve &` again (or set up a
launchd / systemd service later).

To verify: `curl -fsS http://127.0.0.1:7788/healthz`. Should return OK.

### Step 3 — Wire every agent into localmem (one command each)

```bash
# Claude Desktop
localmem mcp install --client claude

# Claude Code (CLI)
localmem mcp install --client claude-code

# Cursor
localmem mcp install --client cursor

# Cline (VS Code extension)
localmem mcp install --client cline

# Windsurf
localmem mcp install --client windsurf
```

Each command auto-edits the client's MCP config to point at your
local localmem core. **Restart each client after wiring**.

Verify: open any client, ask the agent "do you have memory tools?".
It should mention `memory_write`, `memory_search`, `memory_recall`,
`memory_profile`, `memory_forget`, `memory_journal`.

### Step 4 — Write your project's tribal knowledge ONCE

Decide what every agent needs to know about your project. Capture
each as a memory with the right `--kind`:

```bash
# Preferences (stable: language choice, tooling, conventions)
localmem write --kind preference \
  --content "We use Rust 1.83+ for all systems-level code; Python for ML pipelines only"

localmem write --kind preference \
  --content "Test framework: insta for Rust snapshot tests, pytest with hypothesis for Python"

localmem write --kind preference \
  --content "Commit message style: conventional commits with scopes (feat/fix/chore/docs/refactor)"

# Constraints (hard rules that never change without a deliberate decision)
localmem write --kind constraint \
  --content "Never commit .env files. The API key for production lives in 1Password under 'PROD/API'."

localmem write --kind constraint \
  --content "No new dependencies without explicit approval; review supply chain risk first"

# Decisions (made once, agents should defer to them)
localmem write --kind decision \
  --content "We picked DuckDB over SQLite for analytics workloads because of bitemporal column support"

localmem write --kind decision \
  --content "Default LLM provider order: Anthropic Claude > OpenAI > Ollama local fallback"

# Facts (verifiable, source of truth)
localmem write --kind fact \
  --content "Production cluster runs on us-east-1 with three nodes, deployed via Terraform from infra/ repo"
```

Each `write` returns an event_id and runs through the policy engine
(which can decide COMMIT / DEDUP / SKIP). Look at the output: you
should see `action=COMMIT` for substantial captures.

For things you want **scoped to one project** (not visible to memory
for other projects), add a tag:

```bash
localmem write --kind preference \
  --content "JPI uses pgvector for vector search, not LanceDB" \
  --tags project=jpi
```

You'll see how to query with the tag in step 5.

### Step 5 — Have your agents bootstrap themselves with `session_context`

Open any of the wired agents and on its first turn say:

> "Use the `session_context` prompt to brief yourself on what you know
> about me and my projects before answering my next question."

The MCP server renders a single markdown blob: your synthesized
profile + active project tags + last 5 captures. The agent reads it,
and now has the context it would have lost between sessions.

For project-scoped recall:

> "Use `memory_search` with the query 'rust testing conventions' to
> find anything relevant before answering."

Or for a specific project tag:

> "Use `memory_recall` for entity 'JPI' to remind yourself of decisions
> made for that project."

---

## Make this automatic (so you don't have to remember to invoke it)

Most agents read a project-level instruction file (`CLAUDE.md`,
`.cursorrules`, `.windsurfrules`, system prompt). Add this line to it:

> On the first turn of any session, call `prompts/get session_context`
> to orient yourself in this user's memory. For any task, call
> `memory_search` with the task description as the query before
> answering, so you don't re-derive things already known.

Now every agent self-bootstraps on every session. You stop being the
human bus.

---

## How to verify it's working

After day 1 setup, try this flow:

1. **Day 1, Agent A:**
   "Remember: I prefer parameterized SQL over string interpolation,
    even in scripts. This is non-negotiable."
   (Agent A calls `memory_write` with `kind=constraint`.)

2. **Day 2, Agent B (different agent entirely):**
   "Write a quick Python script that selects users by email."
   Agent B calls `memory_search` for "Python sql best practices" (per
   the standing instruction), finds your constraint, and produces a
   parameterized query without you having to remind it.

The litmus test: **you didn't re-explain anything to Agent B.**

---

## Multi-project per-machine setup (optional, more advanced)

If you want **per-project memory** (so JPI's facts don't show up
when working on a different project):

```bash
# Per-project init
cd /path/to/jpi
localmem init --home ./.localmem

# In any .mcp.json for THIS project:
{
  "mcpServers": {
    "localmem": {
      "command": "npx",
      "args": ["-y", "localmem-mcp"],
      "env": {
        "LOCALMEM_HOME": "/path/to/jpi/.localmem",
        "LOCALMEM_CORE_URL": "http://127.0.0.1:7788"
      }
    }
  }
}

# Run the core against this specific home
localmem serve --home /path/to/jpi/.localmem
```

The init step auto-symlinks the global model directory so you don't
re-download the 44 MB ONNX file per project.

---

## Common pitfalls

| Symptom | Fix |
|---|---|
| Agent says "I don't see any memory tools" | Restart the AI client after running `localmem mcp install`. Config changes are only picked up at startup. |
| Agent has tools but `memory_search` returns nothing | `localmem serve` isn't running. Start it. Run `curl http://127.0.0.1:7788/healthz` to confirm. |
| Search returns lex matches but no semantic ones | Model not downloaded. Run `localmem fetch-model` and then `localmem replay` to backfill vectors. |
| "Lexical index schema is stale" error | Run `localmem replay`. The event log is the source of truth; only the cache is stale. |
| You wrote a memory but the agent doesn't reference it | Tell the agent to call `memory_search` explicitly with the relevant query. Or set up the standing instruction (see "Make this automatic" above) so every session starts with `session_context`. |

---

## The mental model that makes this stick

Two memory tiers:

| Tier | What goes here | Example |
|---|---|---|
| **Hot** (CLAUDE.md / .cursorrules / system prompt) | Always-loaded; conventions and operating rules | "Never use em dashes." "Always parameterize SQL." |
| **Cold** (localmem) | Queryable on demand; decisions, facts, time-sensitive context | "We picked DuckDB because of bitemporal." "Deployment is via Terraform in infra/." |

Things you say every session → hot tier (CLAUDE.md). Things the
agent needs to know when relevant → cold tier (localmem). Don't put
your whole life in hot tier, or every session burns context budget on
facts you never reference.

A localmem fact earns promotion to hot tier when it gets queried
3+ times across sessions. Most facts never need promotion.

Full rationale: [MEMORY_TIERS.md](MEMORY_TIERS.md).

---

## TL;DR — the 60-second version to send your colleague

> 1. `curl -fsSL https://localmem.org/install | sh`
> 2. `localmem init && localmem fetch-model && localmem serve &`
> 3. `localmem mcp install --client <claude|cursor|cline|windsurf|claude-code>` for each agent
> 4. Restart the AI clients.
> 5. Write your team's conventions / preferences / decisions as memories:
>    `localmem write --kind preference --content "..."`
> 6. Tell agents: "Use `session_context` at session start, then
>    `memory_search` before answering anything."
>
> You stop re-explaining things. Every agent reads from the same store.
> Your data lives in `~/.localmem/`. Nothing leaves your machine.
