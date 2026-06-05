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

### Prerequisite — start the core

```bash
localmem serve
```

Leave that running. The dashboard talks to it at the default `http://127.0.0.1:7788`.

### Option A — open from file:// (might just work)

```bash
open /Users/you/path/to/localmem-public/dashboard/index.html
```

Some browsers allow fetches from `file://` to `localhost`. If it works, you'll see your data. If you see the "Can't reach the localmem core" dialog with a CORS-shaped message in the browser console, use Option B.

### Option B — serve via localhost (always works)

```bash
cd dashboard
python3 -m http.server 8080
# then in your browser: http://localhost:8080/
```

This serves the dashboard from `http://localhost:8080`. Same-origin policy treats it as different from `http://localhost:7788` (where the API lives), so browser CORS rules apply — but most browsers permit localhost-to-localhost requests freely.

### Option C — coming in v0.2.2

```bash
localmem serve --dashboard      # planned
```

The Rust core will serve `dashboard/` at the same origin as the API. No CORS, no second process. Tracked as a v0.2.2 task. Once shipped, this becomes the default.

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
