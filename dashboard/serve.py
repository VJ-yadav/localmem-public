#!/usr/bin/env python3
"""
dashboard/serve.py — supervisor + proxy for the localmem dashboard.

Manages a single `localmem serve` subprocess and a static-files +
reverse-proxy HTTP server in one process. Adds /__meta routes so the
dashboard can discover all .localmem stores on the machine and
*live-switch* the active one (kills + restarts the core subprocess
behind the scenes).

Routes served by this process:
    GET  /                       static files from dashboard/
    GET  /__meta/stores          list of .localmem dirs on this machine
    GET  /__meta/active          which home the supervised core is on
    POST /__meta/switch          body {home: "..."}, swap active home
    GET  /api/<path>             reverse-proxy to the localmem core
    POST /api/<path>             reverse-proxy to the localmem core

Why this design (vs. Rust `localmem serve --dashboard`):
- Zero Rust changes => no rebuild, no release cut.
- Live switching is "stop subprocess; start subprocess" — bounded by
  the core's startup time (~1s).
- The same UX (one-command dashboard) ports cleanly to a future
  Rust-native dashboard server when that's prioritized.

Usage:
    python3 dashboard/serve.py
    # then open http://localhost:8088/?api=/api

Env vars:
    LOCALMEM_BIN               default: localmem on PATH (override with
                               an absolute path to the binary)
    LOCALMEM_DEFAULT_HOME      default: ~/.localmem
    LOCALMEM_CORE_ADDR         default: 127.0.0.1:7788
    DASHBOARD_PORT             default: 8088
    DASHBOARD_NO_BROWSER       set to 1 to skip the auto-open
    LOCALMEM_DASHBOARD_STORES  colon-separated list of .localmem dirs
                               to surface explicitly
    LOCALMEM_DASHBOARD_SCAN    colon-separated roots to scan one level
                               deep for .localmem subdirs

Stop with Ctrl-C.
"""

from __future__ import annotations

import http.server
import json
import os
import shlex
import shutil
import signal
import socketserver
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import webbrowser
from pathlib import Path

# ---- Config ----------------------------------------------------------------

PORT = int(os.environ.get("DASHBOARD_PORT", "8088"))
CORE_ADDR = os.environ.get("LOCALMEM_CORE_ADDR", "127.0.0.1:7788")
CORE_URL = f"http://{CORE_ADDR}"
LOCALMEM_BIN = os.environ.get("LOCALMEM_BIN") or shutil.which("localmem") or "localmem"
DEFAULT_HOME = Path(os.environ.get("LOCALMEM_DEFAULT_HOME") or Path.home() / ".localmem").expanduser()
NO_BROWSER = os.environ.get("DASHBOARD_NO_BROWSER", "").lower() in ("1", "true", "yes")
EXPLICIT_STORES = os.environ.get("LOCALMEM_DASHBOARD_STORES", "")
SCAN_ROOTS = os.environ.get("LOCALMEM_DASHBOARD_SCAN", "")

DASHBOARD_DIR = Path(__file__).resolve().parent
HOME = Path.home()

# ---- Core subprocess supervisor -------------------------------------------

class CoreSupervisor:
    """
    Owns a single `localmem serve --home <H> --addr <ADDR>` subprocess.

    Thread-safety: switch() acquires a lock so two concurrent dashboard
    requests can't race a restart. Health probing is best-effort.
    """

    def __init__(self, home: Path):
        self.home = Path(home).expanduser().resolve()
        self.proc: subprocess.Popen | None = None
        self._lock = threading.Lock()

    def start(self) -> tuple[bool, str]:
        with self._lock:
            if self.proc and self.proc.poll() is None:
                return True, "already running"
            return self._spawn_locked(self.home)

    def _spawn_locked(self, home: Path, auto_heal: bool = True) -> tuple[bool, str]:
        # Pre-existing core on the same port? Don't double-bind.
        if _is_core_reachable():
            log(f"core already reachable at {CORE_URL} — attaching, not spawning")
            self.proc = None
            self.home = home
            return True, "attached to existing core (home unverified)"

        # Wait for the port to be free if a previous core just exited.
        # macOS holds TCP sockets briefly post-close; this keeps the new
        # bind() from failing with EADDRINUSE.
        if not _wait_for_port_free(CORE_ADDR, timeout=5.0):
            log(f"port {CORE_ADDR} still in use after wait; will try anyway")

        ok, msg = self._spawn_once(home)
        if ok:
            return True, msg

        # Auto-heal: T-81 made the core report stale lexical schema as
        # an actionable error. If we see that, run `localmem replay`
        # on the home and retry the spawn. This eliminates the common
        # "old project home created before v0.2" failure mode.
        if auto_heal and _is_stale_schema_error(msg):
            log(f"stale schema detected; running 'localmem replay --home {home}' to auto-heal")
            healed = self._run_replay(home)
            if healed:
                log("replay succeeded; retrying spawn")
                ok2, msg2 = self._spawn_once(home)
                if ok2:
                    return True, f"{msg2} (auto-healed stale schema)"
                return False, f"replay succeeded but second spawn failed: {msg2}"
            return False, f"stale schema + auto-replay failed. Original: {msg}"

        return False, msg

    def _spawn_once(self, home: Path) -> tuple[bool, str]:
        cmd = [str(LOCALMEM_BIN), "serve", "--home", str(home), "--addr", CORE_ADDR]
        log(f"spawning: {' '.join(shlex.quote(a) for a in cmd)}")
        # Capture stderr to a temp file so the user sees the real error
        # if the core dies before it becomes healthy. Without this we
        # only know "it didn't come up in time," not WHY.
        import tempfile
        self._stderr_log = tempfile.NamedTemporaryFile(
            prefix="localmem-core-stderr-", suffix=".log", delete=False, mode="wb",
        )
        try:
            self.proc = subprocess.Popen(
                cmd,
                stdout=subprocess.DEVNULL,
                stderr=self._stderr_log,
                start_new_session=True,
            )
        except FileNotFoundError:
            return False, f"localmem binary not found at: {LOCALMEM_BIN}"
        except Exception as e:
            return False, f"spawn failed: {e}"

        # Poll /health for up to 30 seconds. Cold release builds with LTO
        # can take ~5-10s to bind, and a back-to-back start after a stop
        # may be slower (Tantivy + LanceDB initialization).
        timeout = 30.0
        if _wait_for_core(timeout=timeout):
            self.home = home
            return True, f"core up on {CORE_URL} (home={home})"

        # It never came up. Capture whatever the subprocess wrote and
        # include it in the error so the dashboard can show the user.
        self._stop_locked()
        last = _tail_stderr_log(getattr(self, "_stderr_log", None))
        snippet = ("\n".join(last[-6:]) or "no stderr").strip()
        return False, f"core did not become healthy in {timeout:.0f}s. stderr:\n{snippet}"

    def _run_replay(self, home: Path) -> bool:
        """Run `localmem replay --home <H>` synchronously; return True on success."""
        cmd = [str(LOCALMEM_BIN), "replay", "--home", str(home)]
        try:
            res = subprocess.run(
                cmd,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                timeout=120.0,
            )
            if res.returncode != 0:
                tail = res.stderr.decode("utf-8", errors="replace").strip().splitlines()[-3:]
                log(f"replay failed (exit {res.returncode}): {' / '.join(tail)}")
                return False
            return True
        except subprocess.TimeoutExpired:
            log("replay timed out after 120s")
            return False
        except Exception as e:
            log(f"replay exception: {e}")
            return False

    def switch(self, new_home: Path) -> tuple[bool, str]:
        new_home = Path(new_home).expanduser()
        if not new_home.is_dir():
            return False, f"not a directory: {new_home}"
        if not (new_home / "events.jsonl").exists():
            return False, f"no events.jsonl at {new_home} (run `localmem init` first)"
        new_home = new_home.resolve()

        with self._lock:
            if new_home == self.home and self.proc and self.proc.poll() is None:
                return True, f"already on {new_home}"
            log(f"switch: {self.home} -> {new_home}")
            self._stop_locked()
            return self._spawn_locked(new_home)

    def _stop_locked(self):
        if not self.proc:
            return
        try:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                log("core didn't stop in 5s, sending SIGKILL")
                self.proc.kill()
                self.proc.wait(timeout=2.0)
        except Exception as e:
            log(f"stop error (ignoring): {e}")
        self.proc = None

    def stop(self):
        with self._lock:
            self._stop_locked()


def _is_core_reachable() -> bool:
    try:
        with urllib.request.urlopen(f"{CORE_URL}/health", timeout=0.4) as r:
            return r.status == 200
    except Exception:
        return False


def _wait_for_core(timeout: float) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if _is_core_reachable():
            return True
        time.sleep(0.2)
    return False


def _wait_for_port_free(addr: str, timeout: float) -> bool:
    """Poll until TCP bind would succeed on addr. Returns True if free."""
    import socket
    host, port = addr.split(":", 1)
    port = int(port)
    deadline = time.time() + timeout
    while time.time() < deadline:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            s.bind((host, port))
            s.close()
            return True
        except OSError:
            time.sleep(0.2)
        finally:
            try: s.close()
            except: pass
    return False


def _is_stale_schema_error(msg: str) -> bool:
    """T-81's actionable drift message is the trigger for auto-replay."""
    if not msg:
        return False
    low = msg.lower()
    return ("schema is stale" in low) or ("schema does not match" in low) or ("run: localmem replay" in low)


def _tail_stderr_log(handle, n_lines: int = 60) -> list[str]:
    """Read the last N lines of a tempfile we wrote subprocess stderr into."""
    if not handle:
        return []
    try:
        handle.flush()
        with open(handle.name, "rb") as f:
            raw = f.read()
        return [line for line in raw.decode("utf-8", errors="replace").splitlines() if line.strip()][-n_lines:]
    except Exception:
        return []


def log(msg: str):
    sys.stderr.write(f"[dashboard] {msg}\n")
    sys.stderr.flush()


# ---- Store discovery ------------------------------------------------------

def candidate_stores() -> list[Path]:
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

    add(HOME / ".localmem")

    for raw in EXPLICIT_STORES.split(":"):
        raw = raw.strip()
        if raw:
            add(Path(raw).expanduser())

    scan_paths: list[Path] = []
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
        with events.open("rb") as f:
            meta["events"] = sum(1 for _ in f)
    except Exception as exc:
        meta["error"] = str(exc)
    return meta


def _label_for(path: Path) -> str:
    try:
        if path.resolve() == (HOME / ".localmem").resolve():
            return "global (~/.localmem)"
    except Exception:
        pass
    parent = path.parent.name or "(root)"
    return parent


# ---- HTTP handler ----------------------------------------------------------

SUPERVISOR: CoreSupervisor  # set in main()


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, fmt, *args):
        sys.stderr.write(f"[dashboard] {self.address_string()} - {fmt % args}\n")

    def do_GET(self):
        if self.path.startswith("/__meta/"):
            self._handle_meta_get()
            return
        if self.path.startswith("/api/"):
            self._proxy("GET")
            return
        super().do_GET()

    def do_POST(self):
        if self.path.startswith("/__meta/"):
            self._handle_meta_post()
            return
        if self.path.startswith("/api/"):
            self._proxy("POST")
            return
        self.send_error(405, "POST only supported on /api/* and /__meta/*")

    def do_OPTIONS(self):
        if self.path.startswith("/api/") or self.path.startswith("/__meta/"):
            self.send_response(204)
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
            self.send_header("Access-Control-Allow-Headers", "content-type")
            self.end_headers()
            return
        self.send_error(405)

    # ---- /__meta/* -------------------------------------------------------

    def _handle_meta_get(self):
        path = self.path.split("?", 1)[0]
        if path == "/__meta/stores":
            return self._send_json(200, self._stores_payload())
        if path == "/__meta/active":
            return self._send_json(200, {
                "ok": True,
                "active_home": str(SUPERVISOR.home),
                "core_url": CORE_URL,
                "managed": SUPERVISOR.proc is not None,
            })
        self.send_error(404, f"unknown meta route: {path}")

    def _handle_meta_post(self):
        path = self.path.split("?", 1)[0]
        if path == "/__meta/switch":
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
            try:
                body = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                return self._send_json(400, {"ok": False, "error": "invalid JSON body"})
            home = body.get("home")
            if not home:
                return self._send_json(400, {"ok": False, "error": "missing 'home' field"})
            ok, msg = SUPERVISOR.switch(Path(home))
            return self._send_json(200 if ok else 502, {
                "ok": ok,
                "message": msg,
                "active_home": str(SUPERVISOR.home),
            })
        self.send_error(404, f"unknown meta route: {path}")

    def _stores_payload(self) -> dict:
        stores = []
        for s in candidate_stores():
            meta = store_metadata(s)
            if meta["exists"]:
                stores.append(meta)
        return {
            "ok": True,
            "stores": stores,
            "active_core": CORE_URL,
            "active_home": str(SUPERVISOR.home),
        }

    def _send_json(self, status: int, payload: dict):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # ---- /api/* proxy ----------------------------------------------------

    def _proxy(self, method: str):
        upstream_path = self.path[len("/api"):] or "/"
        upstream_url = f"{CORE_URL}{upstream_path}"
        body = None
        headers: dict[str, str] = {}
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
                f'"message":"localmem core unreachable at {CORE_URL}. '
                f'It may be restarting after a store-switch."}}}}'
            )
            self.wfile.write(msg.encode())


# ---- Main ------------------------------------------------------------------

def main():
    global SUPERVISOR
    os.chdir(DASHBOARD_DIR)
    socketserver.TCPServer.allow_reuse_address = True

    print(f"[dashboard] dashboard dir: {DASHBOARD_DIR}")
    print(f"[dashboard] core binary:   {LOCALMEM_BIN}")
    print(f"[dashboard] default home:  {DEFAULT_HOME}")
    print(f"[dashboard] core addr:     {CORE_URL}")
    print(f"[dashboard] dashboard at:  http://localhost:{PORT}/?api=/api")
    print()

    SUPERVISOR = CoreSupervisor(DEFAULT_HOME)
    ok, msg = SUPERVISOR.start()
    if ok:
        log(f"core ready: {msg}")
    else:
        log(f"WARNING: core failed to start: {msg}")
        log(f"         the dashboard will still serve, but live switching may be the only way to recover")

    discovered = [s for s in candidate_stores() if (s / "events.jsonl").exists()]
    log(f"discovered {len(discovered)} store(s):")
    for s in discovered:
        log(f"   - {s}")

    if not NO_BROWSER:
        try:
            webbrowser.open(f"http://localhost:{PORT}/")
        except Exception:
            pass

    def shutdown(signum, frame):
        log(f"received signal {signum}, stopping core and exiting")
        SUPERVISOR.stop()
        sys.exit(0)

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)

    with socketserver.TCPServer(("127.0.0.1", PORT), Handler) as httpd:
        try:
            httpd.serve_forever()
        except KeyboardInterrupt:
            pass
        finally:
            SUPERVISOR.stop()
            log("stopped")


if __name__ == "__main__":
    main()
