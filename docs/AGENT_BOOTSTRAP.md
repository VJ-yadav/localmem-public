# Agent bootstrap: how to reach localmem at session start

CLAUDE.md and the harness file-memory get auto-injected into every agent
context. localmem does not, by design: it is a queryable cold tier, not a
hot tier (see [MEMORY_TIERS.md](MEMORY_TIERS.md)). The right pattern is for
the agent to **call localmem on the first turn** of every session, so the
relevant memories enter context only when needed and the budget stays
proportional to the work.

## TL;DR — recommended first move

In any MCP-aware client (Claude Code / Claude Desktop / Cursor / Cline /
Codex / Continue / Zed), the agent's first action on a new session should
be one of:

1. **Render the bootstrap prompt.** `prompts/get` for the `session_context`
   prompt. Returns a single markdown block: synthesized profile + active
   project tags + last 5 captures. Costs one round trip; gives the model
   the "what does my memory currently look like" shape.

2. **Query for the task.** `memory_search` with the user's task as the
   query, k=10. Surfaces anything the agent already knows that is
   relevant to what the user just asked.

3. **Both.** `session_context` for the orientation, then `memory_search`
   for task-specific recall. This is what the maintainer does daily.

## Wiring

The MCP server registers two prompt templates (T-64):

| Prompt | When to call | Returns |
|---|---|---|
| `session_context` | Once per session, first turn | Profile + project tags + last 5 captures |
| `summarize_tag` | When the user mentions a project/topic by name | Tag-scoped summary |

Both call the local Rust core via HTTP. Both are read-only.

The MCP `tools/list` registers six write/read tools (T-26..T-28):

| Tool | Purpose |
|---|---|
| `memory_write` | Append a capture; runs through policy, extractor, all stores |
| `memory_search` | Hybrid BM25 + ANN with optional bitemporal filter |
| `memory_recall` | Entity-centric fact retrieval |
| `memory_profile` | Synthesized markdown profile (scoped or global) |
| `memory_forget` | Soft-delete via `forget` event |
| `memory_journal` | Policy decision log |

## .mcp.json setup (project-scoped)

Drop this in any repo's `.mcp.json` to wire localmem for Claude Code
sessions in that project. The MCP server picks up the project-local
config; the localmem core uses `--home <repo>/.localmem` (per-project
memory) or the global `~/.localmem` (default).

```json
{
  "mcpServers": {
    "localmem": {
      "command": "npx",
      "args": ["-y", "localmem-mcp"],
      "env": {
        "LOCALMEM_CORE_URL": "http://127.0.0.1:7788"
      }
    }
  }
}
```

Per-project memory variant: add `"LOCALMEM_HOME": "<repo>/.localmem"` to
the `env` block. The core then routes every read and write through that
path instead of the global home.

## Verifying the wiring

```bash
# 1. Core is up
curl -fsS http://127.0.0.1:7788/healthz

# 2. MCP client lists the localmem tools
# (In Claude Code: /mcp; in Desktop: Settings → Developer → MCP)

# 3. From the agent's first turn, ask:
#    "Use session_context to brief yourself, then search for anything
#     relevant to <my task>."
```

If the agent doesn't reach for localmem on its own, the wiring is silent
but the *behavior* is opt-in. CLAUDE.md is the right place to instruct
"check localmem first" for any session in the project.

## Why this is not auto-injected

The CLAUDE.md / harness file-memory hot tier costs context on every
session whether the agent uses it or not. That's correct for a tiny set
of always-relevant facts. Inverting that for the full memory store would
blow the context budget on session start, every time, on facts the model
may never reference.

localmem is the queryable cold tier *by design*. The cost of one
`prompts/get session_context` call on every session is the right
trade-off: 1 round trip, ~200 tokens, only the surface that the agent
needs to know *exists*. Anything deeper enters context only when the
agent asks via `memory_search`.

See [MEMORY_TIERS.md](MEMORY_TIERS.md) for the full hot/cold rationale.
