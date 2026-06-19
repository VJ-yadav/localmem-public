# Changelog

Notable changes to the localmem Community Edition. Per-release detail is also on
the [GitHub Releases page](https://github.com/VJ-yadav/localmem-community/releases).

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
