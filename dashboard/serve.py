#!/usr/bin/env python3
"""
dashboard/serve.py — one-shot helper to view the localmem dashboard.

Serves dashboard/ as static files on http://localhost:8088/ and
transparently forwards /api/* to the running localmem core on
http://127.0.0.1:7788. Browser sees a single origin so CORS does not
get in the way.

Usage:
    python3 dashboard/serve.py
    # then open http://localhost:8088/ (auto-opens by default)

Optional env vars:
    DASHBOARD_PORT       defaults to 8088
    LOCALMEM_CORE_URL    defaults to http://127.0.0.1:7788
    DASHBOARD_NO_BROWSER set to 1 to skip the auto-open

Stop with Ctrl-C.

This script ships with the dashboard for v0.2.1. v0.2.2 plan is to
have `localmem serve --dashboard` host the UI on the same port as the
API natively, at which point this helper goes away.
"""

import http.server
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

DASHBOARD_DIR = Path(__file__).resolve().parent


class Handler(http.server.SimpleHTTPRequestHandler):
    # Quiet down the default logger; one line per request is plenty.
    def log_message(self, fmt, *args):
        sys.stderr.write(f"[dashboard] {self.address_string()} - {fmt % args}\n")

    def do_GET(self):
        if self.path.startswith("/api/"):
            self._proxy("GET")
            return
        # Fall through to static file serving.
        super().do_GET()

    def do_POST(self):
        if self.path.startswith("/api/"):
            self._proxy("POST")
            return
        self.send_error(405, "POST only supported on /api/*")

    def do_OPTIONS(self):
        if self.path.startswith("/api/"):
            # The Rust core does not advertise CORS; we are the same
            # origin as the page, so the browser will not actually
            # send preflight. But some setups (e.g. fetch with custom
            # headers under certain Chrome versions) still try. Answer
            # with a generous allow.
            self.send_response(204)
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
            self.send_header("Access-Control-Allow-Headers", "content-type")
            self.end_headers()
            return
        self.send_error(405)

    def _proxy(self, method):
        # /api/foo  ->  CORE_URL/foo
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
                # Forward content-type so JSON parses on the client side.
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
        except urllib.error.URLError as e:
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
    # Allow rapid restarts; default SimpleHTTPRequestHandler leaves
    # the port in TIME_WAIT and refuses to bind on re-run.
    socketserver.TCPServer.allow_reuse_address = True

    print(f"[dashboard] serving {DASHBOARD_DIR} on http://localhost:{PORT}/")
    print(f"[dashboard] proxying /api/* to {CORE_URL}")
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
