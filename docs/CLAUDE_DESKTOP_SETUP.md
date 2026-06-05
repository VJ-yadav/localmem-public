# Connecting localmem to Claude Desktop

This guide hooks `localmem` into Claude Desktop via the Model Context
Protocol (MCP) so every Claude conversation can read and write your
memory layer.

## Prerequisites

1. `localmem` core binary built and on your PATH (or note the absolute
   path).
2. `localmem-mcp` binary built from `mcp-server/` (or `bun` + the
   source checked out).
3. The BGE-small ONNX assets placed under `~/.localmem/models/bge-small-en-v1.5/`
   (or anywhere `LOCALMEM_MODEL_DIR` points to). Hybrid search and
   the vector store depend on the model; without it, the system runs
   in lex+facts-only mode.

## 1. Initialize the home directory

```bash
localmem init
```

This creates `~/.localmem/` with an empty `events.jsonl`, a
`config.toml` template, the default policy, and the supporting
sub-directories (`derived/`, `policies/`, `keys/`, `cache/`,
`models/`).

## 2. Start the core HTTP server

```bash
localmem serve
```

By default the server binds `127.0.0.1:7788`. Override via
`[server].addr` in `~/.localmem/config.toml`, `LOCALMEM_SERVER_ADDR`,
or `--addr`.

Leave this process running. The MCP server connects to it for every
tool call.

## 3. Configure Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json`
(macOS) or the platform equivalent and add the `localmem` MCP server:

```json
{
  "mcpServers": {
    "localmem": {
      "command": "/absolute/path/to/localmem-mcp",
      "env": {
        "LOCALMEM_CORE_URL": "http://127.0.0.1:7788"
      }
    }
  }
}
```

If you prefer to run from source via `bun`:

```json
{
  "mcpServers": {
    "localmem": {
      "command": "bun",
      "args": ["/absolute/path/to/localmem/mcp-server/src/index.ts"],
      "env": {
        "LOCALMEM_CORE_URL": "http://127.0.0.1:7788"
      }
    }
  }
}
```

Restart Claude Desktop. The next conversation will show the six
`memory_*` tools as available.

## 4. Verify

In a new Claude Desktop conversation, ask:

> Use memory_write to remember: "I prefer functional Rust and avoid macros where possible."

Claude should call `memory_write`. Confirm the local audit trail:

```bash
localmem journal --since 1h
```

You should see an `action=COMMIT` entry for the capture you just
wrote. Search via:

```bash
localmem search "rust preferences"
```

The capture text should appear in the results.

## Troubleshooting

- **"localmem core unreachable"** on the first tool call: `localmem
  serve` is not running, or `LOCALMEM_CORE_URL` points at the wrong
  address.
- **Hybrid search returns nothing for paraphrased queries**: the BGE
  model is missing. Run with `LOCALMEM_MODEL_DIR=/path/to/model`
  pointing at a directory containing `model.onnx` and
  `tokenizer.json`, then `localmem reindex`.
- **`memory_write` fails with `LockBusy`**: a CLI `localmem write`
  is running concurrently with `localmem serve` against the same
  home. Use one path only (Claude Desktop -> MCP -> server is the
  intended production path).
- **Tools show but every call fails**: enable verbose logging with
  `RUST_LOG=localmem=debug localmem serve` and watch for the error
  code in the response envelope. The `code` field is stable.
