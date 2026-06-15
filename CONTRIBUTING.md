# Contributing to localmem

Thanks for taking an interest. This repo is the **Community Edition** of
localmem, licensed Apache-2.0. Anything you land here ships to every
user of every MCP-compatible AI tool that wires up localmem. That's
both the opportunity and the responsibility.

## What we welcome

- **Bug reports.** With a reproduction. Include `localmem doctor` output
  if you can.
- **Bug fixes.** Small focused PRs are easier to review than sweeping
  changes.
- **MCP client wiring.** New clients beyond Claude Desktop / Claude Code
  / Cursor / Windsurf / Cline. The adapter pattern is in
  `core/src/cli/mcp_clients/`.
- **Extractor improvements.** Both rule-based (in `core/policies/`) and
  the Ollama LLM extractor surface (`core/src/extractor/`). User-authored
  YAML extractors (`policies/extractors/*.yaml`) are a good place to land
  domain-specific patterns without touching Rust.
- **Importers.** Tools we don't yet read from: Obsidian, Notion, Roam,
  Apple Notes export, etc. Pattern is in `core/src/cli/import_wizard.rs`.
- **Docs.** Clearer install paths, better examples, comparisons, error
  messages.
- **Tests.** We aim for every public surface to be covered.

## What needs to be discussed before you build

Open an Issue or Discussion first if you're considering any of these.
We're not closed to them; we want to make sure they fit the editions
split (`EDITIONS.md`) and the project promise (`CLAUDE.md`).

- **New event kinds in `events.jsonl`.** The event log is the source of
  truth; new event kinds have permanent compatibility implications.
- **Schema changes to derived stores** (Tantivy, LanceDB, DuckDB).
  Replay must still rebuild from `events.jsonl`.
- **New MCP tools.** We're deliberately narrow (6 tools + 4 resources +
  2 prompts). Memento ships ~26; we believe narrow is better. Be
  prepared to justify the addition.
- **Telemetry of any kind.** The project promise is **zero content
  telemetry**. Aggregate, opt-in usage signals might be OK; anything
  touching memory content is not.
- **Anything that requires a network call to complete a `memory_*`
  operation.** Cloud features are opt-in by design.

## What belongs in this repo vs. elsewhere

This repo is **Community Edition only** — Apache-2.0. Features that
target paying enterprise customers (multi-tenancy, SSO, RBAC, audit
log streaming to SIEM, BYOK encryption, SOC 2 compliance hooks,
hosted infrastructure) live in separate `localmem-enterprise` and
`localmem-cloud` repos. We are not accepting OSS contributions on
those tiers — see [EDITIONS.md](EDITIONS.md) for the full split.

## Development setup

You need:

| Tool | Why | Install |
|---|---|---|
| Rust 1.75+ | Core binary | https://rustup.rs |
| Bun (recommended) or Node 20+ | MCP server | https://bun.sh |
| `cargo` on `PATH` | Build | `source ~/.cargo/env` |

Build + test:

```bash
git clone https://github.com/VJ-yadav/localmem-community
cd localmem-community/core
cargo build --release
cargo test --release    # 499+ tests, ~60s on M-series
```

The MCP server has its own tests:

```bash
cd ../mcp-server
bun install
bun test
```

End-to-end acceptance:

```bash
cd ..
./scripts/v0_2_acceptance.sh
```

Should print `=== v0.2 acceptance: PASS ===` at the end.

## Code conventions

These are enforced via CodeRabbit on every PR — taking them seriously
saves a review round.

**Rust:**
- `anyhow::Result` for application errors, `thiserror::Error` for
  library errors. No `String` errors.
- No `panic!` in non-test code. Use `Result`.
- `tracing` macros with structured fields, not `println!` or string
  interpolation.
- No singletons for mutable state. Arc<RwLock> or a resource pool.
- No hardcoded enums or string constants in code. If it looks like
  config, it belongs in `policies/*.yaml` or `config.toml`.

**TypeScript:**
- Imports use `.js` extensions (MCP SDK convention).
- Use `zod` for runtime validation at HTTP boundaries.
- Don't pull in dependencies casually — the MCP server is
  intentionally small.

**Documentation:**
- No em dashes anywhere. Use commas, periods, or restructure.
- No emojis unless the user explicitly asks.
- Comments explain non-obvious **WHY**, not WHAT. Self-documenting code
  preferred.
- Don't create planning, decision, or analysis docs unless asked.
  Architectural changes go in `ARCHITECTURE.md`.

## The Apache-2.0 contribution discipline

By submitting a PR, you agree your contribution is licensed Apache-2.0
under the terms of the `LICENSE` file in this repo. This is the
standard inbound = outbound model (no separate CLA).

We may relicense future Community Edition versions in the unlikely
event of AWS-style appropriation (the Elastic playbook). Anything
you contribute will retain its Apache-2.0 grant on the version it
shipped in; the relicense applies forward only.

## How a review goes

1. **CodeRabbit auto-reviews on PR open.** It catches style + sometimes
   logic issues. Address what it raises; you don't need to argue every
   point.
2. **Human review (maintainer) on top.** Look for: correctness, the
   project promise (`CLAUDE.md`), and the editions split (`EDITIONS.md`). Style nits
   are CodeRabbit's job.
3. **Merge.** We squash-merge to keep `main` history clean.

Typical review turnaround: 48 hours during working week, longer on
weekends. If you don't hear back in a week, ping the PR.

## Getting help

- **Discussions:** https://github.com/VJ-yadav/localmem-community/discussions
- **Issues** (bugs only): https://github.com/VJ-yadav/localmem-community/issues
- **Architecture reference:** `ARCHITECTURE.md`, `STORY.md`

Thanks for being here.
