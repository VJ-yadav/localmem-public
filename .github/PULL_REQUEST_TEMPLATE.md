## What this changes

<!-- One or two sentences. Link the issue if there is one: Closes #123 -->

## Why

<!-- The motivation a reviewer needs: the non-obvious WHY behind the change. -->

## Checklist

- [ ] `cargo test` passes (and `bun test` if the MCP server changed)
- [ ] `cargo fmt` and `cargo clippy -- -D warnings` are clean
- [ ] No new network call on a `memory_*` path (local-first promise)
- [ ] No content telemetry added
- [ ] If this touches `events.jsonl` kinds or a derived store, `localmem replay` still rebuilds from the log
- [ ] Docs and comments explain WHY, not WHAT; no em dashes, no emojis
- [ ] The change fits the Community (Apache-2.0) tier, not Enterprise/Cloud (see [EDITIONS.md](../EDITIONS.md))

<!-- main is protected. This PR is reviewed by CodeRabbit and a maintainer, then squash-merged. -->
