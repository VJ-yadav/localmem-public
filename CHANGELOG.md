# Changelog

Notable changes to the localmem Community Edition. Per-release detail is also on
the [GitHub Releases page](https://github.com/VJ-yadav/localmem-community/releases).

## 0.3.5

- Fix `localmem search` failing with a DuckDB "Conflicting lock" error while the
  always-on service is running. Hybrid and vector search now route through the
  running server (the single writer), instead of opening the facts store
  in-process. Lexical search stays local (it is reader-only).
- Every other database-backed command (`forget`, `write`, `audit`, `understand`,
  `replay`) now shows a clear, actionable message when the service holds the lock,
  instead of a raw DuckDB error: use the dashboard or your MCP client, run
  `localmem search --mode lex`, or stop the service.
- Clean up `localmem --help`: removed internal task IDs and made the command
  descriptions say plainly what each command does.
- Clean-room test now exercises the "service running plus CLI" scenario, the gap
  that let the lock bug ship.

## 0.3.4

- Fix the npm package's core-install URL: `npx localmem-mcp install` now fetches
  the installer from `https://localmem.org/install` (was a stale `localmem.co`).
- Documentation and install instructions standardized on the one-command
  `localmem setup` flow and the `localmem.org` domain.
- Add `scripts/bump-version.mjs` so a release updates every version location in
  one command.

## 0.3.3

- Hybrid retrieval reranking and MMR are on by default. The cross-encoder
  reranker is fetched automatically by `localmem setup` and `localmem fetch-model`.
- `localmem doctor` gained a config-coherence gate: if rerank is enabled, the
  reranker must load and score, so a misconfigured install fails loudly instead
  of silently degrading to first-stage retrieval.
- Typed knowledge graph from the understanding layer, plus a rebuilt local
  dashboard (coverage overview, profile, project-scoped search, timeline, and an
  entity graph) served by the core on `:7788`.
- Bitemporal facts with valid-time contradiction resolution and `--at-time`
  recall.
- Prebuilt binaries for macOS arm64 and Linux x86_64/aarch64, with a two-command
  install via `curl -fsSL https://localmem.org/install | sh`.
