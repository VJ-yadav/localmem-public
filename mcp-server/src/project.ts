// Resolve the current project's scope key for default-scoped reads.
//
// The trust boundary (SPEC §2.8): an agent searching from inside project A
// must not pull project B's memories. The capture hook tags every memory with
// `project_path` = the full working directory (core/src/cli/hooks.rs, which
// trims a trailing slash), so scoping a search to `project_path == <our cwd>`
// plus global user-common memory is exactly the right default.
//
// The MCP server is spawned by the client (Claude Code, Cursor, ...) with its
// working directory set to the workspace root, so `process.cwd()` is the
// project. `LOCALMEM_PROJECT_PATH` overrides for clients that spawn the server
// elsewhere (or for tests).

/// The collision-proof project key for the current session: the full working
/// directory, normalized to match the `project_path` tag the hook writes
/// (trailing slashes trimmed). Empty string only if cwd is somehow blank, in
/// which case the caller should treat scope as unavailable.
export function resolveProjectPath(): string {
  const override = process.env.LOCALMEM_PROJECT_PATH?.trim();
  const raw = override && override.length > 0 ? override : process.cwd();
  return raw.replace(/\/+$/, "");
}
