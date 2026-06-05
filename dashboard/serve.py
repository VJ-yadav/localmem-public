#!/usr/bin/env python3
"""
dashboard/serve.py — one-shot helper to view the localmem dashboard.

Serves dashboard/ as static files on http://localhost:8088/ and
transparently forwards /api/* to the running localmem core (default
http://127.0.0.1:7788). Same origin to the browser, no CORS.

Plus a tiny meta endpoint at /__meta/stores that scans the filesystem
for .localmem/ directories so the dashboard can show every store you
have on this machine, not just the one localmem serve is currently
pointed at.

Usage:
    python3 dashboard/serve.py
    # then open http://localhost:8088/?api=/api

Env vars:
    DASHBOARD_PORT             default 8088
    LOCALMEM_CORE_URL          default http://127.0.0.1:7788
    DASHBOARD_NO_BROWSER       set to 1 to skip the auto-open
    LOCALMEM_DASHBOARD_STORES  colon-separated list of .localmem dirs to
                               surface explicitly (e.g. "$HOME/.localmem:
                               $HOME/projects/foo/.localmem")
    LOCALMEM_DASHBOARD_SCAN    colon-separated list of root dirs to scan
                               for .localmem subdirs (one level deep,
                               e.g. "$HOME/DATA_LAB"). $HOME is always
                               scanned at depth 0 for ~/.localmem.

Stop with Ctrl-C.
"""

import http.server
import json
import os
import socketserver
import sys
import urllib.error
import urllib.parse
import urllib.request
import webbrowser
from pathlib import Path

PORT = int(os.environ.get("DASHBOARD_PORT", "8088"))
CORE_URL = os.environ.get("LOCALMEM_CORE_URL", "http://127.0.0.1:7788").rstrip("/")
NO_BROWSER = os.environ.get("DASHBOARD_NO_BROWSER", "").lower() in ("1", "true", "yes")
EXPLICIT_STORES = os.environ.get("LOCALMEM_DASHBOARD_STORES", "")
SCAN_ROOTS = os.environ.get("LOCALMEM_DASHBOARD_SCAN", "")

DASHBOARD_DIR = Path(__file__).resolve().parent
HOME = Path.home()


def candidate_stores() -> list[Path]:
    """All paths to consider as .localmem stores, deduped + sorted."""
    seen: set[Path] = set()
    out: list[Path] = []

    def add(p: Path):
        try:
            r = p.resolve()
        except Exception:
            r = p
        if r in seen:
            return
        seen.add(r)
        out.append(r)

    # Always include the user's global home
    add(HOME / ".localmem")

    # Explicit stores from env
    for raw in EXPLICIT_STORES.split(":"):
        raw = raw.strip()
        if raw:
            add(Path(raw).expanduser())

    # Scan roots — find .localmem dirs one level deep
    scan_paths = [HOME]  # always shallow-scan $HOME (no-op since we add ~/.localmem above)
    for raw in SCAN_ROOTS.split(":"):
        raw = raw.strip()
        if raw:
            scan_paths.append(Path(raw).expanduser())

    for root in scan_paths:
        if not root.is_dir():
            continue
        try:
            for child in root.iterdir():
                if not child.is_dir():
                    continue
                candidate = child / ".localmem"
                if candidate.is_dir():
                    add(candidate)
        except PermissionError:
            continue

    return out


def store_metadata(path: Path) -> dict:
    """Best-effort metadata: line counts, last modified, label."""
    events = path / "events.jsonl"
    meta: dict = {
        "path": str(path),
        "label": _label_for(path),
        "exists": events.exists(),
        "events": 0,
        "size_bytes": 0,
        "last_modified": None,
    }
    if not events.exists():
        return meta
    try:
        stat = events.stat()
        meta["size_bytes"] = stat.st_size
        meta["last_modified"] = stat.st_mtime
        # Cheap line count: events are JSONL, one event per line.
        with events.open("rb") as f:
            meta["events"] = sum(1 for _ in f)
    except Exception as exc:
        meta["error"] = str(exc)
    return meta


def _label_for(path: Path) -> str:
    """Human-readable label. ~/.localmem -> 'global'; else parent-dir name."""
    try:
        if path.resolve() == (HOME / ".localmem").resolve():
            return "global (~/.localmem)"
    except Exception:
        pass
    parent = path.parent.name or "(root)"
    return parent


def stores_payload() -> dict:
    stores = []
    for s in candidate_stores():
        meta = store_metadata(s)
        if meta["exists"]:
            stores.append(meta)
    # Mark which store is the currently-served one by comparing
    # CORE_URL's home (we cannot ask the core for this without a route).
    # As a heuristic, the running core's home is typically ~/.localmem
    # unless launched with --home; the dashboard surfaces the heuristic
    # so the user can correct it if wrong.
    return {
        "ok": True,
        "stores": stores,
        "active_core": CORE_URL,
        "active_guess": str((HOME / ".localmem").resolve()),
    }


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, fmt, *args):
        sys.stderr.write(f"[dashboard] {self.address_string()} - {fmt % args}\n")

    def do_GET(self):
        if self.path.startswith("/__meta/"):
            self._handle_meta()
            return
        if self.path.startswith("/api/"):
            self._proxy("GET")
            return
        super().do_GET()

    def do_POST(self):
        if self.path.startswith("/api/"):
            self._proxy("POST")
            return
        self.send_error(405, "POST only supported on /api/*")

    def do_OPTIONS(self):
        if self.path.startswith("/api/") or self.path.startswith("/__meta/"):
            self.send_response(204)
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
            self.send_header("Access-Control-Allow-Headers", "content-type")
            self.end_headers()
            return
        self.send_error(405)

    def _handle_meta(self):
        # Strip query string for routing
        path = self.path.split("?", 1)[0]
        if path == "/__meta/stores":
            body = json.dumps(stores_payload()).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_error(404, f"unknown meta route: {path}")

    def _proxy(self, method):
        upstream_path = self.path[len("/api"):] or "/"
        upstream_url = f"{CORE_URL}{upstream_path}"
        body = None
        headers = {}
        if method == "POST":
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else b""
            ct = self.headers.get("Content-Type")
            if ct:
                headers["Content-Type"] = ct
        req = urllib.request.Request(upstream_url, data=body, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=15) as resp:
                self.send_response(resp.status)
                for hdr in ("Content-Type", "Content-Length"):
                    val = resp.headers.get(hdr)
                    if val:
                        self.send_header(hdr, val)
                self.end_headers()
                self.wfile.write(resp.read())
        except urllib.error.HTTPError as e:
            self.send_response(e.code)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(e.read() or b'{"ok":false,"error":"upstream error"}')
        except urllib.error.URLError:
            self.send_response(502)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            msg = (
                f'{{"ok":false,"error":{{"code":"core_unreachable",'
                f'"message":"could not reach localmem core at {CORE_URL}. '
                f'Run \\"localmem serve\\" and try again."}}}}'
            )
            self.wfile.write(msg.encode())


def main():
    os.chdir(DASHBOARD_DIR)
    socketserver.TCPServer.allow_reuse_address = True

    print(f"[dashboard] serving {DASHBOARD_DIR} on http://localhost:{PORT}/")
    print(f"[dashboard] proxying /api/* to {CORE_URL}")
    discovered = candidate_stores()
    discovered_existing = [s for s in discovered if (s / "events.jsonl").exists()]
    print(f"[dashboard] discovered {len(discovered_existing)} store(s):")
    for s in discovered_existing:
        print(f"             - {s}")
    print(f"[dashboard] open http://localhost:{PORT}/?api=/api in your browser")
    print(f"[dashboard] Ctrl-C to stop")
    print()

    if not NO_BROWSER:
        try:
            webbrowser.open(f"http://localhost:{PORT}/?api=/api")
        except Exception:
            pass

    with socketserver.TCPServer(("127.0.0.1", PORT), Handler) as httpd:
        try:
            httpd.serve_forever()
        except KeyboardInterrupt:
            print("\n[dashboard] stopped")


if __name__ == "__main__":
    main()
