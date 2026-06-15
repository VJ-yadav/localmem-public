# Quickstart — 5 minutes to first capture

You have a working memory layer in five minutes. We'll install, wire
it into one MCP client, and verify the round-trip.

## Step 1: Install the binary (60 seconds)

```bash
curl -fsSL https://localmem.org/install | sh
```

What this does: detects your platform (macOS arm64 / x86_64, Linux
x86_64 / aarch64), downloads the right prebuilt binary from the
latest GitHub release, verifies its SHA-256, drops it at
`/usr/local/bin/localmem`. Apache-2.0, single static Rust binary, no
dependencies.

> **macOS Gatekeeper note:** Until Apple Developer ID notarization
> lands (in progress), macOS may quarantine the binary on first run.
> If you see "cannot be opened because it is from an unidentified
> developer," run: `xattr -d com.apple.quarantine /usr/local/bin/localmem`

Verify:

```bash
localmem --version
# localmem 0.2.x
```

## Step 2: Initialize + fetch the embedder (45 seconds)

```bash
localmem init
localmem fetch-model
```

`init` creates `~/.localmem/` with the event log, config, and the
empty derived stores. `fetch-model` pulls the BGE-small embedder
(~44 MB) so the hybrid retriever has real vectors instead of
lex-only fallback.

## Step 3: Start the daemon (10 seconds)

```bash
localmem serve &
```

This binds the local HTTP server on `127.0.0.1:7788`. Every MCP
client talks to it. Leave it running; on macOS / Linux you can
also use `localmem service install` to auto-start on login.

Verify:

```bash
curl -s http://127.0.0.1:7788/health
# {"ok":true}
```

## Step 4: Wire it into one MCP client (30 seconds)

Pick the client you actually use. Each command edits *that* client's
MCP config to register localmem; nothing else changes.

```bash
localmem mcp install --client claude         # Claude Desktop
localmem mcp install --client claude-code    # Claude Code CLI
localmem mcp install --client cursor         # Cursor
localmem mcp install --client windsurf       # Windsurf
localmem mcp install --client cline          # Cline (VS Code)
```

Restart the client (Claude Desktop / Cursor / etc.) so it re-reads
the MCP config and connects to localmem.

Verify the wiring landed:

```bash
localmem mcp list
# client         status           config_path
# claude         installed        ~/Library/Application Support/Claude/...
# claude-code    installed        ~/.claude.json
# cursor         not_configured   ~/Library/Application Support/Cursor/...
# ...
```

## Step 5: First capture + recall (90 seconds)

Open the client you just wired. Tell it to remember something:

> Use the `memory_write` tool to remember: I prefer functional Rust over
> imperative C++ for long sessions.

The client will call `memory_write`, the daemon writes the event,
extracts a fact (`user prefers functional Rust over imperative C++`),
and you'll see it confirmed.

Now, in the *same chat or a different one*, ask:

> What do you know about my coding preferences?

The client calls `memory_recall(entity="user")` or reads
`localmem://profile`, gets the fact back, and answers using it.

Verify from the CLI:

```bash
localmem profile
```

You should see a markdown profile with your preference under
`## Preferences`.

## What just happened

1. `memory_write` appended an event to `~/.localmem/events.jsonl`
   (the source of truth)
2. The rule-based extractor saw "I prefer X" and emitted a fact
   `(user, prefers, "functional Rust over imperative C++")`
3. The lex index (Tantivy), vector index (LanceDB + BGE-small), and
   facts table (DuckDB) all got the new entry
4. The journal recorded a policy decision (`action=COMMIT,
   rule=high_signal`)
5. When you asked the recall question, the hybrid retriever found
   the fact and the MCP server returned it via the
   `localmem://profile` resource or the `memory_recall` tool

Want to see every step?

```bash
localmem journal --since 1h
```

Every policy decision is logged with a reason. Nothing is hidden.

## Common next steps

| You want to | Do |
|---|---|
| Wire localmem into a second MCP client | `localmem mcp install --client <other>` |
| Import your ChatGPT or Claude export | `localmem import-wizard` (auto-detects in ~/Downloads) |
| See everything in your memory | open the dashboard at http://127.0.0.1:8088 (run `localmem serve --dashboard` if not auto-started) |
| Add a tag to a memory | use `--tags key=value` on write: `memory_write content="X" tags={"project": "localmem"}` |
| Trace a fact to its source | `localmem audit <fact-id>` |
| Export everything | `localmem export ~/Downloads/my-memory.tar.gz` |
| Tear it down | `rm -rf ~/.localmem` (everything was on your disk; nothing to clean up elsewhere) |

## Troubleshooting

```bash
localmem doctor
```

Runs all the checks. PASS / WARN / FAIL per item with one-line fixes.

If `doctor` reports a FAIL: paste the output to the
[Discussions](https://github.com/VJ-yadav/localmem-community/discussions)
or open an [Issue](https://github.com/VJ-yadav/localmem-community/issues).

## What you didn't have to do

- Sign up for an account
- Get an API key
- Connect to anyone's cloud
- Agree to anyone's terms of service beyond Apache-2.0
- Worry about per-call billing or rate limits

Your memory is a file on your disk. We're not a SaaS, we don't see
your data, and we never will (unless you opt into the future paid
Localmem Cloud tier).

## Where to go from here

- [WHY_LOCALMEM.md](WHY_LOCALMEM.md) — how it compares to mem0,
  Memento, agentmemory, and just MEMORY.md files
- [INSTALL.md](INSTALL.md) — deep-dive install for each platform,
  air-gapped deployment, building from source
- [HOW_IT_WORKS.md](HOW_IT_WORKS.md) — the event log + derived stores
  + hybrid retriever, with diagrams
- [MIGRATING.md](MIGRATING.md) — import from mem0, Memento, or your
  existing MEMORY.md files
- [FOR_TEAMS.md](FOR_TEAMS.md) — using localmem at a startup or
  small-to-mid-size company (and what changes if you outgrow it)
