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
import {
  ForgetInput,
  JournalInput,
  ProfileInput,
  RecallInput,
  Responses,
  SearchInput,
  WriteInput,
  type ForgetResponse,
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
  description: "Hybrid search (BM25 + ANN + bitemporal filter) over your memories.",
  inputSchema: zodToJsonSchema(
    {
      query: { type: "string", description: "Free-text query." },
      k: { type: "integer", minimum: 1, maximum: 100, description: "Max results (default 10)." },
      at_time: { type: "string", description: "RFC3339 timestamp for bitemporal recall." },
    },
    ["query"],
  ),
  _validator: SearchInput,
  async run(input: unknown, core: CoreClient): Promise<SearchResponse> {
    const parsed = SearchInput.parse(input);
    return core.post("/search", parsed, Responses.Search);
  },
};

export const RecallTool: ToolHandler<typeof RecallInput._type, RecallResponse> = {
  name: "memory_recall",
  description: "List facts about a named entity, optionally as-of a past instant.",
  inputSchema: zodToJsonSchema(
    {
      entity: { type: "string", description: "The subject to recall facts about." },
      at_time: { type: "string", description: "RFC3339 timestamp for bitemporal recall." },
    },
    ["entity"],
  ),
  _validator: RecallInput,
  async run(input: unknown, core: CoreClient): Promise<RecallResponse> {
    const parsed = RecallInput.parse(input);
    return core.post("/recall", parsed, Responses.Recall);
  },
};

export const ProfileTool: ToolHandler<typeof ProfileInput._type, ProfileResponse> = {
  name: "memory_profile",
  description: "Synthesize a markdown profile from all (or scoped) facts.",
  inputSchema: zodToJsonSchema(
    {
      scope: { type: "string", description: "Restrict to a single subject." },
    },
    [],
  ),
  _validator: ProfileInput,
  async run(input: unknown, core: CoreClient): Promise<ProfileResponse> {
    const parsed = ProfileInput.parse(input);
    return core.post("/profile", parsed, Responses.Profile);
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

/// All tools the MCP server exposes. Order is not significant; clients
/// list tools by name. Adding a tool here is the only registration step.
export const TOOLS: ReadonlyArray<ToolHandler<unknown, unknown>> = [
  WriteTool,
  SearchTool,
  RecallTool,
  ProfileTool,
  ForgetTool,
  JournalTool,
] as unknown as ReadonlyArray<ToolHandler<unknown, unknown>>;
