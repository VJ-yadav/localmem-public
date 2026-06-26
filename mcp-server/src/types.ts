// Shared zod schemas for every MCP tool exposed by localmem-mcp.
//
// Shapes mirror SPEC.md "MCP tool surface" exactly. The same schemas are
// reused by the tool registrations (input validation) and by the typed
// HTTP client (response parsing), so a drift between client and server
// surfaces at compile time, not at runtime.

import { z } from "zod";

// ---- Inputs ----------------------------------------------------------------

export const WriteInput = z.object({
  content: z.string().min(1),
  source: z.string().optional(),
  kind: z.string().optional(),
  // RFC3339 instant the memory actually occurred. Lets an agent import a
  // user's history (past chats, exported logs) at its real valid-time instead
  // of capture-now, which is what makes temporal recall correct. Forwarded to
  // the core /write; the core validates the format.
  as_of: z.string().optional(),
});
export type WriteInput = z.infer<typeof WriteInput>;

export const SearchInput = z.object({
  query: z.string().min(1),
  k: z.number().int().positive().max(100).optional(),
  at_time: z.string().optional(),
  // Project scope (trust boundary, SPEC §2.8). By default the search is scoped
  // to the current project (the MCP server's working directory) plus global
  // user-common memory, so a query never pulls another project's content.
  // `all_projects=true` searches everything; `project="<label>"` scopes to a
  // named project instead of the cwd. The two are mutually exclusive;
  // all_projects wins if both are set.
  all_projects: z.boolean().optional(),
  project: z.string().optional(),
});
export type SearchInput = z.infer<typeof SearchInput>;

export const RecallInput = z.object({
  entity: z.string().min(1),
  at_time: z.string().optional(),
  // Like memory_search: scoped to the current project + global by default;
  // all_projects=true pulls facts about the entity from every project.
  all_projects: z.boolean().optional(),
});
export type RecallInput = z.infer<typeof RecallInput>;

export const ProfileInput = z.object({
  // Subject filter (one entity). NOT the project scope.
  scope: z.string().optional(),
  // Scoped to the current project + global by default; all_projects=true
  // synthesizes across every project.
  all_projects: z.boolean().optional(),
});
export type ProfileInput = z.infer<typeof ProfileInput>;

export const ForgetInput = z.object({
  target_id: z.string().optional(),
  criteria: z.record(z.unknown()).optional(),
});
export type ForgetInput = z.infer<typeof ForgetInput>;

export const JournalInput = z.object({
  since: z.string().optional(),
  action: z.string().optional(),
});
export type JournalInput = z.infer<typeof JournalInput>;

export const GetInput = z.object({
  event_id: z.string().min(1),
});
export type GetInput = z.infer<typeof GetInput>;

// ---- Responses -------------------------------------------------------------

const WriteResponse = z.object({
  ok: z.literal(true),
  event_id: z.string(),
  action: z.enum(["COMMIT", "UPDATE", "DEDUP", "SKIP", "FORGET"]),
  facts_extracted: z.number().int().nonnegative(),
});
export type WriteResponse = z.infer<typeof WriteResponse>;

const SearchResult = z.object({
  fact: z.string(),
  score: z.number(),
  sources: z.array(z.string()),
  valid_from: z.string().optional(),
  valid_to: z.string().optional(),
});
const SearchResponse = z.object({
  ok: z.literal(true),
  results: z.array(SearchResult),
  // North Star (§2.9): the REAL token cost of the returned context, so the agent
  // sees "this recall = N tokens" instead of dumping its whole history. cost_usd
  // is present when the accounting model is priced.
  tokens: z.number().int().nonnegative().optional(),
  token_model: z.string().optional(),
  tokens_exact: z.boolean().optional(),
  cost_usd: z.number().nonnegative().optional(),
});
export type SearchResponse = z.infer<typeof SearchResponse>;

const RecallFact = z.object({
  predicate: z.string(),
  object: z.string(),
  valid_from: z.string(),
  valid_to: z.string().optional(),
  sources: z.array(z.string()),
});
const RecallResponse = z.object({
  ok: z.literal(true),
  facts: z.array(RecallFact),
});
export type RecallResponse = z.infer<typeof RecallResponse>;

const ProfileResponse = z.object({
  ok: z.literal(true),
  profile_md: z.string(),
  generated_at: z.string(),
  fact_count: z.number().int().nonnegative(),
});
export type ProfileResponse = z.infer<typeof ProfileResponse>;

const ForgetResponse = z.object({
  ok: z.literal(true),
  forgotten_event_ids: z.array(z.string()),
});
export type ForgetResponse = z.infer<typeof ForgetResponse>;

const JournalEntry = z.object({
  ts: z.string(),
  action: z.string(),
  rule: z.string(),
  input_id: z.string(),
  reasoning: z.string().optional(),
});
const JournalResponse = z.object({
  ok: z.literal(true),
  entries: z.array(JournalEntry),
});
export type JournalResponse = z.infer<typeof JournalResponse>;

// memory_get: expand a hit (event_id) into the FULL memory + its understanding.
const UnderstandingView = z.object({
  summary: z.string(),
  intent: z.string(),
  entities: z.array(z.object({ name: z.string(), kind: z.string() })),
  references: z.array(z.string()),
  salience: z.string(),
});
const GetResponse = z.object({
  ok: z.literal(true),
  event_id: z.string(),
  found: z.boolean(),
  content: z.string().optional(),
  valid_from: z.string().optional(),
  understanding: UnderstandingView.optional(),
});
export type GetResponse = z.infer<typeof GetResponse>;

// memory_status: health + decomposition backlog. Mirrors the core `/stats`
// shape; zod strips the dashboard-only fields we don't surface to the agent.
const StatusResponse = z.object({
  ok: z.literal(true),
  events: z.number().int().nonnegative(),
  captures: z.number().int().nonnegative(),
  understandings: z.number().int().nonnegative(),
  subjects: z.number().int().nonnegative(),
  entities: z.number().int().nonnegative(),
  coverage: z.object({
    decomposed: z.number().int().nonnegative(),
    signal: z.number().int().nonnegative(),
    percent: z.number().int().nonnegative(),
  }),
  understanding: z.object({
    enabled: z.boolean(),
    provider: z.string().nullable().optional(),
    model: z.string().nullable().optional(),
  }),
});
export type StatusResponse = z.infer<typeof StatusResponse>;

// ---- Resources (T-54) ------------------------------------------------------

export const ResourceProfileResponse = z.object({
  ok: z.literal(true),
  profile_md: z.string(),
  generated_at: z.string(),
  fact_count: z.number().int().nonnegative(),
});
export type ResourceProfileResponse = z.infer<typeof ResourceProfileResponse>;

export const ResourceSubjectsResponse = z.object({
  ok: z.literal(true),
  subjects: z.array(
    z.object({ subject: z.string(), count: z.number().int().nonnegative() }),
  ),
});
export type ResourceSubjectsResponse = z.infer<typeof ResourceSubjectsResponse>;

export const ResourceTagsResponse = z.object({
  ok: z.literal(true),
  tags: z.array(
    z.object({
      key: z.string(),
      value: z.string(),
      count: z.number().int().nonnegative(),
    }),
  ),
});
export type ResourceTagsResponse = z.infer<typeof ResourceTagsResponse>;

// P8 (§8): the shared onboarding snapshot, surfaced in-IDE so a user knows they
// are set up and what to do next (dashboard URL, import, next steps).
export const ResourceGettingStartedResponse = z.object({
  ok: z.literal(true),
  dashboard_url: z.string(),
  ready: z.boolean(),
  checks: z.array(
    z.object({
      key: z.string(),
      label: z.string(),
      ok: z.boolean(),
      required: z.boolean(),
      detail: z.string(),
      fix: z.string().optional(),
    }),
  ),
  clients: z.array(
    z.object({
      slug: z.string(),
      label: z.string(),
      wired: z.boolean(),
      command: z.string(),
    }),
  ),
  import_candidates: z.number().int().nonnegative(),
  markdown: z.string(),
});
export type ResourceGettingStartedResponse = z.infer<typeof ResourceGettingStartedResponse>;

export const ResourceRecentResponse = z.object({
  ok: z.literal(true),
  captures: z.array(
    z.object({
      event_id: z.string(),
      ts: z.string(),
      text: z.string(),
      kind: z.string(),
      tags: z.record(z.string(), z.string()).optional(),
      source_app: z.string(),
    }),
  ),
});
export type ResourceRecentResponse = z.infer<typeof ResourceRecentResponse>;

// ---- Error envelope --------------------------------------------------------

export const ErrorEnvelope = z.object({
  ok: z.literal(false),
  error: z.object({
    code: z.string(),
    message: z.string(),
  }),
});
export type ErrorEnvelope = z.infer<typeof ErrorEnvelope>;

// Bundle so the client can dispatch a parse based on `ok`.
export const Responses = {
  Write: WriteResponse,
  Search: SearchResponse,
  Recall: RecallResponse,
  Profile: ProfileResponse,
  Forget: ForgetResponse,
  Journal: JournalResponse,
  Get: GetResponse,
  Status: StatusResponse,
};
