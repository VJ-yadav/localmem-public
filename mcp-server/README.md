# localmem-mcp

MCP (Model Context Protocol) server for [**localmem**](https://github.com/VJ-yadav/localmem-public),
the local-first AI memory layer.

This package is the thin Node adapter that exposes the localmem Rust core to
any MCP-compatible AI tool (Claude Desktop, Claude Code, Cursor, Cline,
Windsurf, etc). The core binary is what actually owns your event log and
derived stores; this package just speaks MCP and forwards calls.

## Install + wire up (60 seconds)

```bash
# 1. Install the Rust core (one-time per machine)
curl -fsSL https://localmem.org/install | sh
localmem init
localmem fetch-model   # downloads BGE-small (~44 MB) for semantic search

# 2. Wire the MCP server into your AI client (zero-config bootstrap)
npx localmem-mcp install --client claude          # Claude Desktop
npx localmem-mcp install --client claude-code     # Claude Code CLI
npx localmem-mcp install --client cursor          # Cursor
npx localmem-mcp install --client cline           # Cline (VS Code)
npx localmem-mcp install --client windsurf        # Windsurf
```

Restart the AI client. The agent can now call `memory_write`,
`memory_search`, `memory_recall`, `memory_profile`, `memory_forget`,
`memory_journal`. The `session_context` and `summarize_tag` prompts are
available via `prompts/get`. Resources at `localmem://{profile,subjects,tags,recent}`.

## What this package exposes

### Tools (6)

| Tool | Purpose |
|---|---|
| `memory_write` | Append a capture; runs policy + extraction + indexing |
| `memory_search` | Hybrid BM25 + ANN search with optional `at_time` bitemporal filter |
| `memory_recall` | Entity-centric fact view |
| `memory_profile` | Synthesized markdown profile (scope or global) |
| `memory_forget` | Soft-delete via `forget` event |
| `memory_journal` | Policy decision log |

### Prompts (2)

| Prompt | Returns |
|---|---|
| `session_context` | Synthesized profile + active project tags + last 5 captures |
| `summarize_tag` | Tag-scoped summary (arg: `tag` as `key=value`) |

### Resources (4)

| URI | Live data |
|---|---|
| `localmem://profile` | Synthesized markdown profile |
| `localmem://subjects` | Distinct subjects with counts |
| `localmem://tags` | Tags in use with counts |
| `localmem://recent?limit=N` | Last N captures |

## How it talks to the Rust core

The MCP server connects to `LOCALMEM_CORE_URL` (default `http://127.0.0.1:7788`).
Start the core HTTP daemon with:

```bash
localmem serve
```

Without the daemon, MCP calls return an "unreachable" error. The CLI works
without the daemon; the MCP server requires it.

## Per-project memory

Point `LOCALMEM_HOME` at any directory to scope memory per-project:

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

Then run `localmem serve --home /Users/you/projects/my-saas/.localmem` in a
terminal. The AI client sees only that project's memory.

## License + links

Apache-2.0. Core repo: https://github.com/VJ-yadav/localmem-public.
Full user guide: [HOW_IT_WORKS.md](https://github.com/VJ-yadav/localmem-public/blob/main/docs/HOW_IT_WORKS.md).
Architecture: [ARCHITECTURE.md](https://github.com/VJ-yadav/localmem-public/blob/main/ARCHITECTURE.md).
