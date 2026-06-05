// MCP integration test.
//
// Boots the Rust core HTTP server in a tempdir, builds an in-process
// MCP server pointed at it, and drives every tool through the tool
// registry. We do NOT spawn the actual MCP stdio loop; we call
// `tool.run(input, core)` directly because that exercises the same
// validation + HTTP pipeline a real MCP client would trigger.
//
// Requires `bun` and a built `localmem` core binary on the PATH (or
// LOCALMEM_BIN pointing at it). The BGE-small model is optional;
// without it, hybrid search degrades to lex-only but all other tools
// still work.

import { spawn, type ChildProcess } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, expect, test } from "bun:test";

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";

import { buildServer, CoreClient } from "../src/index.js";
import { resolveResource, RESOURCES } from "../src/resources.js";
import { ForgetTool, JournalTool, ProfileTool, RecallTool, SearchTool, WriteTool } from "../src/tools.js";

const PORT = Number(process.env.MCP_TEST_PORT ?? 47788);
const HOST = `127.0.0.1:${PORT}`;
const BIN = process.env.LOCALMEM_BIN ?? "localmem";

let serverProc: ChildProcess | undefined;
let home: string;
let core: CoreClient;

beforeAll(async () => {
  home = await mkdtemp(join(tmpdir(), "localmem-mcp-test-"));
  // Init the home first so serve has policy + dirs.
  await runCli(["init"], home);
  // Background-spawn `localmem serve`.
  serverProc = spawn(BIN, ["serve", "--addr", HOST], {
    env: { ...process.env, LOCALMEM_HOME: home },
    stdio: ["ignore", "ignore", "ignore"],
  });
  core = new CoreClient({ baseUrl: `http://${HOST}` });
  // Wait up to 5s for /health.
  const ok = await waitForHealth(core, 5_000);
  if (!ok) {
    throw new Error(`localmem core never came up on ${HOST}`);
  }
});

afterAll(async () => {
  if (serverProc && !serverProc.killed) {
    serverProc.kill("SIGTERM");
  }
  await rm(home, { recursive: true, force: true });
});

test("memory_write commits long content and reports facts_extracted", async () => {
  const out = await WriteTool.run(
    { content: "I prefer functional Rust and avoid macros where possible." },
    core,
  );
  expect(out.ok).toBe(true);
  expect(out.action).toBe("COMMIT");
  expect(out.event_id.length).toBe(26);
  expect(out.facts_extracted).toBeGreaterThanOrEqual(1);
});

test("memory_write rejects empty content via core 400", async () => {
  await expect(WriteTool.run({ content: "" }, core)).rejects.toThrow();
});

test("memory_search returns the previously written capture", async () => {
  const out = await SearchTool.run({ query: "functional rust" }, core);
  expect(out.ok).toBe(true);
  expect(out.results.length).toBeGreaterThan(0);
});

test("memory_recall returns facts about user", async () => {
  const out = await RecallTool.run({ entity: "user" }, core);
  expect(out.ok).toBe(true);
  expect(out.facts.length).toBeGreaterThanOrEqual(1);
});

test("memory_profile contains the user subject in markdown", async () => {
  const out = await ProfileTool.run({}, core);
  expect(out.ok).toBe(true);
  expect(out.profile_md).toContain("user");
  expect(out.fact_count).toBeGreaterThanOrEqual(1);
});

test("memory_journal includes the COMMIT entry from the write", async () => {
  const out = await JournalTool.run({ since: "1h" }, core);
  expect(out.ok).toBe(true);
  const commit = out.entries.find((e) => e.action === "COMMIT");
  expect(commit).toBeDefined();
});

// ---- T-54: MCP Resources -------------------------------------------------

test("resources registry advertises four localmem:// URIs", () => {
  const uris = RESOURCES.map((r) => r.uri).sort();
  expect(uris).toEqual([
    "localmem://profile",
    "localmem://recent",
    "localmem://subjects",
    "localmem://tags",
  ]);
});

test("resource localmem://profile returns synthesized markdown", async () => {
  const fetcher = resolveResource("localmem://profile");
  expect(fetcher).toBeDefined();
  const out = (await fetcher!(core)) as { ok: boolean; profile_md: string; fact_count: number };
  expect(out.ok).toBe(true);
  expect(out.profile_md).toContain("# localmem profile");
  expect(out.fact_count).toBeGreaterThanOrEqual(1);
});

test("resource localmem://subjects returns the entity rollup", async () => {
  const fetcher = resolveResource("localmem://subjects");
  const out = (await fetcher!(core)) as { ok: boolean; subjects: { subject: string; count: number }[] };
  expect(out.ok).toBe(true);
  expect(out.subjects.length).toBeGreaterThanOrEqual(1);
  expect(out.subjects[0]?.subject).toBe("user");
});

test("resource localmem://tags is empty when no tagged captures", async () => {
  const fetcher = resolveResource("localmem://tags");
  const out = (await fetcher!(core)) as { ok: boolean; tags: { key: string; value: string; count: number }[] };
  expect(out.ok).toBe(true);
  // The integration test above wrote no tagged captures, so tags is
  // empty. (If a future test writes tagged content this assertion
  // should weaken.)
  expect(out.tags).toEqual([]);
});

test("resource localmem://recent returns the latest captures newest-first", async () => {
  const fetcher = resolveResource("localmem://recent");
  const out = (await fetcher!(core)) as { ok: boolean; captures: { text: string; event_id: string }[] };
  expect(out.ok).toBe(true);
  expect(out.captures.length).toBeGreaterThanOrEqual(1);
  expect(out.captures[0]?.text.length).toBeGreaterThan(0);
});

test("resource localmem://recent honors ?limit= override", async () => {
  const fetcher = resolveResource("localmem://recent?limit=1");
  const out = (await fetcher!(core)) as { ok: boolean; captures: unknown[] };
  expect(out.ok).toBe(true);
  expect(out.captures.length).toBeLessThanOrEqual(1);
});

test("resolveResource returns undefined for an unknown uri", () => {
  expect(resolveResource("localmem://nope")).toBeUndefined();
});

test("memory_forget retires by criteria and journals the operation", async () => {
  const out = await ForgetTool.run(
    { criteria: { subject: "user", predicate: "prefers" } },
    core,
  );
  expect(out.ok).toBe(true);
  expect(out.forgotten_event_ids.length).toBeGreaterThanOrEqual(1);
  // After forget, recall at-now hides the retired fact.
  const after = await RecallTool.run(
    { entity: "user", at_time: new Date().toISOString() },
    core,
  );
  const stillThere = after.facts.find((f) => f.predicate === "prefers");
  expect(stillThere).toBeUndefined();
});

// ---- T-65: end-to-end MCP Resources over the real protocol ---------------
//
// The earlier "resource localmem://..." tests above call the resource
// fetchers directly (`resolveResource(uri)(core)`). T-65 exercises the
// FULL stack: an MCP `Client` connected via `InMemoryTransport` to the
// `Server` `buildServer` returns, going through the MCP SDK's JSON-RPC
// `resources/list` + `resources/read` requests. This is the contract a
// real Claude/Cursor client uses; the direct-call tests cover the
// handler logic, T-65 covers the protocol wiring.

test("MCP resources/list returns all four localmem URIs over the protocol", async () => {
  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);
      const out = await client.listResources();
      const uris = out.resources.map((r: { uri: string }) => r.uri).sort();
      expect(uris).toEqual([
        "localmem://profile",
        "localmem://recent",
        "localmem://subjects",
        "localmem://tags",
      ]);
      // Each resource carries a name, description, mimeType.
      for (const r of out.resources) {
        expect(r.name?.length).toBeGreaterThan(0);
        expect(r.description?.length).toBeGreaterThan(0);
        expect(r.mimeType).toBe("application/json");
      }
      await client.close();
    })(),
  ]);
});

test("MCP resources/read returns live state for localmem://profile", async () => {
  // Seed some content so profile has facts to render.
  await WriteTool.run(
    { content: "I prefer functional Rust over imperative C++ in long sessions." },
    core,
  );

  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);
      const out = await client.readResource({ uri: "localmem://profile" });
      expect(out.contents.length).toBe(1);
      const content = out.contents[0];
      expect(content.uri).toBe("localmem://profile");
      expect(content.mimeType).toBe("application/json");
      const parsed = JSON.parse(content.text as string);
      expect(parsed.ok).toBe(true);
      expect(typeof parsed.profile_md).toBe("string");
      expect(parsed.profile_md.length).toBeGreaterThan(0);
      expect(typeof parsed.fact_count).toBe("number");
      await client.close();
    })(),
  ]);
});

test("MCP resources/read returns subjects + tags + recent over the protocol", async () => {
  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);

      const subjects = await client.readResource({ uri: "localmem://subjects" });
      const subjectsBody = JSON.parse(subjects.contents[0].text as string);
      expect(subjectsBody.ok).toBe(true);
      expect(Array.isArray(subjectsBody.subjects)).toBe(true);

      const tags = await client.readResource({ uri: "localmem://tags" });
      const tagsBody = JSON.parse(tags.contents[0].text as string);
      expect(tagsBody.ok).toBe(true);
      expect(Array.isArray(tagsBody.tags)).toBe(true);

      const recent = await client.readResource({ uri: "localmem://recent" });
      const recentBody = JSON.parse(recent.contents[0].text as string);
      expect(recentBody.ok).toBe(true);
      expect(Array.isArray(recentBody.captures)).toBe(true);

      await client.close();
    })(),
  ]);
});

test("MCP resources/read honors ?limit= on localmem://recent over the protocol", async () => {
  // Seed multiple captures so the limit clause is meaningful.
  for (let i = 0; i < 3; i++) {
    await WriteTool.run(
      { content: `e2e t-65 seed capture number ${i} with some long content to commit it.` },
      core,
    );
  }

  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);
      const out = await client.readResource({ uri: "localmem://recent?limit=1" });
      const body = JSON.parse(out.contents[0].text as string);
      expect(body.ok).toBe(true);
      expect(body.captures.length).toBeLessThanOrEqual(1);
      await client.close();
    })(),
  ]);
});

test("MCP resources/read errors on an unknown localmem:// uri over the protocol", async () => {
  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);
      await expect(
        client.readResource({ uri: "localmem://nope-not-a-real-resource" }),
      ).rejects.toThrow();
      await client.close();
    })(),
  ]);
});

test("MCP server advertises the resources capability on initialize", async () => {
  // The server's `capabilities: { tools: {}, resources: {} }` must be
  // reflected back to the client via the initialize handshake. Without
  // this, a real Claude/Cursor client won't even attempt resources/*.
  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);
      const caps = client.getServerCapabilities();
      expect(caps?.resources).toBeDefined();
      expect(caps?.tools).toBeDefined();
      expect(caps?.prompts).toBeDefined();
      await client.close();
    })(),
  ]);
});

// ---- T-64: MCP Prompts (session_context + summarize_tag) ----------------
//
// Round-trips prompts/list and prompts/get through the same in-process
// Client + InMemoryTransport rig as the T-65 resource tests. The two
// prompts render server-side against live memory state; the assertions
// here cover the descriptor surface, the rendered text, and the bad-input
// path for `summarize_tag` (missing arg, malformed `tag`).

test("MCP prompts/list advertises session_context + summarize_tag", async () => {
  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);
      const out = await client.listPrompts();
      const names = out.prompts.map((p: { name: string }) => p.name).sort();
      expect(names).toEqual(["session_context", "summarize_tag"]);

      const session = out.prompts.find((p) => p.name === "session_context");
      expect(session?.description?.length).toBeGreaterThan(0);
      expect(session?.arguments ?? []).toEqual([]);

      const sumtag = out.prompts.find((p) => p.name === "summarize_tag");
      expect(sumtag?.arguments?.length).toBe(1);
      expect(sumtag?.arguments?.[0]?.name).toBe("tag");
      expect(sumtag?.arguments?.[0]?.required).toBe(true);

      await client.close();
    })(),
  ]);
});

test("MCP prompts/get session_context renders a markdown brief over live state", async () => {
  // Seed a couple of captures with project + topic tags so the brief
  // has something to render in the "Active projects" + "Recent context"
  // sections.
  await WriteTool.run(
    {
      content:
        "Onboarding doc for the StudentHousing project lives at docs/onboarding.md.",
    },
    core,
  );

  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);
      const out = await client.getPrompt({ name: "session_context" });
      expect(out.description?.length).toBeGreaterThan(0);
      expect(out.messages.length).toBe(1);
      const msg = out.messages[0]!;
      expect(msg.role).toBe("user");
      expect(msg.content.type).toBe("text");
      const text = msg.content.text as string;
      expect(text).toContain("# Session context");
      expect(text).toContain("## Active projects");
      expect(text).toContain("## Recent context");
      await client.close();
    })(),
  ]);
});

test("MCP prompts/get summarize_tag renders a brief for a specific tag", async () => {
  // Seed two captures so the tag-filtered profile has facts to render.
  await WriteTool.run(
    { content: "I prefer functional Rust on long sessions for the localmem core." },
    core,
  );

  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);
      // user=vijay or similar is not the tag key in this test scaffolding;
      // we just need a tag that resolves to a known set. The test core has
      // no per-capture tag writes here (CoreClient.post("/write", ...) takes
      // content only via the v0.1 WriteTool surface), so we exercise the
      // "no memories yet" branch as well as the well-formed key=value path.
      const out = await client.getPrompt({
        name: "summarize_tag",
        arguments: { tag: "project=localmem" },
      });
      expect(out.messages.length).toBe(1);
      const text = out.messages[0]!.content.text as string;
      expect(text).toContain("# Summary for `project=localmem`");
      // Either the synthesized profile lands, or we hit the empty branch.
      // Both paths render a deterministic header, so we just need the
      // title + the footer cite to be present.
      expect(text).toMatch(/Synthesized from \d+ (fact|facts) at /);
      await client.close();
    })(),
  ]);
});

test("MCP prompts/get summarize_tag rejects a missing or malformed tag", async () => {
  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);

      // Missing `tag` argument.
      await expect(
        client.getPrompt({ name: "summarize_tag" }),
      ).rejects.toThrow();

      // Malformed: no `=`.
      await expect(
        client.getPrompt({
          name: "summarize_tag",
          arguments: { tag: "no_equals_sign" },
        }),
      ).rejects.toThrow();

      // Malformed: empty key.
      await expect(
        client.getPrompt({
          name: "summarize_tag",
          arguments: { tag: "=value" },
        }),
      ).rejects.toThrow();

      await client.close();
    })(),
  ]);
});

test("MCP prompts/get errors cleanly on an unknown prompt name", async () => {
  const server = buildServer(core);
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    (async () => {
      const client = new Client(
        { name: "localmem-test", version: "0.0.1" },
        { capabilities: {} },
      );
      await client.connect(clientTransport);
      await expect(
        client.getPrompt({ name: "not_a_real_prompt" }),
      ).rejects.toThrow();
      await client.close();
    })(),
  ]);
});

// ---- Helpers ---------------------------------------------------------------

function runCli(args: string[], home: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const p = spawn(BIN, args, {
      env: { ...process.env, LOCALMEM_HOME: home },
      stdio: ["ignore", "inherit", "inherit"],
    });
    p.on("exit", (code) => {
      if (code === 0) resolve();
      else reject(new Error(`localmem ${args.join(" ")} exit ${code}`));
    });
  });
}

async function waitForHealth(client: CoreClient, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await client.health()) return true;
    await sleep(100);
  }
  return false;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
