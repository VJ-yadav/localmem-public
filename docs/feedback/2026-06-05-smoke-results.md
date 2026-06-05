# Smoke test of PR #15 (commit 3588dff) fixes

Test session 2026-06-05 by the Claude Code agent that filed the original
2026-06-04 field notes. All tests run against the release binary built
fresh from main (`core/target/release/localmem`, version 0.1.1) on macOS
arm64. Two existing homes were healed via replay before testing
(~/.localmem and /Users/vjsnapp/DATA_LAB/JPI/.localmem). One brand new
temporary home was created and discarded.

## Original P0/P1 items: all pass

### P0 schema mismatch self-heals with actionable error
Repro: `echo 0 > ~/.localmem/derived/lexical.tantivy.version; localmem search "x" --mode lex`

Output:
```
Error: lexical index schema is stale (on-disk v0, binary v1). Run: localmem replay to rebuild derived stores from events.jsonl (safe; the event log is the source of truth)
```

Exit code: 1. Recovery confirmed: `localmem replay` rebuilt 25 captures
and search returned results on the next call. This is a textbook
actionable error: it names the problem (versions), prescribes the fix
(`localmem replay`), and reassures the user it is safe (event log is the
source of truth). 

### P0 missing embedder no longer dead-ends search
- `LOCALMEM_MODEL_DIR=/nonexistent localmem search "rust"` (default mode):
  exit 0, lex results returned. No hard error.
- `LOCALMEM_MODEL_DIR=/nonexistent localmem search "rust" --mode lex`:
  exit 0, clean lex results.
- `LOCALMEM_MODEL_DIR=/nonexistent localmem search "rust" --mode vec`:
  exit 1, error message `load embedder from /nonexistent (set
  LOCALMEM_MODEL_DIR to override, or pass --mode lex)`. Including the
  `--mode lex` hint in the error is a thoughtful touch.

### P1 JSON cleanliness
`localmem search "rust" --json | jq .` produces clean parseable JSON.
Stderr does not leak into stdout (the once-per-process WARN that would
have appeared on a TTY is correctly suppressed under the non-TTY pipe
context). Shape:

```json
{
  "query": "rust",
  "mode": "hybrid",
  "hits": [
    { "event_id": "...", "snippet": "...", "score": 0.042, "sources": ["lex", "vec"] }
  ]
}
```

### P2 --content flag synonym
`localmem search --content "rust" --mode lex` works as a synonym for the
positional QUERY. Passing both forms together is rejected with clap exit
code 2 and a clear message: `error: the argument '[QUERY]' cannot be
used with '--content <CONTENT>'`. 

### P1 MCP integration discoverability
`docs/AGENT_BOOTSTRAP.md` exists, is 107 lines, and lays out the
session-start pattern (call `session_context` prompt and/or
`memory_search` on first turn). Includes a drop-in `.mcp.json` block for
project-scoped wiring and verification curls. This closes the original
complaint that the "use memory at session start" pattern was tribal
knowledge.

## Small follow-up observations (not blockers)

### F1 New-home init does not print the model-share status line
The PR description promised:

> You'll see "shared global embedder model into new home" — the model
> symlink is automatic now. No manual symlink workaround needed.

Behavior: `localmem init --home <new_temp_dir>` did populate
`models/bge-small-en-v1.5/model.onnx` and `tokenizer.json` in the new
home (the model is present and usable, so the underlying functional fix
landed). However, no friendly status line printed about it. The init
output was simply:

```
initialized localmem home at <path>
```

Suggested fix: print one extra line after init confirming the embedder
model was either symlinked, copied, or downloaded, so users know
semantic search is ready without having to inspect the filesystem.

**Update after inode check:** the populated files appear as regular
files in `ls -la` but `stat -f "ino=%i"` shows they share the same
inode as the source `~/.localmem/models/...` file (117266413 in this
test). On macOS APFS this is a `clonefile()` copy-on-write clone: each
new home gets a real file at the canonical path (no symlink-following
logic needed in the binary), but disk cost is ~0 until either side is
mutated. Confirmed by `du -sh` on the new home: 8.0K total, despite a
127MB `model.onnx` listed inside. This is the right primitive and
already in use. The only ask above stands: a status line on init so
the magic is visible to the user instead of needing a `stat` to verify.

### F2 JSON output uses `hits`, SPEC.md uses `results`
The CLI `--json` output uses an array key `hits`:

```json
{ "query": "...", "mode": "...", "hits": [...] }
```

But SPEC.md `memory_search` documents the MCP tool surface as
`results`:

```ts
output: { ok: true, results: [{ fact: string, score: number, ... }] }
```

These are different surfaces (CLI vs MCP) so there is no contract
violation. But the divergence creates discoverability friction for
agents that read SPEC.md and try to parse CLI output. Either:
- Pick one shape and use it across both surfaces, or
- Add a short "CLI JSON output" section to SPEC.md documenting the
  `hits` shape explicitly.

This is a documentation item, not a code one. Five lines in SPEC.md
would fix it.

## What was NOT re-tested

- Real MCP server invocation from inside an MCP-aware client (Claude
  Desktop / Cursor). The CLI path was the original complaint vehicle,
  so the agent re-tested that.
- `--batch` CLI flag (the PR description mentions it as future work).
- `localmem fetch-model` referenced in the PR notes; not tested.
- Performance of replay against larger event logs.

## Verdict

PR #15 lands every original blocker and a meaningful documentation fix
on top. Per-project memory via `--home` is now smoothly self-bootstrapping,
the embedder is no longer a footgun on a fresh install, and stderr no
longer poisons stdout. Two minor cosmetic items above (F1 status line,
F2 SPEC shape) can ride a follow-up bundle whenever a small docs PR
fits.
