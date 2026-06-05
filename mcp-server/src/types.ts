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
});
export type WriteInput = z.infer<typeof WriteInput>;

export const SearchInput = z.object({
  query: z.string().min(1),
  k: z.number().int().positive().max(100).optional(),
  at_time: z.string().optional(),
});
export type SearchInput = z.infer<typeof SearchInput>;

export const RecallInput = z.object({
  entity: z.string().min(1),
  at_time: z.string().optional(),
});
export type RecallInput = z.infer<typeof RecallInput>;

export const ProfileInput = z.object({
  scope: z.string().optional(),
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
};
