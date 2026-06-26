// MCP tool definitions for localmem.
//
// One entry per SPEC.md MCP tool: memory_write, memory_search, memory_recall,
// memory_profile, memory_forget, memory_journal. Each tool is a thin
// adapter that:
//   1. validates input via its zod schema (the MCP SDK does this when we
//      pass `inputSchema`, but we re-validate so a bad payload from the
//      SDK still surfaces a typed error here);
//   2. POSTs to the corresponding core HTTP endpoint via CoreClient;
//   3. returns the SPEC.md-shaped response unchanged.
//
// The tool registry pattern keeps src/index.ts trivial and lets
// integration tests iterate over `TOOLS` instead of hand-listing names.

import type { CoreClient } from "./client.js";
import { resolveProjectPath } from "./project.js";
import {
  ForgetInput,
  GetInput,
  JournalInput,
  ProfileInput,
  RecallInput,
  Responses,
  SearchInput,
  WriteInput,
  type ForgetResponse,
  type GetResponse,
  type JournalResponse,
  type ProfileResponse,
  type RecallResponse,
  type SearchResponse,
  type WriteResponse,
} from "./types.js";

export interface ToolHandler<I, R> {
  /// Name as registered on the MCP server. SPEC.md says `memory_*`.
  readonly name: string;
  /// One-line description shown by clients (Claude Desktop, etc.) so the
  /// user knows what the tool does before invoking it.
  readonly description: string;
  /// zod schema used by both the MCP SDK and our local validation. The
  /// SDK accepts a JSON-schema; we hand it the zod schema's JSON form
  /// at registration time.
  readonly inputSchema: object;
  /// Run the tool. Returns the SPEC.md-shaped response on success;
  /// throws `CoreApiError` or a plain Error otherwise.
  readonly run: (input: unknown, core: CoreClient) => Promise<R>;
  /// Internal zod validator paired with the JSON schema above.
  readonly _validator: { parse: (input: unknown) => I };
}

// Helper to bridge zod -> JSON schema understood by the MCP SDK. We
// inline the conversion rather than pull a dependency: the surfaces are
// small and stable, and zod-to-json-schema would double our build size.
function zodToJsonSchema(properties: Record<string, object>, required: string[]): object {
  return {
    type: "object",
    properties,
    required,
    additionalProperties: false,
  };
}

export const WriteTool: ToolHandler<typeof WriteInput._type, WriteResponse> = {
  name: "memory_write",
  description:
    "Append a memory (capture) to the localmem event log. Returns the event id and the policy decision.",
  inputSchema: zodToJsonSchema(
    {
      content: { type: "string", description: "The text to remember." },
      source: { type: "string", description: "App that produced this memory." },
      kind: { type: "string", description: "Optional sub-kind tag (preference, note, ...)." },
      as_of: {
        type: "string",
        description:
          "Optional RFC3339 instant the memory actually occurred (e.g. when importing past history). Defaults to now. Sets the memory's valid-time so 'how long ago' style recall is correct.",
      },
    },
    ["content"],
  ),
  _validator: WriteInput,
  async run(input: unknown, core: CoreClient): Promise<WriteResponse> {
    const parsed = WriteInput.parse(input);
    return core.post("/write", parsed, Responses.Write);
  },
};

export const SearchTool: ToolHandler<typeof SearchInput._type, SearchResponse> = {
  name: "memory_search",
  description:
    "Hybrid search (BM25 + ANN + bitemporal filter) over your memories. Scoped by " +
    "default to the CURRENT project plus global memory, so another project's content " +
    "never leaks in; pass all_projects=true to search everything.",
  inputSchema: zodToJsonSchema(
    {
      query: { type: "string", description: "Free-text query." },
      k: { type: "integer", minimum: 1, maximum: 100, description: "Max results (default 10)." },
      at_time: { type: "string", description: "RFC3339 timestamp for bitemporal recall." },
      all_projects: {
        type: "boolean",
        description:
          "Search across ALL projects. Default false: results are scoped to the current " +
          "project plus global user-common memory, so other projects never leak in.",
      },
      project: {
        type: "string",
        description:
          "Scope to a named project (its label) instead of the current working directory.",
      },
    },
    ["query"],
  ),
  _validator: SearchInput,
  async run(input: unknown, core: CoreClient): Promise<SearchResponse> {
    const { all_projects, project, ...rest } = SearchInput.parse(input);
    // Default-scope to this project + global; the absence of a scope on the
    // wire is what tells the core to search everything (all_projects).
    const body: Record<string, unknown> = { ...rest };
    if (!all_projects) {
      const named = project?.trim();
      body.scope =
        named && named.length > 0
          ? { key: "project", value: named, include_global: true }
          : { key: "project_path", value: resolveProjectPath(), include_global: true };
    }
    return core.post("/search", body, Responses.Search);
  },
};

export const RecallTool: ToolHandler<typeof RecallInput._type, RecallResponse> = {
  name: "memory_recall",
  description:
    "List facts about a named entity, optionally as-of a past instant. Scoped to " +
    "the current project plus global by default; all_projects=true recalls across all.",
  inputSchema: zodToJsonSchema(
    {
      entity: { type: "string", description: "The subject to recall facts about." },
      at_time: { type: "string", description: "RFC3339 timestamp for bitemporal recall." },
      all_projects: {
        type: "boolean",
        description: "Recall facts from ALL projects. Default false: current project + global.",
      },
    },
    ["entity"],
  ),
  _validator: RecallInput,
  async run(input: unknown, core: CoreClient): Promise<RecallResponse> {
    const { all_projects, ...rest } = RecallInput.parse(input);
    const body: Record<string, unknown> = { ...rest };
    if (!all_projects) body.project = resolveProjectPath();
    return core.post("/recall", body, Responses.Recall);
  },
};

export const ProfileTool: ToolHandler<typeof ProfileInput._type, ProfileResponse> = {
  name: "memory_profile",
  description:
    "Synthesize a markdown profile from facts. Scoped to the current project plus " +
    "global by default; all_projects=true synthesizes across every project.",
  inputSchema: zodToJsonSchema(
    {
      scope: { type: "string", description: "Restrict to a single subject." },
      all_projects: {
        type: "boolean",
        description: "Synthesize across ALL projects. Default false: current project + global.",
      },
    },
    [],
  ),
  _validator: ProfileInput,
  async run(input: unknown, core: CoreClient): Promise<ProfileResponse> {
    const { all_projects, ...rest } = ProfileInput.parse(input);
    const body: Record<string, unknown> = { ...rest };
    if (!all_projects) body.project = resolveProjectPath();
    return core.post("/profile", body, Responses.Profile);
  },
};

export const ForgetTool: ToolHandler<typeof ForgetInput._type, ForgetResponse> = {
  name: "memory_forget",
  description:
    "Soft-delete a memory by event id or by {subject, predicate} criteria. Appends a forget event.",
  inputSchema: zodToJsonSchema(
    {
      target_id: { type: "string", description: "Capture or fact event id to retire." },
      criteria: {
        type: "object",
        description: "Criteria match. v0.1: {subject, predicate}.",
        additionalProperties: true,
      },
    },
    [],
  ),
  _validator: ForgetInput,
  async run(input: unknown, core: CoreClient): Promise<ForgetResponse> {
    const parsed = ForgetInput.parse(input);
    return core.post("/forget", parsed, Responses.Forget);
  },
};

export const JournalTool: ToolHandler<typeof JournalInput._type, JournalResponse> = {
  name: "memory_journal",
  description:
    "Read the audit journal of policy decisions. Filter by `since` duration and `action`.",
  inputSchema: zodToJsonSchema(
    {
      since: {
        type: "string",
        description: "Duration window (e.g. `1h`, `1d`, `2w`). Default 1d.",
      },
      action: {
        type: "string",
        description: "Restrict to one action: COMMIT|UPDATE|DEDUP|SKIP|FORGET.",
      },
    },
    [],
  ),
  _validator: JournalInput,
  async run(input: unknown, core: CoreClient): Promise<JournalResponse> {
    const parsed = JournalInput.parse(input);
    return core.post("/journal", parsed, Responses.Journal);
  },
};

export const GetTool: ToolHandler<typeof GetInput._type, GetResponse> = {
  name: "memory_get",
  description:
    "Expand a search/recall hit into its FULL content plus understanding " +
    "(summary, intent, entities). Pass the event_id from a hit's `sources`. " +
    "Use this when a search snippet or title isn't enough and you need the whole memory.",
  inputSchema: zodToJsonSchema(
    {
      event_id: {
        type: "string",
        description:
          "The event id of the memory to expand (from a search hit's `sources`).",
      },
    },
    ["event_id"],
  ),
  _validator: GetInput,
  async run(input: unknown, core: CoreClient): Promise<GetResponse> {
    const parsed = GetInput.parse(input);
    return core.post("/get", parsed, Responses.Get);
  },
};

// Health + decomposition backlog, so the AI assistant can SEE (and tell the
// user about) memories the understanding layer has not processed yet — e.g.
// when the local LLM/Ollama is switched off to save RAM and a backlog builds
// up silently. Without this the backlog is only visible via the CLI/dashboard.
export const StatusTool: ToolHandler<Record<string, never>, unknown> = {
  name: "memory_status",
  description:
    "Health + decomposition backlog of the memory store: total memories captured, how " +
    "many the understanding layer has decomposed, how many are still UNDECOMPOSED (e.g. " +
    "because the local LLM/Ollama is off), and which backend is active. Call this to tell " +
    "the user about a backlog and offer to run `localmem understand --backfill`.",
  inputSchema: zodToJsonSchema({}, []),
  _validator: { parse: () => ({}) as Record<string, never> },
  async run(_input: unknown, core: CoreClient): Promise<unknown> {
    const s = await core.get("/stats", Responses.Status);
    const undecomposed = Math.max(0, s.coverage.signal - s.coverage.decomposed);
    const hint =
      undecomposed > 0
        ? `${undecomposed} of ${s.coverage.signal} signal memories are NOT decomposed ` +
          `(${s.coverage.percent}% understood). Start the understanding worker (a running ` +
          "localmem server + a reachable backend) and run `localmem understand --backfill`."
        : `All ${s.coverage.signal} signal memories are decomposed (100%).`;
    return { ...s, undecomposed, hint };
  },
};

/// All tools the MCP server exposes. Order is not significant; clients
/// list tools by name. Adding a tool here is the only registration step.
export const TOOLS: ReadonlyArray<ToolHandler<unknown, unknown>> = [
  WriteTool,
  SearchTool,
  RecallTool,
  ProfileTool,
  ForgetTool,
  JournalTool,
  GetTool,
  StatusTool,
] as unknown as ReadonlyArray<ToolHandler<unknown, unknown>>;
