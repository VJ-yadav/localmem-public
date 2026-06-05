// MCP Prompts for localmem (T-64).
//
// Two server-provided prompt templates per SPEC_V0_2 "MCP surface →
// Prompts (2)":
//
//   session_context   — Opening brief on the user + active projects +
//                       recent context. AI clients inject this at
//                       session start automatically.
//   summarize_tag     — Brief for a specific tag (key=value pair).
//                       Takes one argument `tag`.
//
// Templates render server-side against live memory state by funneling
// through the existing `/profile`, `/resource/tags`, and
// `/resource/recent` endpoints. The MCP server stays a thin adapter;
// no new core HTTP route was needed.
//
// The registry pattern mirrors `tools.ts` + `resources.ts`: each
// prompt is an object with `name`, `arguments`, a `render(args, core)`
// fetcher, and a list-time `descriptor()` accessor for `prompts/list`.

import { z } from "zod";
import type { CoreClient } from "./client.js";
import {
  ResourceRecentResponse,
  ResourceTagsResponse,
} from "./types.js";

// `/profile` request shape extended with the `tags` filter from T-51b.
// The existing `ProfileInput` zod in types.ts only exposes `scope`, so
// we declare the filtered variant locally to avoid pulling tag-filter
// semantics into the v0.1 tool schema (which a client may already
// depend on).
const TaggedProfileResponse = z.object({
  ok: z.literal(true),
  profile_md: z.string(),
  generated_at: z.string(),
  fact_count: z.number().int().nonnegative(),
});

interface PromptArgumentSpec {
  name: string;
  description: string;
  required: boolean;
}

interface PromptDescriptor {
  name: string;
  description: string;
  arguments: PromptArgumentSpec[];
}

interface PromptMessage {
  role: "user" | "assistant";
  content: { type: "text"; text: string };
}

export interface PromptResult {
  description: string;
  messages: PromptMessage[];
}

export interface PromptHandler {
  descriptor: PromptDescriptor;
  render: (args: Record<string, string>, core: CoreClient) => Promise<PromptResult>;
}

// ---- session_context -------------------------------------------------------

export const SessionContextPrompt: PromptHandler = {
  descriptor: {
    name: "session_context",
    description:
      "Opening brief for an AI session. Renders the user's profile + active project tags + last 5 captures as a single markdown context block.",
    arguments: [],
  },
  async render(_args, core): Promise<PromptResult> {
    const [profile, tags, recent] = await Promise.all([
      core.post("/profile", { scope: null, tags: {} }, TaggedProfileResponse),
      core.get("/resource/tags", ResourceTagsResponse),
      core.get("/resource/recent?limit=5", ResourceRecentResponse),
    ]);

    // Surface project tags first because they're the load-bearing scoping
    // primitive in v0.2 (per SPEC_V0_2 "container-tag model"). Other tag
    // keys still render below them as a single counted list.
    const projectTags = tags.tags
      .filter((t) => t.key === "project")
      .sort((a, b) => b.count - a.count);
    const otherTags = tags.tags
      .filter((t) => t.key !== "project")
      .sort((a, b) => b.count - a.count)
      .slice(0, 8);

    const projectSection =
      projectTags.length === 0
        ? "_no project tags yet_"
        : projectTags
            .map((t) => `- **${t.value}** (${t.count} ${plural(t.count, "memory", "memories")})`)
            .join("\n");

    const otherTagsSection =
      otherTags.length === 0
        ? ""
        : "\n\n### Other tags\n" +
          otherTags
            .map((t) => `- \`${t.key}=${t.value}\` (${t.count})`)
            .join("\n");

    const recentSection =
      recent.captures.length === 0
        ? "_no recent captures_"
        : recent.captures
            .map((c) => {
              const ts = formatShortTs(c.ts);
              const snippet = c.text.length > 200 ? c.text.slice(0, 197) + "..." : c.text;
              return `- ${ts} _(${c.kind})_ — ${snippet}`;
            })
            .join("\n");

    const text =
      `# Session context\n\n` +
      `${profile.profile_md.trim() || "_no synthesized profile yet_"}\n\n` +
      `## Active projects\n${projectSection}` +
      otherTagsSection +
      `\n\n## Recent context (last ${recent.captures.length})\n${recentSection}\n`;

    return {
      description: SessionContextPrompt.descriptor.description,
      messages: [{ role: "user", content: { type: "text", text } }],
    };
  },
};

// ---- summarize_tag ---------------------------------------------------------

export const SummarizeTagPrompt: PromptHandler = {
  descriptor: {
    name: "summarize_tag",
    description:
      "Render a markdown brief over every memory carrying the given tag (e.g. `tag=project=localmem`). Filters server-side via the /profile tag predicate.",
    arguments: [
      {
        name: "tag",
        description:
          "Tag to summarize, as `key=value` (e.g. `project=localmem`). Bare keys (`project`) are rejected; the brief is over a single key/value pair.",
        required: true,
      },
    ],
  },
  async render(args, core): Promise<PromptResult> {
    const raw = args.tag;
    if (typeof raw !== "string" || raw.length === 0) {
      throw new Error("summarize_tag requires `tag` arg in `key=value` form");
    }
    const eq = raw.indexOf("=");
    if (eq <= 0 || eq === raw.length - 1) {
      throw new Error(
        `summarize_tag: tag must be \`key=value\` (got: ${JSON.stringify(raw)})`,
      );
    }
    const key = raw.slice(0, eq).trim();
    const value = raw.slice(eq + 1).trim();
    if (!key || !value) {
      throw new Error(
        `summarize_tag: empty key or value in tag ${JSON.stringify(raw)}`,
      );
    }

    const filterTags: Record<string, string> = { [key]: value };
    const profile = await core.post(
      "/profile",
      { scope: null, tags: filterTags },
      TaggedProfileResponse,
    );

    const body =
      profile.fact_count === 0
        ? `_no memories carry tag \`${key}=${value}\` yet._`
        : profile.profile_md.trim();

    const text =
      `# Summary for \`${key}=${value}\`\n\n` +
      `${body}\n\n` +
      `_Synthesized from ${profile.fact_count} ${plural(profile.fact_count, "fact", "facts")} at ${profile.generated_at}._`;

    return {
      description: SummarizeTagPrompt.descriptor.description,
      messages: [{ role: "user", content: { type: "text", text } }],
    };
  },
};

// ---- registry --------------------------------------------------------------

export const PROMPTS: ReadonlyArray<PromptHandler> = [
  SessionContextPrompt,
  SummarizeTagPrompt,
];

export function findPrompt(name: string): PromptHandler | undefined {
  return PROMPTS.find((p) => p.descriptor.name === name);
}

// ---- helpers ---------------------------------------------------------------

function plural(n: number, singular: string, plural: string): string {
  return n === 1 ? singular : plural;
}

// Format an RFC3339 timestamp into a short `YYYY-MM-DD HH:MM` slice. We
// keep it timezone-naive in the rendered prompt; consumers needing the
// full instant can read `localmem://recent` directly.
function formatShortTs(rfc3339: string): string {
  const m = /^(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2})/.exec(rfc3339);
  return m ? `${m[1]} ${m[2]}` : rfc3339;
}
