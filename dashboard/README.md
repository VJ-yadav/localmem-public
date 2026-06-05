# localmem dashboard

A local web UI for visually browsing the memory stored in your
`~/.localmem/` directory. Read-only by design.

**Status:** v0.2.1 MVP. Single static page (HTML + CSS + ~250 lines of vanilla JS, no framework). Future versions will be served by the Rust core directly (`localmem serve --dashboard`) on the same port as the API.

## What you see

- **Subjects panel** (left) — every entity your memory knows about, with capture counts. Click to recall facts about that subject.
- **Tags panel** (left) — every container tag in use, with counts. Click to filter the feed to that tag.
- **Recent captures** (center) — last 25 captures as cards, color-coded by kind (`fact`, `preference`, `decision`, `constraint`, `todo`, `note`).
- **Search** (top bar) — hybrid / lex / vec retrieval against your memory. Results render in the right panel.
- **Subject recall** (right panel, after clicking a subject) — every fact tied to that subject.
- **Profile** (right panel, on demand) — synthesized markdown profile across all subjects.
- **Connection status** (top bar) — pill turns red if the dashboard can't reach `localmem serve`.

## How to use it (v0.2.1)

### Step 1 — start the localmem core

```bash
localmem serve
```

Leave that running. The dashboard talks to it at the default `http://127.0.0.1:7788`.

### Step 2 — start the dashboard server

```bash
python3 dashboard/serve.py
```

This does **two** things in a single process:
1. Serves the dashboard static files on `http://localhost:8088/`
2. Forwards `/api/*` requests to the localmem core at `http://127.0.0.1:7788`

Because the dashboard and the API now share the same origin (your localhost:8088), the browser does not block fetches. **No CORS issues.**

The script auto-opens your default browser to `http://localhost:8088/?api=/api`. Set `DASHBOARD_NO_BROWSER=1` to skip the auto-open. Other env vars:

| Var | Default | Purpose |
|---|---|---|
| `DASHBOARD_PORT` | `8088` | Where to serve the UI |
| `LOCALMEM_CORE_URL` | `http://127.0.0.1:7788` | Where the localmem core is running |
| `DASHBOARD_NO_BROWSER` | unset | If set to `1`, do not auto-open the browser |

Stop with Ctrl-C.

### Why a helper script and not just open the HTML directly

Browsers block cross-origin fetches between different localhost ports. The dashboard at `file://...` or `localhost:8080` cannot read JSON from `localhost:7788` because the localmem core does not (yet) send `Access-Control-Allow-Origin` headers. The proxy in `serve.py` makes the dashboard and the API look like the same origin to the browser. ~110 lines of Python, only the stdlib.

### Coming in v0.2.2

```bash
localmem serve --dashboard      # planned
```

The Rust core will serve `dashboard/` at the same origin as the API natively. No proxy, no `serve.py`, no second process. Tracked as a v0.2.2 task. Once shipped, the Python helper goes away.

## Custom API endpoint

If your `localmem serve` runs on a non-default port (`--addr 127.0.0.1:9999`), point the dashboard at it via URL parameter:

```
file:///path/to/dashboard/index.html?api=http://127.0.0.1:9999
```

## What this is NOT

This dashboard is intentionally:

- **Read-only.** It doesn't write, forget, or edit anything. The MCP tools + CLI are the write paths.
- **Single-user.** No login, no team views, no RBAC. This is your local memory on your machine.
- **MVP.** v0.2.1 is "tabular browse + search." Force-directed entity graph, conflict timeline, journal viewer, capture editor are post-v0.2.1.

Multi-user / team views / audit retention dashboards / SSO are on the **enterprise tier roadmap** — separate offering in a future release.

## Hacking on it

Pure HTML + CSS + JS. No build step.

```bash
# Edit any file, refresh the browser
open dashboard/index.html
```

To make a change you'd contribute back: edit, test locally, open a PR against `VJ-yadav/localmem-public`. The dashboard is intentionally framework-free; please don't add React / Vue / Svelte unless we genuinely need them.
