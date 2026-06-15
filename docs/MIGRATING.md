# Migrating to localmem

If you've already invested in another memory tool, you don't have
to start from scratch. We import from the formats that matter.

Supported imports today:

- [ChatGPT export ZIP](#chatgpt-export-zip)
- [Claude conversation export](#claude-conversation-export)
- [`MEMORY.md` files (Claude Code-style)](#memorymd-files)
- [localmem archive](#localmem-archive)
- [Memento SQLite (planned)](#memento)
- [mem0 export (planned)](#mem0)
- [agentmemory export (planned)](#agentmemory)
- [Obsidian / Notion / Roam (planned)](#obsidian--notion--roam)

If your source isn't listed, see [Building your own importer](#building-your-own-importer).

---

## How import works (one page of context)

Every import lands in the same `events.jsonl` your normal use writes to.
Each imported memory becomes:

1. A **capture event** with the original content + source attribution
   (`source.app = "import:chatgpt"` for example)
2. A **policy decision** in the journal (COMMIT / DEDUP / SKIP based
   on the same rules as live writes)
3. **Extracted facts** in the DuckDB facts table (if the extractor
   matches patterns in the content)
4. **Embeddings** in LanceDB (vector retrieval)
5. **Lex entries** in Tantivy (BM25 retrieval)

Imports are **idempotent**: running the same import twice doesn't
duplicate. We detect by content hash.

Imports are **fully reversible**: each imported memory has a unique
event id. `localmem forget <id>` retires one. `localmem forget --criteria '{"source": "import:chatgpt"}'` retires the whole import.

---

## ChatGPT export ZIP

OpenAI's ChatGPT lets you export your full conversation history via
Settings → Data Controls → Export. They email you a ZIP file.

### Auto-detect

```bash
localmem import-wizard
# Scans ~/Downloads, ~/Desktop, and CWD for chatgpt-*.zip and similar
# Reports each detection with HIGH or LOW confidence
```

If the wizard reports HIGH-confidence ChatGPT files:

```bash
localmem import-wizard --apply
# Runs the importer for every HIGH-confidence detection
```

### Explicit

```bash
localmem import chatgpt ~/Downloads/chatgpt-export.zip
```

### What we import

- `conversations.json` (every chat thread)
- `user.json` (your account info, used for `subject = user` on facts)
- We **do not** import: `message_feedback.json` (your thumbs-up/down on
  responses), media files (we're text-only in Community Edition)

### What we skip

- System prompts (noise)
- Tool-call JSON blobs (not user-authored content)
- Conversations with no user turns (empty threads)

### Verify

```bash
localmem recent --limit 20
localmem subjects
```

You should see your ChatGPT history reflected.

---

## Claude conversation export

Claude (claude.ai) exports also produce a ZIP file via Settings →
Privacy & Personalization → Export data.

### Auto-detect / Explicit

```bash
localmem import-wizard --apply
# OR
localmem import claude ~/Downloads/claude-export.zip
```

### What we import

- Every conversation as a series of captures
- We use `source.app = "import:claude"` for traceability

---

## `MEMORY.md` files

The simplest case: Claude Code reads `MEMORY.md` files at the project
root. If you've been using that, you can carry the content over.

### Single file

```bash
cat MEMORY.md | localmem write --content-stdin --source "MEMORY.md" --tags "project=$(basename $(pwd))"
```

### All MEMORY.md files in a directory tree

```bash
find . -name "MEMORY.md" | while read f; do
  cat "$f" | localmem write --content-stdin \
    --source "MEMORY.md" \
    --tags "project=$(basename $(dirname $f))"
done
```

Each file becomes a single capture; the extractor processes the
content for facts. Tag by project so you can filter:
`localmem profile --tags project=my-app`.

> This is a starter recipe; for repeatable bulk imports we recommend
> writing a small shell script that's idempotent (skips already-seen
> hashes).

---

## localmem archive

Already a localmem user on another machine? Export there, import here.

```bash
# On machine A
localmem export ~/Downloads/my-memory-2026-06-15.tar.gz

# Transfer the file however you like (USB stick, secure file transfer,
# rclone, whatever). The archive is yours; nothing leaks anywhere.

# On machine B
localmem import archive ~/Downloads/my-memory-2026-06-15.tar.gz
```

The archive is a portable bundle: `events.jsonl` + `config.toml` +
`policies/`. Derived stores (Tantivy, LanceDB, DuckDB) are rebuilt
via `localmem replay` after import.

> If you want **continuous cross-device sync** (write on machine A,
> see it on machine B in under a minute, E2E encrypted), that's
> Localmem Cloud. The Community Edition supports archive
> export/import; Cloud handles the continuous case.

---

## Memento

[Memento](https://github.com/veerps57/memento) stores everything in a
single SQLite database (`$XDG_DATA_HOME/memento/memento.db`).

**Status:** importer planned, not yet shipped. Track at
[Issue #](https://github.com/VJ-yadav/localmem-community/issues) (open
one if you need this).

The importer will read Memento's SQLite tables, map their five kinds
(fact / preference / decision / todo / snippet) to ours (fact /
preference / decision / constraint / todo / note), preserve scope as
tags, and reconstruct the supersession chain.

If you need it now, a manual approach: use Memento's
`memento export` (their backup command) to dump everything to JSON,
then write a script that walks the JSON and calls `localmem write`
once per memory.

---

## mem0

[mem0](https://mem0.ai) is cloud-hosted, so "export" means pulling
your memories from their API.

**Status:** importer planned, not yet shipped.

Manual approach: their Python SDK has `mem0.get_all()`. Loop over
the results, call `localmem write` for each, tag by `mem0.user_id`
so you can filter later.

---

## agentmemory

[agentmemory](https://github.com/rohitg00/agentmemory) stores in
SQLite. Importer planned, not yet shipped.

---

## Obsidian / Notion / Roam

Knowledge-base tools with their own data shapes.

**Status:** importers planned, not yet shipped. These are higher-effort
because the source format isn't conversation-shaped; we'd want to
treat each note as a capture and infer kinds from front-matter and
content patterns.

If you need one of these, open a Discussion with a representative
export so we can scope the work.

---

## Building your own importer

If your source isn't supported and you want to write the importer,
the pattern is small. Look at
[`core/src/cli/import_wizard.rs`](../core/src/cli/import_wizard.rs)
and the existing `chatgpt` / `claude` paths in
[`core/src/cli/export.rs`](../core/src/cli/export.rs).

The interface is:

1. Take a path to your source file
2. Parse + enumerate memories
3. For each, build a `CapturePayload` with text, kind, tags, source
4. Append as an `Event` to `EventLog`
5. Let the policy + extractor pipeline run

Open a Discussion before sinking time into it — we may have prior art
that helps.

---

## After any import: rebuild derived stores

If the import ran a long time, you can rebuild the derived stores from
scratch:

```bash
localmem replay
```

This walks `events.jsonl` and rebuilds Tantivy, LanceDB, DuckDB, and
the journal. Useful if:

- An import failed partway through and you want to start over
- You upgraded localmem and the schema is different
- You want to verify recomputability ("does my derived state really
  rebuild from the log?")

The integrity invariant: `localmem replay` over a 1 GB `events.jsonl`
takes ~30s on an M-series Mac and produces identical state to the
original derivation.

---

## Verifying the import

```bash
# How many captures?
localmem recent --limit 200 | wc -l

# Anyone mentioned more than 5 times?
localmem subjects | head

# What tags are now in play?
localmem tags

# Synthesized brief
localmem summarize
```

If the synthesized brief reads like *you*, the import worked. If it
reads like noise, the source format may need a different importer
strategy — open a Discussion.

---

## What we won't import

- Plaintext from someone else's account (no scraping)
- Anything that requires browser automation (too brittle, too much
  surface area for breakage)
- Binary blobs without conversion (we're text-only in Community
  Edition; multi-modal lands in v0.3)

---

## What changes after migration

You're now on a memory layer where:

- The source of truth is a file on your disk
- Nothing leaves the machine unless you ask
- Every read is auditable via `localmem journal` and
  `localmem audit <fact-id>`
- You can leave any time with `localmem export` and nobody will
  notice or stop you

If anything broke during migration, open an Issue. Migration is the
single most failure-prone path in any memory tool, and we treat
import bugs as P0.
