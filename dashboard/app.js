// localmem dashboard — vanilla JS, no framework
//
// Talks to a running `localmem serve` over HTTP at the default
// 127.0.0.1:7788 (override via ?api=http://host:port). Read-only by
// design — no writes from the dashboard UI for now; the surface is
// `subjects`, `tags`, `recent`, `search`, `recall`, `profile`.

const params = new URLSearchParams(location.search);
const API = (params.get("api") || "http://127.0.0.1:7788").replace(/\/+$/, "");

const $ = (id) => document.getElementById(id);
const setText = (id, t) => { const el = $(id); if (el) el.textContent = t; };

// ---- Small helpers ---------------------------------------------------------

function timeAgo(iso) {
  try {
    const then = new Date(iso).getTime();
    const now = Date.now();
    const diff = Math.max(0, (now - then) / 1000);
    if (diff < 60) return `${Math.floor(diff)}s ago`;
    if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
    return `${Math.floor(diff / 86400)}d ago`;
  } catch { return iso; }
}

function kindClass(kind) {
  const k = (kind || "note").toLowerCase();
  const known = ["fact", "preference", "decision", "constraint", "todo", "note"];
  return known.includes(k) ? `kind-${k}` : "kind-note";
}

function escapeHtml(s) {
  return (s ?? "").toString()
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

async function api(path, init = {}) {
  const url = `${API}${path}`;
  const res = await fetch(url, init);
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText} for ${path}`);
  }
  const ct = res.headers.get("content-type") || "";
  return ct.includes("application/json") ? res.json() : res.text();
}

// ---- Connection check + status pill ---------------------------------------

async function checkConnection() {
  $("endpoint-label").innerHTML = `connected to <code>${API}</code>`;
  $("help-endpoint").textContent = API;
  try {
    await api("/healthz");
    const pill = $("conn-pill");
    pill.textContent = "connected";
    pill.className = "pill pill-ok";
    return true;
  } catch (err) {
    const pill = $("conn-pill");
    pill.textContent = "disconnected";
    pill.className = "pill pill-err";
    pill.style.cursor = "pointer";
    pill.title = "click for help";
    pill.onclick = () => $("help-dialog").showModal();
    // Auto-open help on first failure so the user sees the fix immediately
    $("help-dialog").showModal();
    return false;
  }
}

// ---- Subjects + tags (left panel) -----------------------------------------

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
      `<li data-subject="${escapeHtml(s.subject)}">
         <span class="lbl">${escapeHtml(s.subject)}</span>
         <span class="count">${s.count}</span>
       </li>`
    ).join("");
    list.querySelectorAll("li").forEach(li => {
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
    list.innerHTML = tags.map(t =>
      `<li data-key="${escapeHtml(t.key)}" data-value="${escapeHtml(t.value)}">
         <span class="lbl"><code>${escapeHtml(t.key)}=${escapeHtml(t.value)}</code></span>
         <span class="count">${t.count}</span>
       </li>`
    ).join("");
    list.querySelectorAll("li").forEach(li => {
      li.addEventListener("click", () => selectTag(li.dataset.key, li.dataset.value));
    });
  } catch (err) {
    $("tags-list").innerHTML = `<li class="empty">error loading: ${escapeHtml(err.message)}</li>`;
    setText("stat-tags", "— tags");
  }
}

// ---- Recent captures (center feed) ----------------------------------------

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

// ---- Search (right panel) -------------------------------------------------

async function runSearch(query) {
  if (!query) return;
  setText("detail-title", `Search results`);
  const detail = $("detail");
  detail.innerHTML = `<p class="hint">searching for <code>${escapeHtml(query)}</code>&hellip;</p>`;
  const mode = $("search-mode").value;
  try {
    const data = await api("/search", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ query, k: 10, mode })
    });
    const hits = data.results || data.hits || [];
    if (hits.length === 0) {
      detail.innerHTML = `<p class="hint">no results for <code>${escapeHtml(query)}</code> in <code>${mode}</code> mode.</p>`;
      return;
    }
    detail.innerHTML = hits.map((h, i) => `
      <div class="result-card">
        <span class="result-score">[${i + 1}] score=${(h.score ?? 0).toFixed(3)}</span>
        <p class="result-snippet">${escapeHtml(h.snippet || h.fact || h.content || h.text || "")}</p>
        <span class="result-id">${escapeHtml(h.event_id || h.id || "")}</span>
      </div>
    `).join("");
  } catch (err) {
    detail.innerHTML = `<p class="hint">search failed: ${escapeHtml(err.message)}</p>`;
  }
}

async function selectSubject(subject) {
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
    const facts = data.facts || data.results || [];
    if (facts.length === 0) {
      detail.innerHTML = `<p class="hint">no facts recorded for <code>${escapeHtml(subject)}</code>.</p>`;
      return;
    }
    detail.innerHTML = facts.map(f => `
      <div class="result-card">
        <p class="result-snippet"><strong>${escapeHtml(f.predicate || "")}</strong> ${escapeHtml(f.object || "")}</p>
        <span class="result-id">${escapeHtml(f.id || f.event_id || "")} ${f.retired_at ? "(retired)" : ""}</span>
      </div>
    `).join("");
  } catch (err) {
    detail.innerHTML = `<p class="hint">recall failed: ${escapeHtml(err.message)}</p>`;
  }
}

async function selectTag(key, value) {
  activeTag = `${key}=${value}`;
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
    // Use search with tag filter as the filtering primitive — the
    // resource/recent endpoint doesn't accept a tag filter today.
    // Quick heuristic: empty query => return everything sorted by recency.
    const data = await api("/search", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ query: "*", k: 25, mode: "lex", tags: { [key]: value } })
    });
    const hits = data.results || data.hits || [];
    if (hits.length === 0) {
      feed.innerHTML = `<div class="empty">no captures matching that tag.</div>`;
      return;
    }
    feed.innerHTML = hits.map(h => `
      <article class="capture-card">
        <div class="capture-head">
          <span class="kind-chip ${kindClass(h.kind)}">${escapeHtml(h.kind || "note")}</span>
          <span class="capture-ts" title="${escapeHtml(h.ts || "")}">${h.ts ? timeAgo(h.ts) : ""}</span>
        </div>
        <div class="capture-text">${escapeHtml(h.snippet || h.text || "")}</div>
        <div class="capture-id">${escapeHtml(h.event_id || "")}</div>
      </article>
    `).join("");
  } catch (err) {
    feed.innerHTML = `<div class="empty">tag-filter search failed: ${escapeHtml(err.message)}</div>`;
  }
}

function clearFilter() {
  activeTag = null;
  document.querySelectorAll("#tags-list li").forEach(li => li.classList.remove("active"));
  setText("feed-title", "Recent captures");
  $("clear-filter-btn").hidden = true;
  loadRecent();
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

// ---- Wire-up --------------------------------------------------------------

document.addEventListener("DOMContentLoaded", async () => {
  const ok = await checkConnection();
  if (!ok) return;
  await Promise.all([loadSubjects(), loadTags(), loadRecent()]);

  // Search box
  $("search-input").addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      const q = e.target.value.trim();
      if (q) runSearch(q);
    }
  });

  // Refresh button
  $("refresh-btn").addEventListener("click", async () => {
    await Promise.all([loadSubjects(), loadTags(), loadRecent()]);
  });

  // Clear tag filter
  $("clear-filter-btn").addEventListener("click", clearFilter);

  // Profile loader
  $("load-profile").addEventListener("click", loadProfile);
});
