// localmem MCP server entry point.
//
// Bridges the Model Context Protocol (stdio transport by default) to the
// local Rust core HTTP server. The transport choice is determined at
// startup: `--http :PORT` switches to HTTP for integration tests;
// otherwise we use stdio so Claude Desktop / Cursor can spawn this
// binary directly per their MCP config conventions.
//
// All real logic lives in `tools.ts` (one entry per MCP tool) and
// `client.ts` (HTTP transport to the core). This file is the wire-up.

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  type GetPromptResult,
  GetPromptRequestSchema,
  ListPromptsRequestSchema,
  ListResourcesRequestSchema,
  ListToolsRequestSchema,
  ReadResourceRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { CoreApiError, CoreClient } from "./client.js";
import { findPrompt, PROMPTS } from "./prompts.js";
import { RESOURCES, resolveResource } from "./resources.js";
import { TOOLS, type ToolHandler } from "./tools.js";

/// Build a configured MCP server. Pulled out as a factory so tests can
/// instantiate one against a mock CoreClient.
export function buildServer(core: CoreClient) {
  const server = new Server(
    { name: "localmem", version: "0.0.1" },
    { capabilities: { tools: {}, resources: {}, prompts: {} } },
  );

  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: TOOLS.map((t) => ({
      name: t.name,
      description: t.description,
      inputSchema: t.inputSchema,
    })),
  }));

  server.setRequestHandler(CallToolRequestSchema, async (req) => {
    const name = req.params.name;
    const tool = TOOLS.find((t) => t.name === name) as
      | ToolHandler<unknown, unknown>
      | undefined;
    if (!tool) {
      return errorResponse("unknown_tool", `no tool named ${name}`);
    }
    try {
      const result = await tool.run(req.params.arguments ?? {}, core);
      return {
        content: [{ type: "text", text: JSON.stringify(result) }],
      };
    } catch (err) {
      if (err instanceof CoreApiError) {
        return errorResponse(err.code, err.message);
      }
      const msg = err instanceof Error ? err.message : String(err);
      return errorResponse("tool_failed", msg);
    }
  });

  // T-54 + T-65: MCP Resources surface. `resources/list` advertises
  // the four `localmem://` URIs; `resources/read` fetches the live
  // state via the core HTTP server. End-to-end protocol coverage in
  // `mcp-server/test/integration.test.ts`. Subscribe / list_changed
  // notifications are intentionally not declared (`capabilities.resources`
  // is `{}` rather than `{ subscribe: true, listChanged: true }`):
  // wiring those needs a hook into the core event-log writer to push
  // notifications on mutation, which we defer to a later task. A
  // client that polls resources/read gets fresh state every call.
  server.setRequestHandler(ListResourcesRequestSchema, async () => ({
    resources: RESOURCES.map((r) => ({
      uri: r.uri,
      name: r.name,
      description: r.description,
      mimeType: r.mimeType,
    })),
  }));

  server.setRequestHandler(ReadResourceRequestSchema, async (req) => {
    const uri = req.params.uri;
    const fetcher = resolveResource(uri);
    if (!fetcher) {
      throw new Error(`unknown resource uri: ${uri}`);
    }
    const data = await fetcher(core);
    return {
      contents: [
        {
          uri,
          mimeType: "application/json",
          text: JSON.stringify(data),
        },
      ],
    };
  });

  // T-64: MCP Prompts surface. Two server-rendered templates per
  // SPEC_V0_2 "MCP surface → Prompts (2)". Same registry pattern as
  // tools + resources: `prompts/list` enumerates `PROMPTS`,
  // `prompts/get` dispatches to the matched handler's `render(args)`
  // which fetches live state from the core via `CoreClient`.
  server.setRequestHandler(ListPromptsRequestSchema, async () => ({
    prompts: PROMPTS.map((p) => ({
      name: p.descriptor.name,
      description: p.descriptor.description,
      arguments: p.descriptor.arguments.map((a) => ({
        name: a.name,
        description: a.description,
        required: a.required,
      })),
    })),
  }));

  server.setRequestHandler(GetPromptRequestSchema, async (req): Promise<GetPromptResult> => {
    const handler = findPrompt(req.params.name);
    if (!handler) {
      throw new Error(`unknown prompt: ${req.params.name}`);
    }
    const args = (req.params.arguments ?? {}) as Record<string, string>;
    const result = await handler.render(args, core);
    return result as unknown as GetPromptResult;
  });

  return server;
}

function errorResponse(code: string, message: string) {
  return {
    isError: true,
    content: [
      {
        type: "text",
        text: JSON.stringify({ ok: false, error: { code, message } }),
      },
    ],
  };
}

async function main() {
  // T-66: `npx localmem-mcp install --client <name>` branch. The same
  // bin entry doubles as the MCP stdio server (no args, default flow)
  // and the bootstrapper (when invoked with `install`). Routing on
  // argv keeps the npm surface small — one `bin` entry to publish.
  if (process.argv[2] === "install") {
    const { runInstall } = await import("./install.js");
    await runInstall(process.argv);
    return;
  }

  const core = new CoreClient();
  const server = buildServer(core);

  // Optional preflight: if the core is unreachable at startup we still
  // start (the user may be running `localmem-mcp` before `localmem
  // serve`), but log a hint so the first tool call's error makes sense.
  if (!(await core.health())) {
    console.error(
      "[localmem-mcp] warning: core HTTP server unreachable at startup. " +
        "Make sure `localmem serve` is running, or set LOCALMEM_CORE_URL.",
    );
  }

  const transport = new StdioServerTransport();
  await server.connect(transport);
  // The MCP SDK keeps the transport alive until stdin closes.
}

// Run main() only when this file is the entry point. Importing this
// module from a test file (to get `buildServer`) must NOT spawn a
// stdio loop. Bun runs TypeScript natively so `process.argv[1]` ends
// in `.ts` when launched directly; the compiled build ends in `.js`.
// Match both so the same entry guard works in dev and in the
// `bun build --compile` artifact.
if (
  typeof process !== "undefined" &&
  process.argv[1] &&
  (process.argv[1].endsWith("index.ts") ||
    process.argv[1].endsWith("index.js") ||
    process.argv[1].endsWith("localmem-mcp"))
) {
  main().catch((err) => {
    console.error("localmem-mcp fatal:", err);
    process.exit(1);
  });
}

export { CoreApiError, CoreClient } from "./client.js";
export { TOOLS } from "./tools.js";
