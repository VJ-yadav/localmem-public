// MCP Resources for localmem (T-54).
//
// Four read-only `localmem://*` URIs that mirror SPEC_V0_2's Discovery
// API. Each resource handler fetches the live state from the core HTTP
// server (`GET /resource/*`) and returns it as a single JSON text
// content per the MCP `resources/read` contract. Subscription support
// is deferred to T-65 (end-to-end test will exercise the
// `notifications/resources/list_changed` path).
//
// The handler registry pattern mirrors `tools.ts` so adding a new
// resource is a one-line registration plus a fetcher.

import type { CoreClient } from "./client.js";
import {
  ResourceGettingStartedResponse,
  ResourceProfileResponse,
  ResourceRecentResponse,
  ResourceSubjectsResponse,
  ResourceTagsResponse,
} from "./types.js";

/// One MCP Resource. `uri` is the canonical address the client uses
/// in `resources/read`; `fetch()` returns the JSON object whose shape
/// matches the corresponding core endpoint's response.
export interface ResourceHandler {
  readonly uri: string;
  readonly name: string;
  readonly description: string;
  readonly mimeType: string;
  readonly fetch: (core: CoreClient) => Promise<unknown>;
}

export const ProfileResource: ResourceHandler = {
  uri: "localmem://profile",
  name: "profile",
  description:
    "Synthesized markdown profile of every live fact in localmem. Auto-refreshes when facts change (subscribe via resources/subscribe).",
  mimeType: "application/json",
  fetch: (core) => core.get("/resource/profile", ResourceProfileResponse),
};

export const SubjectsResource: ResourceHandler = {
  uri: "localmem://subjects",
  name: "subjects",
  description:
    "Distinct entity subjects in the facts table with row counts. The 'who/what does memory know about' surface.",
  mimeType: "application/json",
  fetch: (core) => core.get("/resource/subjects", ResourceSubjectsResponse),
};

export const TagsResource: ResourceHandler = {
  uri: "localmem://tags",
  name: "tags",
  description:
    "Container tags in use across captures, with how many memories carry each (`project=localmem` style key/value pairs).",
  mimeType: "application/json",
  fetch: (core) => core.get("/resource/tags", ResourceTagsResponse),
};

export const RecentResource: ResourceHandler = {
  uri: "localmem://recent",
  name: "recent",
  description:
    "Last 20 captures, newest first. Forgotten captures are dropped. Use ?limit=N (up to 200) on the URI to override.",
  mimeType: "application/json",
  fetch: (core) => core.get("/resource/recent", ResourceRecentResponse),
};

export const GettingStartedResource: ResourceHandler = {
  uri: "localmem://getting-started",
  name: "getting-started",
  description:
    "Onboarding: confirms localmem is set up and gives the dashboard URL + ordered next steps (bring your history, etc.). Surface this to the user on first use so they know what they have and what to do.",
  mimeType: "application/json",
  fetch: (core) => core.get("/getting-started", ResourceGettingStartedResponse),
};

/// Every resource the MCP server exposes. Order is not significant.
/// Adding a resource here is the only registration step.
export const RESOURCES: ReadonlyArray<ResourceHandler> = [
  ProfileResource,
  SubjectsResource,
  TagsResource,
  RecentResource,
  GettingStartedResource,
];

/// Resolve a `localmem://...` URI to a handler. Honors a `?limit=N`
/// query string on `localmem://recent` by routing through a one-off
/// fetcher: the URI grammar otherwise stays opaque to the rest of
/// the code.
export function resolveResource(uri: string): ((core: CoreClient) => Promise<unknown>) | undefined {
  if (uri.startsWith("localmem://recent")) {
    const q = parseQuery(uri);
    if (q.limit !== undefined) {
      const safeLimit = Math.max(0, Math.min(200, Math.floor(q.limit)));
      return (core) =>
        core.get(`/resource/recent?limit=${safeLimit}`, ResourceRecentResponse);
    }
    return RecentResource.fetch;
  }
  const handler = RESOURCES.find((r) => r.uri === uri);
  return handler?.fetch;
}

interface ParsedQuery {
  limit?: number;
}

function parseQuery(uri: string): ParsedQuery {
  const q: ParsedQuery = {};
  const idx = uri.indexOf("?");
  if (idx === -1) return q;
  const params = new URLSearchParams(uri.slice(idx + 1));
  const limit = params.get("limit");
  if (limit !== null) {
    const n = Number(limit);
    if (Number.isFinite(n)) q.limit = n;
  }
  return q;
}
