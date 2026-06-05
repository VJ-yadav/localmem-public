// localmem dashboard — vanilla JS, no framework
//
// Talks to a running `localmem serve` via the local proxy at /api,
// plus uses /__meta/* for store discovery + live switching.
//
// URL state convention (kept clean — no implementation params surface):
//   /                       default view of the active store
//   /?subject=Vijay         recall view for a subject
//   /?q=rust                search results
//   /?tag=project=foo       feed filtered by container tag
//   /?api=http://host:port  ONLY for non-default API base (override)
//
// All other state lives in the panels and updates via pushState as you
// click around, so the back button works and links are shareable.

const params = () => new URLSearchParams(location.search);

// API base: prefer ?api= override; else "/api" (the proxy default).
// This keeps the URL bar clean for normal users.
const API = (params().get("api") || "/api").replace(/\/+$/, "");

const $ = (id) => document.getElementById(id);
const setText = (id, t) => { const el = $(id); if (el) el.textContent = t; };

function escapeHtml(s) {
  return (s ?? "").toString()
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function timeAgo(iso) {
  try {
    const then = new Date(iso).getTime();
    const diff = Math.max(0, (Date.now() - then) / 1000);
    if (diff < 60) return `${Math.floor(diff)}s ago`;
    if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
    return `${Math.floor(diff / 86400)}d ago`;
  } catch { return iso; }
}

function timeAgoFromEpoch(secs) {
  if (!secs) return "";
  const diff = Math.max(0, Date.now() / 1000 - secs);
  if (diff < 60) return `${Math.floor(diff)}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return `${Math.floor(diff / 86400)}d ago`;
}

function formatBytes(n) {
  if (!n && n !== 0) return "";
  if (n < 1024) return `${n}B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}KB`;
  return `${(n / (1024 * 1024)).toFixed(1)}MB`;
}

function kindClass(kind) {
  const k = (kind || "note").toLowerCase();
  const known = ["fact", "preference", "decision", "constraint", "todo", "note"];
  return known.includes(k) ? `kind-${k}` : "kind-note";
}

async function api(path, init = {}) {
  const url = `${API}${path}`;
  const res = await fetch(url, init);
  if (!res.ok) throw new Error(`${res.status} ${res.statusText} for ${path}`);
  const ct = res.headers.get("content-type") || "";
  return ct.includes("application/json") ? res.json() : res.text();
}

async function meta(path, init = {}) {
  const res = await fetch(path, init);
  if (!res.ok) throw new Error(`${res.status} ${res.statusText} for ${path}`);
  return res.json();
}

// ---- Toast ----------------------------------------------------------------

function showToast(msg, opts = {}) {
  let el = document.getElementById("__toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "__toast";
    el.className = "toast";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.className = "toast" + (opts.kind ? ` toast-${opts.kind}` : "") + " show";
  clearTimeout(showToast._t);
  showToast._t = setTimeout(() => { el.className = "toast"; }, opts.ms || 2400);
}

// ---- URL state ------------------------------------------------------------
//
// pushUrlState writes a clean URL like "/?subject=Vijay" without reloading,
// preserving the ?api= override only when one was given explicitly.

function pushUrlState(updates) {
  const p = new URLSearchParams(location.search);
  // preserve only api= override if it was originally explicit
  // strip any keys we manage so they don't accumulate stale state
  for (const k of ["subject", "q", "tag", "store"]) p.delete(k);
  for (const [k, v] of Object.entries(updates || {})) {
    if (v) p.set(k, v);
  }
  const qs = p.toString();
  const newUrl = qs ? `${location.pathname}?${qs}` : location.pathname;
  history.pushState(updates || {}, "", newUrl);
}

// ---- Connection -----------------------------------------------------------

async function checkConnection() {
  $("endpoint-label").innerHTML = `connected to <code>${API === "/api" ? "localmem core (via proxy)" : API}</code>`;
  $("help-endpoint").textContent = API;
  try {
    await api("/health");
    const pill = $("conn-pill");
    pill.textContent = "connected";
    pill.className = "pill pill-ok";
    try {
      const v = await api("/version");
      const versionStr = (v && (v.version || v.localmem_version)) || "";
      if (versionStr) {
        const vpill = $("version-pill");
        vpill.textContent = `v${String(versionStr).replace(/^v/, "")}`;
        vpill.hidden = false;
      }
    } catch { /* /version optional */ }
    return true;
  } catch {
    const pill = $("conn-pill");
    pill.textContent = "disconnected";
    pill.className = "pill pill-err";
    pill.style.cursor = "pointer";
    pill.title = "click for help";
    pill.onclick = () => $("help-dialog").showModal();
    $("help-dialog").showModal();
    return false;
  }
}

// ---- Stores (sidebar) -----------------------------------------------------

let storesData = { stores: [], active_home: "" };

async function loadStores() {
  try {
    const data = await meta("/__meta/stores");
    storesData = data;
    renderStores();
  } catch {
    $("stores-list").innerHTML = `<li class="empty">no proxy <code>/__meta/stores</code> endpoint. Start the dashboard via <code>python3 serve.py</code>.</li>`;
  }
}

function renderStores() {
  const list = $("stores-list");
  const stores = storesData.stores || [];
  const activePath = storesData.active_home || "";

  if (stores.length === 0) {
    list.innerHTML = `<li class="empty">no <code>.localmem</code> dirs discovered. Set <code>LOCALMEM_DASHBOARD_SCAN</code> when starting <code>serve.py</code>.</li>`;
    return;
  }

  list.innerHTML = stores.map(s => {
    const isActive = s.path === activePath;
    return `<li class="store-row ${isActive ? "active" : ""}" data-path="${escapeHtml(s.path)}">
      <div class="store-top">
        <span class="store-label">${escapeHtml(s.label || "store")} ${isActive ? "<span class=\"active-marker\">active</span>" : ""}</span>
        <span class="store-meta">${s.events} events</span>
      </div>
      <div class="store-path" title="${escapeHtml(s.path)}">${escapeHtml(s.path)}</div>
      <div class="store-meta">${formatBytes(s.size_bytes)} &middot; ${timeAgoFromEpoch(s.last_modified)}</div>
      ${isActive ? "" : `<div class="store-actions"><button class="btn btn-primary btn-sm switch-btn" data-path="${escapeHtml(s.path)}">switch to this store</button></div>`}
    </li>`;
  }).join("");

  list.querySelectorAll(".switch-btn").forEach(btn => {
    btn.addEventListener("click", async (e) => {
      e.stopPropagation();
      await switchStore(btn.dataset.path, btn);
    });
  });
}

async function switchStore(path, btn) {
  const label = path.split("/").slice(-2).join("/");
  showToast(`Switching to ${label}…`);
  if (btn) { btn.disabled = true; btn.textContent = "switching…"; }
  try {
    const res = await fetch("/__meta/switch", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ home: path })
    });
    const data = await res.json();
    if (!data.ok) throw new Error(data.message || "switch failed");
    showToast(`Active: ${label}`, { kind: "ok" });
    storesData.active_home = data.active_home || path;
    renderStores();
    // Reset main view to defaults for the new store, then reload data.
    activeSubject = null;
    activeTag = null;
    setText("feed-title", "Recent captures");
    setText("detail-title", "Search");
    $("detail").innerHTML = `<p class="hint">Type a query above and press Enter, or click a subject / tag on the left.</p>`;
    $("clear-filter-btn").hidden = true;
    pushUrlState({});
    await Promise.all([loadSubjects(), loadTags(), loadRecent()]);
    await checkConnection();
  } catch (err) {
    showToast(`Switch failed: ${err.message}`, { kind: "err", ms: 4000 });
  } finally {
    if (btn) { btn.disabled = false; btn.textContent = "switch to this store"; }
  }
}

// ---- Subjects + tags + recent --------------------------------------------

let activeSubject = null;
let activeTag = null;

async function loadSubjects() {
  try {
    const data = await api("/resource/subjects");
    const list = $("subjects-list");
    const subjects = data.subjects || [];
    setText("stat-subjects", `${subjects.length} subjects`);
    if (subjects.length === 0) {
      list.innerHTML = `<li class="empty">no subjects yet &mdash; capture a memory to see them appear</li>`;
      return;
    }
    list.innerHTML = subjects.map(s =>
      `<li data-subject="${escapeHtml(s.subject)}" class="${activeSubject === s.subject ? "active" : ""}">
         <span class="lbl">${escapeHtml(s.subject)}</span>
         <span class="count">${s.count}</span>
       </li>`
    ).join("");
    list.querySelectorAll("li[data-subject]").forEach(li => {
      li.addEventListener("click", () => selectSubject(li.dataset.subject));
    });
  } catch (err) {
    $("subjects-list").innerHTML = `<li class="empty">error loading: ${escapeHtml(err.message)}</li>`;
    setText("stat-subjects", "— subjects");
  }
}

async function loadTags() {
  try {
    const data = await api("/resource/tags");
    const list = $("tags-list");
    const tags = data.tags || [];
    setText("stat-tags", `${tags.length} tags`);
    if (tags.length === 0) {
      list.innerHTML = `<li class="empty">no tags in use yet &mdash; try <code>--tags project=foo</code> on a write</li>`;
      return;
    }
    list.innerHTML = tags.map(t => {
      const tagKey = `${t.key}=${t.value}`;
      const isActive = activeTag === tagKey;
      return `<li data-key="${escapeHtml(t.key)}" data-value="${escapeHtml(t.value)}" class="${isActive ? "active" : ""}">
        <span class="lbl"><code>${escapeHtml(t.key)}=${escapeHtml(t.value)}</code></span>
        <span class="count">${t.count}</span>
      </li>`;
    }).join("");
    list.querySelectorAll("li[data-key]").forEach(li => {
      li.addEventListener("click", () => selectTag(li.dataset.key, li.dataset.value));
    });
  } catch (err) {
    $("tags-list").innerHTML = `<li class="empty">error loading: ${escapeHtml(err.message)}</li>`;
    setText("stat-tags", "— tags");
  }
}

async function loadRecent(limit = 25) {
  try {
    const data = await api(`/resource/recent?limit=${limit}`);
    const feed = $("feed");
    const captures = data.captures || [];
    setText("stat-recent", `${captures.length} recent`);
    if (captures.length === 0) {
      feed.innerHTML = `<div class="empty">no captures yet &mdash; try <code>localmem write --kind note --content "hello"</code></div>`;
      return;
    }
    feed.innerHTML = captures.map(c => `
      <article class="capture-card">
        <div class="capture-head">
          <span class="kind-chip ${kindClass(c.kind)}">${escapeHtml(c.kind || "note")}</span>
          <span class="capture-ts" title="${escapeHtml(c.ts)}">${timeAgo(c.ts)}</span>
        </div>
        <div class="capture-text">${escapeHtml(c.text)}</div>
        <div class="capture-id">${escapeHtml(c.event_id)}</div>
      </article>
    `).join("");
  } catch (err) {
    $("feed").innerHTML = `<div class="empty">error loading recent: ${escapeHtml(err.message)}</div>`;
    setText("stat-recent", "— recent");
  }
}

async function runSearch(query, opts = {}) {
  if (!query) return;
  setText("detail-title", `Search: "${query}"`);
  const detail = $("detail");
  detail.innerHTML = `<p class="hint">searching for <code>${escapeHtml(query)}</code>&hellip;</p>`;
  try {
    const data = await api("/search", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ query, k: 10 })
    });
    const hits = data.results || [];
    if (hits.length === 0) {
      detail.innerHTML = `<p class="hint">no results for <code>${escapeHtml(query)}</code>.</p>`;
      return;
    }
    detail.innerHTML = hits.map((h, i) => `
      <div class="result-card">
        <span class="result-score">[${i + 1}] score=${(h.score ?? 0).toFixed(3)}</span>
        <p class="result-snippet">${escapeHtml(h.fact || "")}</p>
        <span class="result-id">${escapeHtml((h.sources && h.sources[0]) || "")}</span>
      </div>
    `).join("");
  } catch (err) {
    detail.innerHTML = `<p class="hint">search failed: ${escapeHtml(err.message)}</p>`;
  }
  if (!opts.skipUrl) pushUrlState({ q: query });
}

async function selectSubject(subject, opts = {}) {
  activeSubject = subject;
  activeTag = null;
  document.querySelectorAll("#subjects-list li").forEach(li =>
    li.classList.toggle("active", li.dataset.subject === subject)
  );
  document.querySelectorAll("#tags-list li").forEach(li => li.classList.remove("active"));
  setText("detail-title", `Facts about ${subject}`);
  const detail = $("detail");
  detail.innerHTML = `<p class="hint">loading recall for <code>${escapeHtml(subject)}</code>&hellip;</p>`;
  try {
    const data = await api("/recall", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ entity: subject })
    });
    const facts = data.facts || [];
    if (facts.length === 0) {
      detail.innerHTML = `<p class="hint">no facts recorded for <code>${escapeHtml(subject)}</code>.</p>`;
      return;
    }
    detail.innerHTML = facts.map(f => `
      <div class="result-card">
        <p class="result-snippet"><strong>${escapeHtml(f.predicate || "")}</strong> ${escapeHtml(f.object || "")}</p>
        <span class="result-id">${escapeHtml((f.sources && f.sources[0]) || "")} ${f.valid_to ? "(retired)" : ""}</span>
      </div>
    `).join("");
  } catch (err) {
    detail.innerHTML = `<p class="hint">recall failed: ${escapeHtml(err.message)}</p>`;
  }
  if (!opts.skipUrl) pushUrlState({ subject });
}

async function selectTag(key, value, opts = {}) {
  const tagKey = `${key}=${value}`;
  activeTag = tagKey;
  activeSubject = null;
  document.querySelectorAll("#tags-list li").forEach(li =>
    li.classList.toggle("active", li.dataset.key === key && li.dataset.value === value)
  );
  document.querySelectorAll("#subjects-list li").forEach(li => li.classList.remove("active"));
  setText("feed-title", `Captures tagged ${key}=${value}`);
  $("clear-filter-btn").hidden = false;
  const feed = $("feed");
  feed.innerHTML = `<div class="empty">loading captures tagged <code>${escapeHtml(key)}=${escapeHtml(value)}</code>&hellip;</div>`;
  try {
    const data = await api("/search", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ query: "*", k: 25, tags: { [key]: value } })
    });
    const hits = data.results || [];
    if (hits.length === 0) {
      feed.innerHTML = `<div class="empty">no captures matching that tag.</div>`;
      return;
    }
    feed.innerHTML = hits.map(h => `
      <article class="capture-card">
        <div class="capture-head">
          <span class="kind-chip kind-note">tagged</span>
          <span class="capture-ts">${escapeHtml((h.sources && h.sources[0]) || "")}</span>
        </div>
        <div class="capture-text">${escapeHtml(h.fact || "")}</div>
      </article>
    `).join("");
  } catch (err) {
    feed.innerHTML = `<div class="empty">tag-filter search failed: ${escapeHtml(err.message)}</div>`;
  }
  if (!opts.skipUrl) pushUrlState({ tag: tagKey });
}

function clearFilter(opts = {}) {
  activeTag = null;
  document.querySelectorAll("#tags-list li").forEach(li => li.classList.remove("active"));
  setText("feed-title", "Recent captures");
  $("clear-filter-btn").hidden = true;
  loadRecent();
  if (!opts.skipUrl) pushUrlState({});
}

async function loadProfile() {
  const out = $("profile-md");
  out.hidden = false;
  out.textContent = "loading profile…";
  try {
    const data = await api("/profile", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ scope: null, tags: {} })
    });
    out.textContent = (data.profile_md || "(empty profile)").trim();
  } catch (err) {
    out.textContent = `profile load failed: ${err.message}`;
  }
}

// ---- Apply URL state on load + back/forward ------------------------------

async function applyUrlState() {
  const p = params();
  const subject = p.get("subject");
  const q = p.get("q");
  const tag = p.get("tag");

  // Default UI state
  setText("detail-title", "Search");
  $("detail").innerHTML = `<p class="hint">Type a query above and press Enter, or click a subject / tag on the left.</p>`;
  setText("feed-title", "Recent captures");
  $("clear-filter-btn").hidden = true;
  activeSubject = null;
  activeTag = null;
  document.querySelectorAll("#subjects-list li").forEach(li => li.classList.remove("active"));
  document.querySelectorAll("#tags-list li").forEach(li => li.classList.remove("active"));

  if (subject) {
    await selectSubject(subject, { skipUrl: true });
  } else if (tag && tag.includes("=")) {
    const [k, v] = tag.split("=", 2);
    await selectTag(k, v, { skipUrl: true });
  }
  if (q) {
    $("search-input").value = q;
    await runSearch(q, { skipUrl: true });
  }
}

// ---- Wire-up --------------------------------------------------------------

document.addEventListener("DOMContentLoaded", async () => {
  loadStores();
  const ok = await checkConnection();
  if (!ok) return;
  await Promise.all([loadSubjects(), loadTags(), loadRecent()]);
  await applyUrlState();

  $("search-input").addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      const q = e.target.value.trim();
      if (q) runSearch(q);
    }
  });

  $("refresh-btn").addEventListener("click", async () => {
    await Promise.all([loadStores(), loadSubjects(), loadTags(), loadRecent()]);
  });

  $("clear-filter-btn").addEventListener("click", () => clearFilter());

  $("load-profile").addEventListener("click", loadProfile);

  window.addEventListener("popstate", applyUrlState);
});
