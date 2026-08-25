# Per-client install — every supported MCP client

This is the deep-dive companion to `QUICKSTART.md`. One section per
client, with the exact config file edits localmem makes, troubleshooting
for that client's specific quirks, and uninstall steps.

Supported clients (Community Edition):

- [Claude Desktop](#claude-desktop)
- [Claude Code](#claude-code)
- [Cursor](#cursor)
- [Windsurf](#windsurf)
- [Cline (VS Code)](#cline-vs-code)
- [Codex](#codex)

Auto-install for **Aider** is reported as not-yet-supported. It can
still be wired manually if you use an MCP-capable Aider extension.

---

## Common prerequisite

You need a working localmem install + the daemon running.

```bash
# Once
curl -fsSL https://localmem.org/install | sh
localmem init
localmem fetch-model

# Every boot (or set up `localmem service install`)
localmem serve &
```

Check the daemon is up:

```bash
curl -s http://127.0.0.1:7788/health
# {"ok":true}
```

---

## Claude Desktop

**Config file** (macOS): `~/Library/Application Support/Claude/claude_desktop_config.json`
**Config file** (Linux): `~/.config/Claude/claude_desktop_config.json`
**Config file** (Windows): `%APPDATA%\Claude\claude_desktop_config.json`

### Install

```bash
localmem mcp install --client claude
```

What this writes to your config:

```jsonc
{
  "mcpServers": {
    "localmem": {
      "command": "bun",
      "args": ["/path/to/mcp-server/src/index.ts"],
      "env": {
        "LOCALMEM_CORE_URL": "http://127.0.0.1:7788"
      }
    }
  }
}
```

(Other servers you have already registered are preserved.)

### Activate

Fully quit Claude Desktop (Cmd-Q on macOS — closing the window isn't
enough; the menubar process needs to restart). Reopen.

### Verify

In any chat, ask:
> What MCP tools do you have access to?

You should see `memory_write`, `memory_search`, `memory_recall`,
`memory_profile`, `memory_forget`, `memory_journal` plus the
`localmem://` resources and the `session_context` + `summarize_tag`
prompts.

### Uninstall

```bash
localmem mcp uninstall --client claude
```

Removes only the `localmem` entry. Other servers you have registered
stay intact.

---

## Claude Code

**Config file:** `~/.claude.json` (per-user)

### Install

```bash
localmem mcp install --client claude-code
```

### Activate

Claude Code reads `.claude.json` on session start. If you have a
session running, end it (`Ctrl-D` or `/exit`) and restart with
`claude` in a new terminal.

### Verify

```text
/mcp
```

You should see `localmem` listed.

Or just ask:
> Use the localmem memory_profile tool to summarize what you know about me.

### Project-scoped vs. user-scoped

The auto-installer writes to the **user-scoped** config (`~/.claude.json`).
If you want localmem only for a specific project, edit
`.claude/mcp.json` in that project's directory manually and remove
the user-scoped entry.

### Uninstall

```bash
localmem mcp uninstall --client claude-code
```

---

## Cursor

**Config file:** `~/Library/Application Support/Cursor/User/globalStorage/mcpServers.json`
(or your platform's Cursor config dir).

### Install

```bash
localmem mcp install --client cursor
```

### Activate

Cursor needs to be fully quit (not just the window closed) and
restarted. Cursor reads MCP config on launch.

### Verify

In the Cursor chat panel, click the MCP tools indicator (a hammer
icon, typically). You should see localmem's six tools listed.

### Cursor-specific gotcha

Cursor's MCP support shipped relatively recently. If you're on
Cursor older than ~v0.42, MCP isn't available — update.

### Uninstall

```bash
localmem mcp uninstall --client cursor
```

---

## Windsurf

**Config file:** `~/Library/Application Support/Windsurf/User/globalStorage/mcpServers.json`
(or your platform's Windsurf config dir).

### Install

```bash
localmem mcp install --client windsurf
```

### Activate

Fully quit Windsurf and reopen.

### Verify

Open the Cascade panel; localmem's tools should appear in the MCP
section.

### Uninstall

```bash
localmem mcp uninstall --client windsurf
```

---

## Cline (VS Code)

Cline is a VS Code extension. MCP config lives in the extension's
storage:
`~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json`

### Install

```bash
localmem mcp install --client cline
```

### Activate

Reload the VS Code window (`Cmd-Shift-P` → "Developer: Reload Window").

### Verify

Open the Cline panel. The MCP servers list should include localmem.

### Cline-specific gotcha

Cline's MCP settings file path can vary by VS Code variant
(VS Code Insiders, Cursor-as-VS-Code, etc.). If `mcp install` reports
the path was not found, find your Cline storage manually:

```bash
find ~/Library/Application\ Support -name "cline_mcp_settings.json" 2>/dev/null
```

Then write the localmem entry manually, or open an issue with
the path so we can add detection.

### Uninstall

```bash
localmem mcp uninstall --client cline
```

---

## Codex

**Config file:** `~/.codex/config.toml`

### Install

```bash
localmem mcp install --client codex
```

The installer preserves other Codex settings and MCP servers, writes a
`[mcp_servers.localmem]` table, and carries `LOCALMEM_CORE_URL` in
`[mcp_servers.localmem.env]`. It saves the previous file as
`config.toml.localmem.bak` before changing it.

Restart Codex after installation, then ask it to list its MCP tools and
run a `memory_search` smoke test.

### Uninstall

```bash
localmem mcp uninstall --client codex
```

This removes only the `localmem` table.

---

## Manual install (any MCP client, including unsupported ones)

If your MCP client isn't in the supported list, you can still wire
localmem manually. The MCP config entry looks like this:

```jsonc
{
  "name": "localmem",
  "command": "bun",
  "args": ["/path/to/localmem/mcp-server/src/index.ts"],
  "env": {
    "LOCALMEM_CORE_URL": "http://127.0.0.1:7788"
  }
}
```

Where `/path/to/localmem/mcp-server/src/index.ts` is the path Bun
should execute. If you installed via `npx localmem-mcp` and want to
use the npm-shipped version instead of a local checkout:

```jsonc
{
  "name": "localmem",
  "command": "npx",
  "args": ["-y", "localmem-mcp"],
  "env": {
    "LOCALMEM_CORE_URL": "http://127.0.0.1:7788"
  }
}
```

Drop that into your client's MCP config and restart.

---

## Bulk wiring all clients

```bash
for client in claude claude-code cursor windsurf cline; do
  localmem mcp install --client "$client" || true
done
localmem mcp list
```

The `|| true` ignores "not configured" errors (you don't have that
client installed yet) so the loop completes. `mcp list` shows the
final state.

---

## Common failures

### "localmem core unreachable at http://127.0.0.1:7788"

The daemon isn't running. Start it: `localmem serve &`.

If you want it always-on across reboots:

```bash
localmem service install
localmem service start
localmem service status   # should print "running"
```

### "bun: command not found"

The MCP entry uses `bun` by default. Either install Bun
(`curl -fsSL https://bun.sh/install | bash`) or change the entry to
use `node` (`npx -y localmem-mcp` works on plain Node too).

### Client doesn't see the tools after restart

1. Check the daemon is up: `curl -s http://127.0.0.1:7788/health`
2. Check the MCP entry: `localmem mcp list`
3. Restart the client *completely* — not just the window. On macOS,
   Cmd-Q. On Linux, `pkill -f <client-process-name>` and reopen.
4. Some clients log MCP errors to a debug console. For Claude
   Desktop: `tail -f ~/Library/Logs/Claude/mcp*.log` and look for
   stderr from the localmem entry.

If you're still stuck, `localmem doctor` runs all the checks and
suggests fixes.

---

## What localmem never touches

- Any MCP server you registered before installing localmem
- Any config outside the named client's MCP section
- Your shell config (`.zshrc`, `.bashrc`) — localmem ships as a
  binary on `PATH`, no shell init required
- The system Python, Node, or any package manager

Reversing every change made by `localmem mcp install` is a single
command: `localmem mcp uninstall --client <name>`. We don't leave
state behind.
