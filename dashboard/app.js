// localmem viewer, vanilla JS SPA, no framework, no build step.
// Talks to a running `localmem serve --dashboard` at /api/* (override with ?api=).

// ---- API ----------------------------------------------------------------
const API = (new URLSearchParams(location.search).get("api") || "/api").replace(/\/+$/, "");
const api = {
  async get(path) {
    const r = await fetch(API + path);
    if (!r.ok) throw new Error(`${path} → ${r.status}`);
    return r.json();
  },
  async post(path, body) {
    const r = await fetch(API + path, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body || {}),
    });
    if (!r.ok) {
      let msg = `${path} → ${r.status}`;
      try { const j = await r.json(); if (j.error?.message) msg = j.error.message; } catch (_) {}
      throw new Error(msg);
    }
    return r.json();
  },
};

// ---- tiny DOM helpers ---------------------------------------------------
const $ = (sel, root = document) => root.querySelector(sel);
function el(tag, attrs = {}, ...kids) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") n.className = v;
    else if (k === "html") n.innerHTML = v;
    else if (k.startsWith("on") && typeof v === "function") n.addEventListener(k.slice(2), v);
    else if (v !== null && v !== undefined && v !== false) n.setAttribute(k, v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    n.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return n;
}
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

function fmtWhen(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  if (isNaN(d)) return iso;
  const diff = (Date.now() - d.getTime()) / 1000;
  if (diff < 60) return "just now";
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  if (diff < 86400 * 7) return `${Math.floor(diff / 86400)}d ago`;
  return d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}
const badge = (kind) => el("span", { class: `badge ${kind}` }, kind.replace("_", " "));

// Minimal, safe markdown → HTML (## h, **bold**, - lists, paragraphs).
function mdToHtml(md) {
  const lines = String(md || "").split("\n");
  let out = "", inList = false;
  const inline = (s) => esc(s).replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>").replace(/`(.+?)`/g, "<code>$1</code>");
  for (let raw of lines) {
    const line = raw.trimEnd();
    if (/^#{1,6}\s/.test(line)) {
      if (inList) { out += "</ul>"; inList = false; }
      const lvl = line.match(/^#+/)[0].length;
      out += `<h${Math.min(lvl + 1, 6)}>${inline(line.replace(/^#+\s/, ""))}</h${Math.min(lvl + 1, 6)}>`;
    } else if (/^[-*]\s/.test(line.trim())) {
      if (!inList) { out += "<ul>"; inList = true; }
      out += `<li>${inline(line.trim().replace(/^[-*]\s/, ""))}</li>`;
    } else if (line.trim() === "") {
      if (inList) { out += "</ul>"; inList = false; }
    } else {
      if (inList) { out += "</ul>"; inList = false; }
      out += `<p>${inline(line)}</p>`;
    }
  }
  if (inList) out += "</ul>";
  return out;
}

// ---- app state ----------------------------------------------------------
const state = { tab: "overview", project: "", search: "" };
const view = $("#view");
const setView = (...nodes) => { view.replaceChildren(...nodes); };
const loadingView = (label = "Loading…") => setView(el("div", { class: "loading" }, label));
// Build the empty-state NODE (does not touch the view). `emptyView` renders it.
// Keeping them separate matters: passing emptyView() as a child to setView would
// render the `undefined` it returns as the literal text "undefined".
const emptyNode = (msg, hint) => el("div", { class: "empty" }, msg, hint ? el("span", { class: "hint" }, hint) : null);
const emptyView = (msg, hint) => setView(emptyNode(msg, hint));

// One-line "what this page is for", shown under every tab's title.
const PAGE_DESC = {
  home: "A live snapshot of everything localmem holds, how much has been understood, which model is doing it, and how to bring in your existing history.",
  memory: "Your decomposed memory: resolved entities and their beliefs, their timeline over time, the raw stream, and beliefs due for review. One place for what localmem knows.",
  trust: "The receipts: every policy decision, the write cadence, and a replay of the event log. Nothing is hidden.",
  overview: "A live snapshot of everything localmem holds, totals, how much has been understood, and which model is doing the understanding.",
  brain: "A short briefing synthesized from your understood memory, the way an assistant would catch up at the start of a session.",
  review: "Current beliefs that are the latest you said but haven't been re-confirmed in a while (past their half-life). Memory that flags when it might be out of date, so you can keep or forget each one.",
  search: "Ask your whole memory anything. Hybrid retrieval blends keyword, meaning, and the fact graph, then reranks, each hit shows why it matched.",
  memories: "The raw stream of what you've captured, newest first. Filter by kind; click any row for the full event and its decomposition.",
  activity: "How much you're writing over time, and how much of it is real signal vs ephemeral tool-traces vs understood.",
  timeline: "How a single entity's facts changed over time, current vs superseded beliefs, with bitemporal time-travel.",
  graph: "Your knowledge as a typed graph: entities are nodes (colored by kind), relations are edges. Click a node to expand its neighborhood.",
  replay: "Scrub the event log like a tape to watch memory get built, one event at a time.",
  audit: "The trust trail: every policy decision localmem made about your captures, and why. Nothing is hidden.",
  profile: "Who/what localmem knows about, resolved entities ranked by how much you've said, each with its current beliefs. Click an entity to see its timeline.",
};
const head = (title, sub, ...right) =>
  el("div", { class: "view-head" },
    el("div", { class: "view-head-main" },
      el("h1", {}, title),
      sub ? el("div", { class: "sub" }, sub) : null,
      PAGE_DESC[state.tab] ? el("div", { class: "page-desc" }, PAGE_DESC[state.tab]) : null),
    el("div", { class: "topbar-right" }, ...right));

// ---- drawer -------------------------------------------------------------
const drawer = $("#drawer"), scrim = $("#drawerScrim");
function openDrawer(title, bodyNode) {
  $("#drawerTitle").textContent = title;
  $("#drawerBody").replaceChildren(bodyNode);
  drawer.hidden = false; scrim.hidden = false;
}
function closeDrawer() { drawer.hidden = true; scrim.hidden = true; }
$("#drawerClose").addEventListener("click", closeDrawer);
scrim.addEventListener("click", closeDrawer);

function eventDrawer(ev) {
  const d = ev.detail?.payload || {};
  const kv = el("div", { class: "kv" });
  const add = (k, v) => { kv.append(el("div", { class: "k" }, k), el("div", { class: "v" }, v)); };
  add("kind", badge(ev.kind));
  add("id", el("span", { class: "mono" }, ev.id));
  add("when", new Date(ev.ts).toLocaleString());
  if (ev.project) add("project", ev.project);
  const body = el("div", {}, kv);
  if (ev.kind === "understanding") {
    if (d.summary) body.append(el("p", {}, d.summary));
    const meta = el("div", { class: "kv" });
    if (d.intent) { meta.append(el("div", { class: "k" }, "intent"), el("div", { class: "v" }, d.intent)); }
    if (d.salience) { meta.append(el("div", { class: "k" }, "salience"), el("div", { class: "v" }, badge(d.salience.replace(/\s+/g, "_")))); }
    if (d.entities?.length) { meta.append(el("div", { class: "k" }, "entities"), el("div", { class: "v" }, d.entities.map((e) => `${e.name} (${e.kind})`).join(", "))); }
    if (d.references?.length) { meta.append(el("div", { class: "k" }, "references"), el("div", { class: "v" }, ...d.references.map((r) => el("span", { class: "ref", style: "margin-right:6px" }, r)))); }
    body.append(meta);
  }
  body.append(el("div", { class: "section-label" }, "raw event"));
  body.append(el("pre", { class: "json" }, JSON.stringify(ev.detail, null, 2)));
  openDrawer(ev.title.slice(0, 60) || ev.kind, body);
}

// ---- reusable: an event row --------------------------------------------
function eventRow(ev) {
  return el("div", { class: "row" + (ev.ephemeral ? " trace" : ""), onclick: () => eventDrawer(ev) },
    el("div", { class: "row-main" },
      el("div", { class: "row-title" }, ev.title || "(empty)"),
      el("div", { class: "row-meta" },
        ev.project ? el("span", { class: "faint" }, "▸ " + ev.project) : null,
        el("span", { class: "id" }, ev.id))),
    el("div", { class: "row-side" },
      ev.ephemeral ? el("span", { class: "badge trace" }, "trace") : badge(ev.kind),
      el("span", { class: "faint", style: "font-size:12px" }, fmtWhen(ev.ts))));
}

// ---- TABS ---------------------------------------------------------------
const tabs = {};

// Onboarding: discover importable history (Claude Code / ChatGPT / Claude) and
// show how to bring it in. Read-only scan; full one-click decompose-on-import
// (with an optional OpenAI key for fast bulk processing) is the next pass.
async function importCard() {
  let scan = { candidates: [] };
  try { scan = await api.get("/import/scan"); } catch (_) {}
  const cands = scan.candidates || [];
  const card = el("div", { class: "panel import-card" },
    el("div", { class: "section-label flush" }, "get started · bring your history"),
    el("div", { class: "import-lead" }, "Import your existing AI history; localmem decomposes it into searchable, connected memory."));
  if (!cands.length) {
    card.append(
      el("div", { class: "faint" }, "No exports auto-detected. Import Claude Code history directly, or export your ChatGPT/Claude data first:"),
      el("pre", { class: "import-cmd" }, "localmem import ~/.claude/projects"));
  } else {
    cands.forEach((c) => card.append(el("div", { class: "import-found" },
      el("div", { class: "import-found-h" },
        el("span", { class: "import-fmt" }, c.format),
        el("span", { class: "faint" }, c.hint)),
      el("pre", { class: "import-cmd" }, `localmem import ${c.path}`))));
  }
  card.append(el("div", { class: "faint import-note" },
    "After importing, run localmem replay to decompose + index it. One-click import (decompose-on-import, with an optional OpenAI key for fast bulk processing) is coming next."));
  return card;
}

// Setup status strip (§8): localmem does the dependency work (model, service,
// client wiring) and reports each as a CHECK. Unchecked items show the manual
// fallback. Genuine choices (understanding, import) are toggles/actions, not
// failures. Backed by the one shared /getting-started source.
async function setupStatusCard() {
  let gs;
  try { gs = await api.get("/getting-started"); } catch (_) { return null; }
  const icon = (c) => (c.ok ? "✓" : (c.required ? "✗" : "○"));
  const rows = (gs.checks || []).map((c) => el("div", { class: "setup-check " + (c.ok ? "ok" : (c.required ? "bad" : "opt")) },
    el("span", { class: "sc-icon" }, icon(c)),
    el("span", { class: "sc-label" }, c.label),
    el("span", { class: "sc-detail faint" }, c.detail),
    (!c.ok && c.fix) ? el("code", { class: "sc-fix" }, c.fix) : null));
  const unwired = (gs.clients || []).filter((c) => !c.wired);
  const clientList = unwired.length
    ? el("details", { class: "setup-clients" },
        el("summary", { class: "faint" }, "wire another AI client"),
        ...unwired.map((c) => el("div", { class: "sc-client" },
          el("span", {}, c.label), el("code", {}, c.command))))
    : null;
  return el("div", { class: "panel setup-status" },
    el("div", { class: "section-label flush" }, gs.ready ? "setup · all systems go" : "setup · finish setup"),
    ...rows,
    clientList);
}

// North Star panel (§2.9): the real cost-of-use headline. Usage telemetry is
// global (all retrievals), so this is not project-scoped.
async function northStarCard() {
  let ns;
  try { ns = await api.get("/north-star"); } catch (_) { return null; }
  const all = ns.all_time || {};
  if (!all.retrievals) {
    return el("div", { class: "panel north-star" },
      el("div", { class: "section-label flush" }, "north star · tokens-to-correct-context"),
      el("div", { class: "faint ns-note" }, "No agent retrievals yet. When your AI agent searches its memory (via MCP), this shows the precise context it was handed and what that saved versus dumping your whole history into the model. Browsing the dashboard here does not count — it feeds no model."));
  }
  const n = (x) => (x || 0).toLocaleString();
  const usd = (x) => "$" + (x || 0).toFixed((x || 0) < 1 ? 4 : 2);
  const mult = ns.baseline_multiplier || 10;
  const d30 = ns.last_30d || {};
  const stat = (num, lab, sub) => el("div", { class: "ns-stat" },
    el("div", { class: "ns-num" }, num),
    el("div", { class: "ns-lab" }, lab),
    sub ? el("div", { class: "ns-sub faint" }, sub) : null);
  return el("div", { class: "panel north-star" },
    el("div", { class: "section-label flush" }, "north star · tokens-to-correct-context"),
    el("div", { class: "ns-lead" }, "When your agent needs context, localmem hands it a precise slice instead of dumping your history. Here is how lean that is, and what it saves."),
    el("div", { class: "ns-grid" },
      // Lead with the VALUE (the saving), not gross served.
      stat("~" + n(all.est_tokens_saved), "tokens saved vs dumping history", "estimated at " + mult + "x · ≈ " + usd(all.est_cost_saved_usd)),
      stat(n(all.tokens_served), "precise context delivered", "≈ " + usd(all.cost_usd) + " to feed gpt-4o"),
      stat(n(all.retrievals), "agent retrievals", ns.since ? "since " + ns.since.slice(0, 10) : ""),
      stat(n(d30.tokens_served), "delivered last 30d", "≈ " + usd(d30.cost_usd) + " gpt-4o input")),
    el("div", { class: "faint ns-note" }, "Token counts are real (tiktoken). The dollar figures are gpt-4o INPUT-cost equivalents of that context — NOT money spent; localmem search is free and local. Savings is an estimate at a " + mult + "x baseline until the A/B benchmark sets the measured ratio."));
}

tabs.home = async function () {
  loadingView();
  const pq = state.project ? "?project=" + encodeURIComponent(state.project) + "&include_global=false" : "";
  const [stats, act, recent] = await Promise.all([
    api.get("/stats" + pq),
    api.get("/activity" + pq).catch(() => ({ by_kind: {} })),
    api.post("/events", { limit: 12, project: state.project, include_global: false }).catch(() => ({ events: [] })),
  ]);
  const card = (num, lab, accent) => el("div", { class: "card" }, el("div", { class: "num" + (accent ? " accent" : "") }, num.toLocaleString()), el("div", { class: "lab" }, lab));
  const cards = el("div", { class: "cards" },
    card(stats.events, "events"),
    card(stats.captures, "captures"),
    card(stats.understandings, "understood", true),
    card(stats.facts, "facts"),
    card(stats.entities ?? 0, "entities"),
    card(stats.subjects, "subjects"),
    card(stats.projects, "projects"),
  );

  // Decomposition coverage + the active understanding backend. A user must be
  // able to tell from the dashboard HOW MUCH is understood and on WHICH model.
  const cov = stats.coverage || { decomposed: 0, signal: 0, percent: 0 };
  const u = stats.understanding || { enabled: false };
  const pct = cov.percent ?? 0;
  const gauge = el("div", { class: "gauge" },
    el("div", { class: "gauge-track" }, el("div", { class: "gauge-fill", style: `width:${pct}%` })),
    el("div", { class: "gauge-meta" },
      el("span", { class: "gauge-pct" }, pct + "%"),
      el("span", { class: "faint" }, `${cov.decomposed.toLocaleString()} / ${cov.signal.toLocaleString()} signal captures understood`)));
  const pending = Math.max(0, (cov.signal || 0) - (cov.decomposed || 0));
  const provider = (u.provider || "").toLowerCase();
  const isLocal = provider === "ollama" || provider === "";
  // Honest, provider-aware explanation so a user is never confused about why
  // understanding is fast/slow/idle, and knows the lever to change it.
  let note;
  if (!u.enabled) {
    note = "Understanding is off. Captures are still stored and searchable, just not decomposed into summaries, entities, and facts. Enable it in config.toml [understanding].";
  } else if (isLocal) {
    note = `Running a LOCAL model (private, free, offline). On a low-RAM machine this is slow: each capture can take minutes and runs in the background, so some may stay pending. For speed, set provider to "openai"/"anthropic" with your own API key in config.toml [understanding].`;
  } else {
    note = `Using your own ${u.provider} key (bring-your-own-key). Fast and frontier-quality; capture text is sent to ${u.provider} under your account. Fully local is available by setting provider to "ollama" (needs more RAM).`;
  }
  const backend = u.enabled
    ? el("div", { class: "backend on" },
        el("span", { class: "bdot live" }),
        el("span", {}, "Understanding on "),
        el("strong", {}, u.model || "?"),
        el("span", { class: "faint" }, ` via ${u.provider || "local"}`),
        pending ? el("span", { class: "pending-tag", title: "captures awaiting decomposition" }, `${pending.toLocaleString()} pending`) : null)
    : el("div", { class: "backend off" },
        el("span", { class: "bdot" }),
        el("span", { class: "faint" }, "Understanding offline"));
  const understanding = el("div", { class: "panel understanding" },
    el("div", { class: "section-label flush" }, "decomposition coverage"),
    gauge, backend, el("div", { class: "backend-note faint" }, note));

  const byKind = el("div", { class: "bars" }, ...kindBars(act.by_kind || {}));
  const feed = el("div", { class: "feed" }, ...(recent.events || []).map(eventRow));
  const imp = await importCard();
  const northStar = await northStarCard();
  const setupStatus = await setupStatusCard();
  setView(
    head("Home", `Everything localmem is holding for you · ${state.project || "all projects"}`),
    setupStatus || el("span"),
    cards,
    northStar || el("span"),
    understanding,
    imp,
    el("div", { class: "section-label" }, "by kind"),
    el("div", { class: "panel" }, byKind),
    el("div", { class: "section-label" }, "latest activity"),
    feed.children.length ? feed : el("div", { class: "empty" }, "No events yet."),
  );
};
tabs.overview = tabs.home; // back-compat alias

function kindBars(byKind) {
  const entries = Object.entries(byKind).sort((a, b) => b[1] - a[1]);
  const max = Math.max(1, ...entries.map((e) => e[1]));
  return entries.map(([k, n]) =>
    el("div", { class: "bar-row" },
      badge(k),
      el("div", { class: "bar-track" }, el("div", { class: "bar-fill", style: `width:${(n / max) * 100}%; background:var(--k-${k}, var(--accent))` })),
      el("div", { class: "bar-num" }, n.toLocaleString())));
}

tabs.brain = async function () {
  loadingView();
  const proj = state.project;
  const label = proj || "all projects";
  // Read the CACHED briefing, instant, no LLM. Regenerate is explicit.
  let res = {};
  try { res = await api.get("/brief/cached?project=" + encodeURIComponent(proj)); } catch (_) {}
  const md = (res.briefing_md || "").trim();

  let regenerating = false;
  const regen = el("button", { class: "btn sm primary" }, "✦ regenerate");
  regen.addEventListener("click", async () => {
    if (regenerating) return;
    regenerating = true; regen.textContent = "synthesizing…"; regen.classList.add("muted");
    try { await api.post("/brief", { project: proj }); } catch (e) { /* shown on reload */ }
    if (state.tab === "brain") tabs.brain();
  });

  if (!md || md.replace(/^##.*$/m, "").trim() === "") {
    setView(head("Brain", `Session boot briefing · ${label}`, regen),
      emptyNode(`No briefing for ${label} yet.`,
        "Click ✦ regenerate above to synthesize one from this project's memory (a few seconds). It distills your facts and understandings into a ranked session-start digest."));
    return;
  }
  setView(
    head("Brain", `Session boot briefing · ${label}`, regen),
    el("div", { class: "panel brain", html: mdToHtml(md) }),
    el("div", { class: "brief-grounding" }, "Synthesized locally from your understood memory · grounded in the source captures"),
  );
};

tabs.review = async function () {
  loadingView();
  const res = await api.post("/review", { project: state.project, include_global: false, limit: 100 });
  const items = res.items || [];
  const sub = `${items.length} of ${res.checked || 0} durable beliefs are past their half-life · ${state.project || "all"}`;
  if (!items.length) {
    return setView(head("Review", sub),
      emptyNode("Nothing to review.", "Every durable belief (decision, rule, preference, fact) is still within its freshness window. Memory is confident."));
  }
  const body = el("div", { class: "feed" });
  items.forEach((it) => {
    const pct = Math.round((it.freshness || 0) * 100);
    const row = el("div", { class: "review-item" },
      el("div", { class: "ri-main" },
        el("div", { class: "ri-claim" },
          el("span", { class: "kindtag", style: `--kc:${kindColor(it.kind)}` }, it.kind),
          el("span", { class: "ri-subj" }, it.subject),
          el("span", { class: "ri-pred" }, it.predicate),
          el("span", { class: "ri-obj" }, it.object)),
        el("div", { class: "ri-meta faint" },
          el("span", { class: "fresh-bar", title: `freshness ${pct}%` }, el("span", { class: "fresh-fill", style: `width:${pct}%` })),
          `last said ${it.age_days}d ago · ${pct}% fresh`)),
      el("div", { class: "ri-actions" }));
    const forgetBtn = el("button", { class: "btn sm", title: "drop this belief (emits a forget event)" }, "forget");
    forgetBtn.addEventListener("click", async () => {
      forgetBtn.disabled = true; forgetBtn.textContent = "forgetting…";
      try { await api.post("/forget", { target_id: it.id }); row.classList.add("done"); forgetBtn.textContent = "forgotten"; }
      catch (e) { forgetBtn.textContent = "failed"; }
    });
    row.querySelector(".ri-actions").append(forgetBtn);
    body.append(row);
  });
  setView(head("Review", sub), body);
};

tabs.memories = async function () {
  loadingView();
  const KINDS = ["all", "capture", "understanding", "fact", "update", "forget"];
  const active = state.memKind || "all";
  // Hide ephemeral tool-traces ([Bash]/[Read]/...) by default: they're real
  // captures but operational noise that otherwise drowns the signal.
  if (state.memTraces === undefined) state.memTraces = false;
  const chips = el("div", { class: "chips" }, ...KINDS.map((k) =>
    el("button", { class: "chip" + (k === active ? " on" : ""), onclick: () => { state.memKind = k; tabs.memories(); } }, k)));
  const traceToggle = el("button", { class: "chip" + (state.memTraces ? " on" : ""), title: "tool-use traces are ephemeral noise; off by default", onclick: () => { state.memTraces = !state.memTraces; tabs.memories(); } },
    state.memTraces ? "✓ tool traces" : "tool traces hidden");

  const body = el("div", { class: "feed" });
  setView(head("Memories", `Newest first · ${state.project || "all"}`),
    el("div", { class: "chips-row" }, chips, traceToggle), body);

  const kinds = active === "all" ? [] : [active];
  const res = await api.post("/events", { kinds, project: state.project, include_global: false, limit: 120, signal_only: !state.memTraces });
  if (!res.events.length) { body.append(el("div", { class: "empty" }, state.memTraces ? "No memories in this scope yet." : "No signal memories here. Toggle 'tool traces' to see ephemeral captures.")); return; }
  res.events.forEach((ev) => body.append(eventRow(ev)));
};

tabs.search = async function () {
  const q = (state.search || "").trim();
  const sub = `Hybrid: BM25 + meaning + facts → reranked · ${state.project || "all projects"}`;
  if (!q) {
    const samples = ["what do I prefer", "decisions we locked", "what shipped this week", "what was I working on"];
    const sampleRow = el("div", { class: "chips-row", style: "margin-top:16px" },
      el("span", { class: "faint", style: "margin-right:6px" }, "try:"),
      el("div", { class: "chips" }, ...samples.map((s) =>
        el("button", { class: "chip", onclick: () => { state.search = s; tabs.search(); } }, s))));
    return setView(head("Search", sub),
      emptyNode("Search your whole memory.", "Type a keyword, a phrase, or a question. Results are reranked across lexical, semantic, and the fact graph, scoped to the selected project."),
      sampleRow);
  }
  loadingView(`Searching "${q}"…`);
  let res;
  try { res = await api.post("/search", { query: q, k: 25, browse: true, ...searchScope() }); }
  catch (e) { return setView(head("Search", sub), el("div", { class: "empty" }, e.message)); }
  const results = res.results || [];
  if (!results.length) {
    return setView(head("Search", sub), el("div", { class: "empty" }, `No matches for "${q}" in this scope.`));
  }
  const terms = q.toLowerCase().split(/\s+/).filter((t) => t.length > 1);
  const rows = results.map((r) => {
    const text = r.fact || "";
    const kw = terms.some((t) => text.toLowerCase().includes(t));
    return el("div", { class: "sresult" },
      el("div", { class: "sr-text", html: highlightTerms(text, terms) }),
      el("div", { class: "sr-meta" },
        el("span", { class: "why " + (kw ? "kw" : "sem"), title: kw ? "matched on keyword + meaning" : "surfaced by semantic similarity / fact graph" }, kw ? "keyword + meaning" : "semantic"),
        r.valid_from ? el("span", { class: "faint", title: r.valid_from }, fmtWhen(r.valid_from)) : null,
        ...(r.sources || []).map((s) => el("span", { class: "id" }, s)),
        el("span", { class: "score", title: "rerank score" }, (r.score ?? 0).toFixed(3))));
  });
  setView(
    head("Search", `${results.length} result${results.length === 1 ? "" : "s"} for "${q}" · ${sub}`),
    el("div", { class: "search-results" }, ...rows),
  );
};

// Highlight query terms in a result snippet (the lexical "why it matched").
function highlightTerms(text, terms) {
  let html = esc(text);
  for (const t of terms) {
    const re = new RegExp("(" + t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + ")", "ig");
    html = html.replace(re, "<mark>$1</mark>");
  }
  return html;
}

tabs.activity = async function () {
  loadingView();
  const pq = state.project ? "?project=" + encodeURIComponent(state.project) + "&include_global=false" : "";
  const act = await api.get("/activity" + pq);
  const byDate = new Map((act.days || []).map((d) => [d.date, d.count]));
  // Trailing ~26 weeks heatmap.
  const weeks = 26, today = new Date();
  const start = new Date(today); start.setDate(start.getDate() - weeks * 7 + 1);
  // align to Sunday
  start.setDate(start.getDate() - start.getDay());
  const max = Math.max(1, ...byDate.values());
  const lvl = (n) => (n === 0 ? "" : n >= max * 0.66 ? "l4" : n >= max * 0.33 ? "l3" : n >= max * 0.1 ? "l2" : "l1");
  const heat = el("div", { class: "heat" });
  for (let w = 0; w <= weeks; w++) {
    for (let d = 0; d < 7; d++) {
      const day = new Date(start); day.setDate(start.getDate() + w * 7 + d);
      if (day > today) continue;
      const key = day.toISOString().slice(0, 10);
      const n = byDate.get(key) || 0;
      heat.append(el("div", { class: `cell ${lvl(n)}`, title: `${key}: ${n} event${n === 1 ? "" : "s"}` }));
    }
  }
  const legend = el("div", { class: "heat-legend" }, "less",
    ...["", "l1", "l2", "l3", "l4"].map((c) => el("span", { class: `cell ${c}` })), "more");

  // Signal vs trace vs understood, so the user sees how much of the captured
  // volume is real knowledge vs ephemeral tool-traces, and how much is decomposed.
  const sig = act.signal || 0, tr = act.trace || 0, dec = act.decomposed || 0;
  const cap = sig + tr;
  const pct = (n) => (cap ? Math.round((n / cap) * 100) : 0);
  const splitCard = (n, lab, hint, cls) => el("div", { class: "splitcard " + cls },
    el("div", { class: "sc-num" }, n.toLocaleString()), el("div", { class: "sc-lab" }, lab), el("div", { class: "sc-hint faint" }, hint));
  const split = el("div", { class: "split" },
    splitCard(sig, "signal", `${pct(sig)}% of captures, real knowledge`, "sig"),
    splitCard(tr, "trace", `${pct(tr)}% of captures, ephemeral tool-traces`, "tr"),
    splitCard(dec, "understood", sig ? `${Math.round((dec / Math.max(1, sig)) * 100)}% of signal decomposed` : "decomposed", "dec"));
  // One bar showing the signal-vs-noise composition at a glance.
  const compBar = cap ? el("div", { class: "panel" },
    el("div", { class: "comp-bar" },
      el("div", { class: "comp-seg sig", style: `width:${(sig / cap) * 100}%`, title: `signal: ${sig}` }),
      el("div", { class: "comp-seg tr", style: `width:${(tr / cap) * 100}%`, title: `trace: ${tr}` })),
    el("div", { class: "comp-key" },
      el("span", {}, el("span", { class: "swatch sig" }), `signal ${sig.toLocaleString()}`),
      el("span", {}, el("span", { class: "swatch tr" }), `trace ${tr.toLocaleString()}`))) : null;
  setView(
    head("Activity", `When memory is being written · ${state.project || "all"}`),
    split,
    el("div", { class: "section-label" }, "capture composition (signal vs noise)"),
    compBar || el("div", { class: "empty" }, "No captures in this scope yet."),
    el("div", { class: "section-label" }, "write cadence"),
    el("div", { class: "panel" }, heat, legend),
    el("div", { class: "section-label" }, "by event kind"),
    el("div", { class: "panel" }, el("div", { class: "bars" }, ...kindBars(act.by_kind || {}))),
  );
};

tabs.timeline = async function () {
  loadingView();
  // Scope the entity list to the selected project (falls back to all).
  const q = state.project ? "?project=" + encodeURIComponent(state.project) + "&include_global=false" : "";
  const subs = await api.get("/resource/subjects" + q);
  const list = (subs.subjects || []).slice(0, 80);
  const picker = el("select", { class: "select", style: "max-width:280px",
    onchange: (e) => renderRecall(e.target.value) },
    el("option", { value: "" }, list.length ? "Pick an entity…" : "no entities in this scope"),
    ...list.map((s) => el("option", { value: s.subject }, `${s.subject} (${s.count})`)));
  const asof = el("input", { class: "select", type: "date", title: "As-of (bitemporal time-travel)" });
  asof.addEventListener("change", () => { if (picker.value) renderRecall(picker.value); });
  const out = el("div", {});
  setView(head("Timeline", `Beliefs over time · bitemporal as-of · ${state.project || "all"}`, picker, asof), out);

  async function renderRecall(entity) {
    if (!entity) { out.replaceChildren(el("div", { class: "empty" }, "Pick an entity to see its history.")); return; }
    out.replaceChildren(el("div", { class: "loading" }, "Recalling…"));
    const body = { entity };
    if (state.project) { body.project = state.project; body.include_global = false; }
    if (asof.value) body.at_time = new Date(asof.value + "T23:59:59Z").toISOString();
    const res = await api.post("/recall", body);
    // Newest first, grouped by month so a long history collapses into periods
    // instead of one infinite list. Dates are the ORIGINAL valid_from (when the
    // belief was true), never the ingest time.
    const facts = (res.facts || []).slice().sort((a, b) => new Date(b.valid_from) - new Date(a.valid_from));
    if (!facts.length) { out.replaceChildren(el("div", { class: "empty" }, "No facts for this entity in this scope.")); return; }
    const groups = new Map();
    facts.forEach((f) => {
      const d = new Date(f.valid_from);
      const key = isNaN(d) ? "undated" : `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}`;
      const label = isNaN(d) ? "Undated" : d.toLocaleDateString(undefined, { year: "numeric", month: "long" });
      if (!groups.has(key)) groups.set(key, { label, facts: [] });
      groups.get(key).facts.push(f);
    });
    const wrap = el("div", { class: "tl-groups" });
    let first = true;
    for (const [, g] of groups) {
      const tl = el("div", { class: "tl" });
      g.facts.forEach((f) => {
        const retired = !!(f.retired_at || f.valid_to);
        tl.append(el("div", { class: "tl-item" + (retired ? " retired" : "") },
          el("div", { class: "when", title: f.valid_from }, new Date(f.valid_from).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" }),
            el("span", { class: "pill " + (retired ? "super" : "current") }, retired ? "superseded" : "current")),
          el("div", { class: "claim" }, `${f.predicate} → ${f.object}`)));
      });
      const open = first; first = false;
      const tlBody = el("div", { class: "tl-body", style: open ? "" : "display:none" }, tl);
      const hdr = el("button", { class: "tl-period" + (open ? " open" : "") },
        el("span", { class: "tl-caret" }, "▸"), el("span", {}, g.label), el("span", { class: "faint" }, `${g.facts.length} belief${g.facts.length === 1 ? "" : "s"}`));
      hdr.addEventListener("click", () => { const show = tlBody.style.display === "none"; tlBody.style.display = show ? "" : "none"; hdr.classList.toggle("open", show); });
      wrap.append(el("div", { class: "tl-group" }, hdr, tlBody));
    }
    out.replaceChildren(el("div", { class: "panel" }, wrap));
  }

  // Auto-render the most-connected entity so the tab isn't blank on open.
  if (list.length) { picker.value = list[0].subject; renderRecall(list[0].subject); }
  else out.replaceChildren(el("div", { class: "empty" }, "No entities in this scope yet."));
};

tabs.audit = async function () {
  loadingView();
  const ACTIONS = ["all", "COMMIT", "UPDATE", "DEDUP", "SKIP", "FORGET"];
  const active = state.auditAction || "all";
  const chips = el("div", { class: "chips" }, ...ACTIONS.map((a) =>
    el("button", { class: "chip" + (a === active ? " on" : ""), onclick: () => { state.auditAction = a; tabs.audit(); } }, a.toLowerCase())));
  const req = { since: "3650d", project: state.project, include_global: false };
  if (active !== "all") req.action = active;
  const res = await api.post("/journal", req);
  const entries = res.entries || [];
  // Newest first.
  entries.sort((a, b) => new Date(b.ts) - new Date(a.ts));

  const ACTION_DESC = {
    COMMIT: "Kept in memory",
    UPDATE: "Updated a belief (superseded an older one)",
    DEDUP: "Skipped an exact duplicate",
    SKIP: "Skipped (didn't meet the keep policy)",
    FORGET: "Forgotten on request",
  };
  const auditRow = (e) => el("div", { class: "row" },
    el("div", { class: "row-main" },
      el("div", { class: "row-title" }, ACTION_DESC[e.action] || e.action || "decision"),
      el("div", { class: "row-meta" },
        e.reasoning ? el("span", { class: "faint" }, "why: " + e.reasoning) : null,
        el("span", { class: "faint" }, "rule: " + e.rule),
        el("span", { class: "id" }, e.input_id))),
    el("div", { class: "row-side" }, el("span", { class: `badge ${actionClass(e.action)}` }, (e.action || "").toLowerCase()), el("span", { class: "faint", style: "font-size:12px" }, fmtWhen(e.ts))));

  const sub = `Every policy decision, source-linked, the trust trail · ${state.project || "all"}`;
  if (!entries.length) {
    return setView(head("Audit", sub), chips, el("div", { class: "empty" }, "No decisions recorded in this scope."));
  }

  let bodyNodes;
  if (active === "all") {
    // Grouped by action with counts, most-active first; cap each group with a
    // "show more" so the trail is scannable, not one endless list.
    const groups = {};
    entries.forEach((e) => { (groups[e.action] = groups[e.action] || []).push(e); });
    const order = Object.keys(groups).sort((a, b) => groups[b].length - groups[a].length);
    bodyNodes = order.map((act) => {
      const list = groups[act];
      const CAP = 12;
      const feed = el("div", { class: "feed" }, ...list.slice(0, CAP).map(auditRow));
      if (list.length > CAP) {
        const more = el("button", { class: "btn sm", style: "margin-top:8px" }, `show ${list.length - CAP} more`);
        more.addEventListener("click", () => { state.auditAction = act; tabs.audit(); });
        feed.append(more);
      }
      return el("div", {},
        el("div", { class: "section-label" }, el("span", { class: `badge ${actionClass(act)}` }, act.toLowerCase()), el("span", { class: "faint", style: "margin-left:8px" }, `${list.length}`)),
        feed);
    });
  } else {
    bodyNodes = [el("div", { class: "feed" }, ...entries.map(auditRow))];
  }
  setView(head("Audit", sub), chips, ...bodyNodes);
};
const actionClass = (a) => ({ COMMIT: "fact", UPDATE: "update", DEDUP: "policy", SKIP: "policy", FORGET: "forget" }[a] || "policy");

tabs.profile = async function () {
  loadingView();
  const scope = state.project ? { project: state.project, include_global: false } : {};
  const res = await api.post("/profile/grouped", scope);
  const groups = res.groups || [];
  if (!groups.length) {
    return emptyView("No facts to synthesize yet.", "Understanding turns captures into beliefs, grouped by resolved entity.");
  }
  const cards = groups.map((g) => {
    const beliefs = (g.facts || []).map((f) =>
      el("div", { class: "belief" + (f.stale ? " stale" : "") },
        el("span", { class: "pred" }, f.predicate),
        el("span", { class: "obj" }, f.object),
        f.stale ? el("span", { class: "stale-dot", title: `${Math.round((f.freshness || 0) * 100)}% fresh, past its half-life (see Review)` }, "◷") : null,
        el("span", { class: "faint fresh", title: f.valid_from }, fmtWhen(f.valid_from))));
    return el("div", { class: "profile-group" },
      el("div", { class: "pg-head", title: "open in graph", onclick: () => { state.graphAnchor = g.canonical || g.entity; switchTab("graph"); } },
        el("span", { class: "kindtag", style: `--kc:${kindColor(g.kind)}` }, g.kind),
        el("span", { class: "pg-name" }, g.entity),
        el("span", { class: "pg-meta faint" },
          (g.mentions ? `${g.mentions} mention${g.mentions === 1 ? "" : "s"} · ` : "") + `${(g.facts || []).length} belief${(g.facts || []).length === 1 ? "" : "s"}`),
        el("span", { class: "pg-go" }, "⬡")),
      el("div", { class: "pg-body" }, ...beliefs));
  });
  setView(
    head("Profile", `Resolved entities · ${state.project || "all"} · ${res.fact_count ?? 0} current beliefs`),
    el("div", { class: "profile-grid" }, ...cards),
  );
};

// Stable color per entity kind: a few well-known kinds get fixed hues, any other
// (the kind set is open by design) gets a deterministic palette slot from a hash
//, so the legend + node colors stay consistent without a hardcoded enum.
const KIND_PALETTE = ["#5b8cff", "#6ee7d6", "#f7b955", "#c792ea", "#f78c6c", "#7ee787", "#ff7b9c", "#62b6ff", "#d6bcfa", "#9fb0c8"];
const KIND_FIXED = { person: "#6ee7d6", project: "#5b8cff", tool: "#f7b955", org: "#c792ea", concept: "#f78c6c", topic: "#7ee787", decision: "#ff7b9c", thing: "#9fb0c8" };
function kindColor(kind) {
  const k = String(kind || "thing").toLowerCase();
  if (KIND_FIXED[k]) return KIND_FIXED[k];
  let h = 0;
  for (let i = 0; i < k.length; i++) h = (h * 31 + k.charCodeAt(i)) >>> 0;
  return KIND_PALETTE[h % KIND_PALETTE.length];
}

tabs.replay = async function () {
  loadingView();
  const res = await api.post("/events", { project: state.project, include_global: false, limit: 400 });
  const evs = (res.events || []).slice().reverse(); // chronological
  if (!evs.length) { emptyView("Nothing to replay in this scope."); return; }
  let idx = 0, playing = false, speed = 1, timer = null;

  const pos = el("span", { class: "pos" });
  const slider = el("input", { type: "range", min: 0, max: evs.length - 1, value: 0 });
  const playBtn = el("button", { class: "btn sm" }, "▶");
  const detail = el("div", { class: "panel" });
  const listWrap = el("div", { class: "replay-list" });
  const rows = evs.map((ev, i) => el("div", { class: "replay-row", onclick: () => seek(i) },
    badge(ev.kind), el("span", { class: "rt" }, ev.title || "(empty)"), el("span", { class: "rw" }, fmtWhen(ev.ts))));
  rows.forEach((r) => listWrap.append(r));

  function renderCur() {
    const ev = evs[idx];
    pos.textContent = `${idx + 1} / ${evs.length}`;
    slider.value = idx;
    rows.forEach((r, i) => r.classList.toggle("cur", i === idx));
    rows[idx].scrollIntoView({ block: "nearest" });
    detail.replaceChildren(
      el("div", { class: "row-meta", style: "margin-bottom:8px" }, badge(ev.kind),
        ev.project ? el("span", { class: "faint" }, "▸ " + ev.project) : null,
        el("span", { class: "faint" }, new Date(ev.ts).toLocaleString())),
      el("div", {}, ev.title || "(empty)"),
      el("pre", { class: "json", style: "margin-top:10px" }, JSON.stringify(ev.detail, null, 2)));
  }
  const seek = (i) => { idx = Math.max(0, Math.min(evs.length - 1, i)); renderCur(); };
  const step = (n) => seek(idx + n);
  function stop() { playing = false; playBtn.textContent = "▶"; if (timer) { clearInterval(timer); timer = null; } }
  function play() {
    if (playing) return stop();
    playing = true; playBtn.textContent = "⏸";
    timer = setInterval(() => { if (idx >= evs.length - 1) return stop(); step(1); }, 700 / speed);
  }
  state._cleanup = stop;
  playBtn.addEventListener("click", play);
  slider.addEventListener("input", () => seek(+slider.value));
  const speeds = el("div", { class: "speeds" }, ...[0.5, 1, 2, 4].map((s) =>
    el("button", { class: "btn sm" + (s === speed ? " primary" : ""), onclick: (e) => {
      speed = s; if (playing) { stop(); play(); }
      [...e.target.parentElement.children].forEach((b) => b.classList.toggle("primary", b === e.target));
    } }, s + "×")));

  setView(
    head("Replay", `Scrub the event log · ${state.project || "all"} · ${evs.length} events`),
    el("div", { class: "replay-bar" },
      el("button", { class: "btn sm", onclick: () => step(-1) }, "⏮"), playBtn,
      el("button", { class: "btn sm", onclick: () => step(1) }, "⏭"), slider, pos, speeds),
    el("div", { style: "display:grid; grid-template-columns:1fr 1fr; gap:14px; align-items:start" }, listWrap, detail),
  );
  renderCur();
};

// Cytoscape layout config shared by initial render + anchor-expand relayouts.
// Register fcose (force-directed, packs disconnected components nicely) if its
// libs loaded; fall back to built-in cose so the graph always renders.
let FCOSE = false;
try {
  if (typeof cytoscape !== "undefined" && window.cytoscapeFcose) { cytoscape.use(window.cytoscapeFcose); FCOSE = true; }
} catch (_) { FCOSE = false; }
const GRAPH_LAYOUT = FCOSE
  ? { name: "fcose", quality: "default", animate: false, randomize: true, padding: 40, nodeSeparation: 120, idealEdgeLength: 110, nodeRepulsion: 6500, gravity: 0.25, gravityRange: 3.8, packComponents: true, numIter: 2500 }
  : { name: "cose", animate: false, padding: 30, nodeRepulsion: 16000, idealEdgeLength: 120, nodeOverlap: 16, gravity: 0.25, componentSpacing: 140, numIter: 1200 };

tabs.graph = async function () {
  loadingView("Building graph…");
  // A click from Profile focuses the graph on one entity (anchor-first).
  const anchorInit = state.graphAnchor; state.graphAnchor = null;
  const req = anchorInit ? { anchor: anchorInit, limit: 200 } : { project: state.project, include_global: false, limit: 240 };
  const res = await api.post("/graph", req);
  const nodesIn = res.nodes || [], edgesIn = res.edges || [];
  if (!nodesIn.length) {
    return emptyView(anchorInit ? `Nothing connected to "${anchorInit}" yet.` : "No typed graph yet.", "Understanding decomposes captures into typed entities + relations, backfill to grow it.");
  }
  if (typeof cytoscape !== "function") {
    return emptyView("Graph library failed to load.", "vendor/cytoscape.min.js did not load, try a hard refresh.");
  }

  const nodeEl = (n) => ({ data: { id: n.id, label: n.label || n.id, kind: n.kind || "thing", degree: n.degree || 1, mentions: n.mentions || 0 } });
  const edgeEl = (e, i) => ({ data: { id: "e" + i + "_" + e.source + "_" + e.target, source: e.source, target: e.target, label: e.label || "", conf: e.confidence, when: e.valid_from } });
  const nodeIds = new Set(nodesIn.map((n) => n.id));
  let edges = edgesIn.filter((e) => nodeIds.has(e.source) && nodeIds.has(e.target));

  // Declutter (default view): keep the connected CORE and drop tiny isolated
  // fragments (the 2-node / 1-edge pairs that scatter the canvas). agentmemory
  // does the same via a degree-ranked snapshot; here we drop components < 3.
  let nodes = nodesIn, hiddenFrags = 0;
  if (!anchorInit) {
    // Drop only obvious junk labels (None / null / empty / single char). Keep
    // every real entity, INCLUDING 2-node pairs: those pairs are your decisions
    // (subject -> relation -> object), not noise to hide.
    const isJunk = (n) => {
      const l = String(n.label || n.id || "").trim().toLowerCase();
      return !l || l === "none" || l === "null" || l === "undefined" || l.length < 2;
    };
    let base = nodesIn.filter((n) => !isJunk(n));
    const baseIds0 = new Set(base.map((n) => n.id));
    let e2 = edges.filter((e) => baseIds0.has(e.source) && baseIds0.has(e.target));
    // Keep only meaningful clusters: drop single dots and bare pairs (connected
    // components < 3 nodes). A real insight is several memories linked together,
    // not a lone dot or a one-off pair.
    const adj = new Map(base.map((n) => [n.id, []]));
    e2.forEach((e) => { adj.get(e.source).push(e.target); adj.get(e.target).push(e.source); });
    const comp = new Map();
    let cid = 0;
    for (const n of base) {
      if (comp.has(n.id)) continue;
      const stack = [n.id]; comp.set(n.id, cid);
      while (stack.length) { const u = stack.pop(); for (const v of adj.get(u) || []) if (!comp.has(v)) { comp.set(v, cid); stack.push(v); } }
      cid++;
    }
    const size = {};
    comp.forEach((c) => { size[c] = (size[c] || 0) + 1; });
    let keep = base.filter((n) => size[comp.get(n.id)] >= 3);
    if (keep.length < 3) keep = base; // too sparse: don't blank the graph
    // Cap very large graphs to the most-connected core so it stays legible.
    if (keep.length > 120) {
      keep = keep.slice().sort((a, b) =>
        ((b.degree || 0) + (b.mentions || 0)) - ((a.degree || 0) + (a.mentions || 0))).slice(0, 120);
    }
    const keepIds = new Set(keep.map((n) => n.id));
    nodes = keep;
    edges = e2.filter((e) => keepIds.has(e.source) && keepIds.has(e.target));
    hiddenFrags = nodesIn.length - nodes.length;
  }
  const elements = [...nodes.map(nodeEl), ...edges.map(edgeEl)];

  const cyEl = el("div", { class: "cy" });
  const legend = el("div", { class: "graph-legend" });
  const wrap = el("div", { class: "graph-wrap" },
    cyEl, legend,
    el("div", { class: "graph-hint" }, "click a node to expand its neighborhood · scroll to zoom · drag to pan"));
  const fragNote = hiddenFrags > 0 ? ` · ${hiddenFrags} small fragment${hiddenFrags === 1 ? "" : "s"} hidden` : "";
  const headSub = anchorInit
    ? `Anchored on "${anchorInit}" · ${nodes.length} nodes · ${edges.length} edges`
    : `Typed knowledge graph · ${nodes.length} nodes · ${edges.length} edges · ${state.project || "all"}${fragNote}`;
  // Query-the-graph: a hybrid search whose hits light up the matching subgraph.
  const gsearch = el("input", { class: "graph-search", type: "search", placeholder: "Query the graph — e.g. 'license decision', 'eggs last week'…" });
  const gstatus = el("span", { class: "graph-search-status faint" });
  setView(head("Graph", headSub, gsearch, gstatus), wrap);
  cyEl.style.height = Math.max(520, window.innerHeight - 250) + "px";

  const cy = cytoscape({
    container: cyEl,
    elements,
    wheelSensitivity: 0.25,
    style: [
      { selector: "node", style: {
        "background-color": (ele) => kindColor(ele.data("kind")),
        "label": "data(label)", "color": "#cdd6e6", "font-size": 9,
        "text-valign": "bottom", "text-halign": "center", "text-margin-y": 3,
        "min-zoomed-font-size": 7, "text-max-width": 120, "text-wrap": "ellipsis",
        "width": (ele) => 14 + Math.min(ele.data("degree") || 1, 16) * 2 + Math.min(ele.data("mentions") || 0, 50) * 0.35,
        "height": (ele) => 14 + Math.min(ele.data("degree") || 1, 16) * 2 + Math.min(ele.data("mentions") || 0, 50) * 0.35,
        "border-width": 0,
      }},
      { selector: "edge", style: {
        "width": 1, "line-color": "rgba(120,140,180,.30)",
        "target-arrow-shape": "triangle", "target-arrow-color": "rgba(120,140,180,.4)",
        "arrow-scale": 0.7, "curve-style": "bezier",
        "label": "data(label)", "font-size": 8, "color": "#c7d0e0",
        "text-rotation": "autorotate", "text-opacity": 1, "min-zoomed-font-size": 10,
        "text-background-color": "#12131a", "text-background-opacity": 0.85,
        "text-background-padding": 2, "text-background-shape": "roundrectangle",
      }},
      { selector: "edge.hl", style: { "text-opacity": 1, "line-color": "#6ee7d6", "width": 1.6, "target-arrow-color": "#6ee7d6" } },
      { selector: "node.anchor", style: { "border-width": 3, "border-color": "#f7b955" } },
      { selector: "node.faded", style: { "opacity": 0.25 } },
      { selector: "edge.faded", style: { "opacity": 0.12 } },
      // Query-the-graph highlight: matches glow, the rest dims back.
      { selector: "node.match", style: { "border-width": 4, "border-color": "#6ee7d6", "opacity": 1 } },
      { selector: "edge.matchedge", style: { "line-color": "#6ee7d6", "width": 2, "target-arrow-color": "#6ee7d6", "opacity": 1, "text-opacity": 1 } },
      { selector: ".dim", style: { "opacity": 0.1 } },
    ],
    layout: GRAPH_LAYOUT,
  });

  // Data-driven legend: one swatch per entity kind present.
  const renderLegend = () => {
    const kinds = [...new Set(cy.nodes().map((n) => n.data("kind")))].sort();
    legend.replaceChildren(...kinds.map((k) =>
      el("span", { class: "lg-item" }, el("span", { class: "lg-sw", style: `background:${kindColor(k)}` }), k)));
  };
  renderLegend();

  // If we arrived via a Profile click, mark + center the anchored entity.
  if (anchorInit) {
    const a = cy.getElementById(anchorInit);
    if (a && a.nonempty()) { a.addClass("anchor"); cy.animate({ center: { eles: a }, zoom: 1.1 }, { duration: 300 }); }
  } else {
    // Fit the whole graph into view once the layout settles, so it never renders
    // tiny and off-center on first load.
    cy.one("layoutstop", () => cy.fit(undefined, 50));
  }

  // Hover: reveal the relation label + highlight incident edges.
  cy.on("mouseover", "node", (e) => { e.target.connectedEdges().addClass("hl"); cyEl.style.cursor = "pointer"; });
  cy.on("mouseout", "node", (e) => { e.target.connectedEdges().removeClass("hl"); cyEl.style.cursor = "grab"; });

  // Track which nodes have been expanded, so a second click retracts them.
  const expanded = new Set();

  // Anchor-first expansion (Cypher-lite MATCH (a)-[r]-(n)): pull the 2-hop
  // neighborhood and merge it in, then relayout. Focuses the view the way a
  // knowledge graph is meant to be explored, never the whole hairball at once.
  cy.on("tap", "node", async (evt) => {
    const id = evt.target.id();
    if (expanded.has(id)) {
      // Second click on an expanded node: retract the leaf neighbors it added
      // (degree-1 nodes hanging only off it). Shared / further-explored nodes stay.
      cy.remove(evt.target.neighborhood("node").filter((n) => n.degree() <= 1));
      expanded.delete(id);
      evt.target.removeClass("anchor");
      renderLegend();
      cy.animate({ fit: { eles: evt.target.closedNeighborhood(), padding: 90 }, duration: 300 });
      return;
    }
    let r;
    try { r = await api.post("/graph", { anchor: id, limit: 60 }); }
    catch (_) { return; }
    const anchorPos = evt.target.position();
    let addedColl = cy.collection();
    const isJunkN = (n) => { const l = String(n.label || n.id || "").trim().toLowerCase(); return !l || l === "none" || l === "null" || l === "undefined" || l.length < 2; };
    cy.batch(() => {
      const have = new Set(cy.nodes().map((n) => n.id()));
      (r.nodes || []).filter((n) => !isJunkN(n)).forEach((n) => {
        if (!have.has(n.id)) {
          const ne = cy.add(nodeEl(n));
          // Spawn new nodes AT the tapped node so the neighborhood grows outward
          // from it, instead of the whole canvas re-scattering on every click.
          ne.position({ x: anchorPos.x + (Math.random() - 0.5) * 80, y: anchorPos.y + (Math.random() - 0.5) * 80 });
          addedColl = addedColl.union(ne); have.add(n.id);
        }
      });
      const haveE = new Set(cy.edges().map((e) => e.data("source") + "|" + e.data("target") + "|" + e.data("label")));
      (r.edges || []).forEach((e, i) => {
        const k = e.source + "|" + e.target + "|" + e.label;
        if (!haveE.has(k) && cy.getElementById(e.source).nonempty() && cy.getElementById(e.target).nonempty()) {
          cy.add(edgeEl(e, "x" + Date.now() + i)); haveE.add(k);
        }
      });
      cy.nodes().removeClass("anchor");
      evt.target.addClass("anchor");
    });
    renderLegend();
    expanded.add(id);
    if (addedColl.nonempty()) {
      // Lock the existing nodes so only the NEW neighborhood lays out; never
      // re-fit the viewport. The graph stays put and grows where you clicked.
      const existing = cy.nodes().difference(addedColl);
      existing.lock();
      const lay = cy.layout({ ...GRAPH_LAYOUT, randomize: false, fit: false, animate: true, animationDuration: 350, numIter: 500 });
      lay.one("layoutstop", () => { existing.unlock(); cy.animate({ fit: { eles: evt.target.closedNeighborhood(), padding: 80 }, duration: 350 }); });
      lay.run();
    }
  });

  // Query-the-graph: hybrid search (keyword + meaning + temporal phrases like
  // "last week") -> the hit captures' entities light up, everything else dims,
  // and the view fits to the matched subgraph. The graph becomes queryable.
  async function runGraphSearch(q) {
    q = (q || "").trim();
    cy.elements().removeClass("match matchedge dim");
    if (!q) { gstatus.textContent = ""; return; }
    gstatus.textContent = "searching…";
    let hits;
    try { hits = await api.post("/search", { query: q, k: 30, browse: true, ...searchScope() }); }
    catch (_) { gstatus.textContent = "search failed"; return; }
    const eventIds = [];
    (hits.results || []).forEach((r) => (r.sources || []).forEach((s) => eventIds.push(s)));
    if (!eventIds.length) { gstatus.textContent = "no matches"; return; }
    let hl;
    try { hl = await api.post("/graph/highlight", { event_ids: eventIds }); }
    catch (_) { gstatus.textContent = "highlight failed"; return; }
    const matchSet = new Set(hl.entities || []);
    const inView = cy.nodes().filter((n) => matchSet.has(n.id()));
    if (!inView.length) { gstatus.textContent = `${matchSet.size} match(es), none in the current view — click a node to expand, or clear the project filter`; return; }
    cy.batch(() => {
      cy.nodes().forEach((n) => n.addClass(matchSet.has(n.id()) ? "match" : "dim"));
      cy.edges().forEach((e) => {
        if (matchSet.has(e.data("source")) && matchSet.has(e.data("target"))) e.addClass("matchedge");
        else e.addClass("dim");
      });
    });
    gstatus.textContent = `${inView.length} node(s) lit for "${q}"`;
    cy.animate({ fit: { eles: inView, padding: 70 } }, { duration: 400 });
  }
  let gTimer;
  gsearch.addEventListener("input", (e) => { clearTimeout(gTimer); const v = e.target.value; gTimer = setTimeout(() => runGraphSearch(v), 260); });

  state._cleanup = () => { try { cy.destroy(); } catch (_) {} };
};

// ---- nav + chrome -------------------------------------------------------
function switchTab(tab) {
  if (state._cleanup) { try { state._cleanup(); } catch (_) {} state._cleanup = null; }
  state.tab = tab;
  document.querySelectorAll(".nav-item").forEach((b) => b.classList.toggle("active", b.dataset.tab === tab));
  // Search lives in the top bar (not a nav item); leaving it clears the marker.
  if (tab !== "search") { state.search = ""; const gs = $("#globalSearch"); if (gs && document.activeElement !== gs) gs.value = ""; }
  renderDest(tab).catch((e) => emptyView("Something went wrong", e.message));
}

// The 5 destinations. Two of them group several views under a sub-nav so the
// top-level navigation stays small (was 11 tabs) while reusing each view's
// existing renderer. [subKey, label, tabFn].
const DEST_SUBS = {
  memory: [["entities", "Entities", "profile"], ["timeline", "Timeline", "timeline"], ["stream", "Stream", "memories"], ["review", "Review", "review"]],
  trust: [["replay", "Event tape", "replay"], ["activity", "Activity", "activity"], ["audit", "Audit", "audit"]],
};
async function renderDest(dest) {
  const subs = DEST_SUBS[dest];
  if (!subs) { await (tabs[dest] || tabs.home)(); return; }
  const active = subs.some((s) => s[0] === state.sub) ? state.sub : subs[0][0];
  state.sub = active;
  await tabs[subs.find((s) => s[0] === active)[2]]();
  // Prepend the sub-nav after the view rendered, so each sub-view keeps using
  // its own setView untouched.
  const subnav = el("div", { class: "subnav" }, ...subs.map(([k, label]) =>
    el("button", { class: "subchip" + (k === active ? " on" : ""), onclick: () => { state.sub = k; switchTab(dest); } }, label)));
  if (view.firstChild) view.insertBefore(subnav, view.firstChild); else view.append(subnav);
}
document.querySelectorAll(".nav-item").forEach((b) => b.addEventListener("click", () => { state.sub = null; switchTab(b.dataset.tab); }));
$("#refreshBtn").addEventListener("click", () => switchTab(state.tab));

// global search → the dedicated Search tab (hybrid, project-scoped), debounced.
let searchTimer;
$("#globalSearch").addEventListener("input", (e) => {
  clearTimeout(searchTimer);
  const q = e.target.value.trim();
  searchTimer = setTimeout(() => { state.search = q; switchTab("search"); }, 220);
});

// Readable label for a project_path (its last path segment), for the selector.
function basename(p) {
  if (!p) return p;
  const parts = String(p).replace(/\/+$/, "").split("/");
  return parts[parts.length - 1] || p;
}

// /search takes a SPEC §2.8 scope object; every other endpoint takes a bare
// `project` field carrying the same collision-proof project_path. state.project
// IS that path (the selector value), so the two stay in lockstep.
function searchScope() {
  return state.project
    ? { scope: { key: "project_path", value: state.project, include_global: false } }
    : {};
}

// project selector (re-renders the current tab scoped). Values are the
// collision-proof project_path; the option text shows the readable basename.
async function loadProjects() {
  try {
    const t = await api.get("/resource/tags");
    const projects = (t.tags || [])
      .filter((x) => x.key === "project_path")
      .sort((a, b) => b.count - a.count);
    const sel = $("#projectSelect");
    sel.replaceChildren(el("option", { value: "" }, "All projects"),
      ...projects.map((p) => el("option", { value: p.value }, `${basename(p.value)} (${p.count})`)));
    // Default to the dominant project so a developer lands in their current
    // work scoped, not an all-projects soup. "All projects" stays one click away.
    if (!state.project && projects.length) {
      state.project = projects[0].value;
      sel.value = state.project;
    }
    sel.addEventListener("change", (e) => { state.project = e.target.value; switchTab(state.tab); });
  } catch (_) {}
}

// status pill
async function pollStatus() {
  const dot = $("#status-dot"), txt = $("#status-text");
  try {
    const [h, v] = await Promise.all([api.get("/health"), api.get("/version").catch(() => null)]);
    dot.className = "dot up"; txt.textContent = "core running";
    if (v?.version) $("#version").textContent = "v" + v.version;
  } catch (_) { dot.className = "dot down"; txt.textContent = "core unreachable"; }
}

// ---- theme (light / dark, persisted) ------------------------------------
function applyTheme(t) {
  document.documentElement.setAttribute("data-theme", t);
  const btn = $("#themeBtn");
  if (btn) btn.textContent = t === "light" ? "☀" : "☾";
  try { localStorage.setItem("lm-theme", t); } catch (_) {}
}
$("#themeBtn").addEventListener("click", () => {
  const cur = document.documentElement.getAttribute("data-theme") === "light" ? "dark" : "light";
  applyTheme(cur);
});
applyTheme((() => { try { return localStorage.getItem("lm-theme") || "dark"; } catch (_) { return "dark"; } })());

// ---- boot ---------------------------------------------------------------
pollStatus(); setInterval(pollStatus, 10000);
// Resolve the default project BEFORE the first render so the dashboard lands
// scoped to the developer's current work (loadProjects swallows its own errors,
// so the view always renders even if the tag fetch fails).
// Deep-linkable tabs: open the tab named in the URL hash (#graph, #brain, ...),
// default to home. Also lets the tab be screenshotted / shared directly.
function tabFromHash() { const h = (location.hash || "").replace(/^#/, "").trim(); return h || "home"; }
loadProjects().finally(() => switchTab(tabFromHash()));
window.addEventListener("hashchange", () => switchTab(tabFromHash()));
