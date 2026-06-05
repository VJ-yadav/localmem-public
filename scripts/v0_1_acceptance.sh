#!/usr/bin/env bash
# localmem v0.1 acceptance script.
#
# Runs the demo sequence from SPEC.md "Acceptance criteria for v0.1"
# end-to-end against a freshly-built binary and a throwaway home directory.
# Skips the manual Claude Desktop step (step 7 in SPEC) since that's a UI
# loop, not a scriptable check.
#
# Usage:
#   ./scripts/v0_1_acceptance.sh
#
# Exits 0 on full pass, non-zero on first failure. Each step prints its
# command, the expected behavior, and the actual outcome.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP_HOME="$(mktemp -d -t localmem-acceptance.XXXXXX)"
PORT="${LOCALMEM_TEST_PORT:-37788}"
SERVER_ADDR="127.0.0.1:${PORT}"
BIN=""

# Reuse the BGE-small ONNX model cached by the test harness so the hybrid
# retriever has real embeddings during step 4. If the cache directory is
# absent the script still runs, but the search step uses lex-only mode and
# the semantic-recall assertion will be weaker. To populate the cache,
# run `cargo test -p localmem --lib embed::tests::embed_returns_normalized_384_vec`
# once on a network-enabled host before executing this script offline.
TEST_MODEL_DIR="${TMPDIR:-/tmp}/localmem-test-models/bge-small-en-v1.5"
if [ -f "$TEST_MODEL_DIR/model.onnx" ] && [ -f "$TEST_MODEL_DIR/tokenizer.json" ]; then
    export LOCALMEM_MODEL_DIR="$TEST_MODEL_DIR"
    echo "using cached BGE model at $LOCALMEM_MODEL_DIR"
else
    echo "warning: no cached BGE model at $TEST_MODEL_DIR; step 4 will use lex-only fallback"
fi

cleanup() {
    if [ -n "${SERVER_PID:-}" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$TMP_HOME"
}
trap cleanup EXIT

ok()    { printf "\033[32m  PASS\033[0m  %s\n" "$1"; }
fail()  { printf "\033[31m  FAIL\033[0m  %s\n" "$1"; exit 1; }
step()  { printf "\n\033[36m==> %s\033[0m\n" "$*"; }

# -----------------------------------------------------------------------------
# Step 0: build the binary (release mode).
# -----------------------------------------------------------------------------
step "Step 0: build release binary"
(cd "$ROOT/core" && cargo build --release --bin localmem) >/dev/null
BIN="$ROOT/core/target/release/localmem"
[ -x "$BIN" ] || fail "binary not built at $BIN"
ok "binary at $BIN"

run() {
    LOCALMEM_HOME="$TMP_HOME" "$BIN" "$@"
}

# -----------------------------------------------------------------------------
# Step 1: localmem init
# -----------------------------------------------------------------------------
step "Step 1: localmem init creates the home directory tree"
run init
[ -d "$TMP_HOME" ] || fail "home dir not created"
[ -f "$TMP_HOME/events.jsonl" ] || fail "events.jsonl not created"
ok "home tree created at $TMP_HOME"

# -----------------------------------------------------------------------------
# Step 2: localmem write (BEFORE the server starts)
#
# v0.1 CLI write opens the lexical index in writer mode, and so does
# `localmem serve`. Tantivy enforces a single writer per directory, so the
# CLI and the HTTP server cannot share the lexical store concurrently in
# this release. The MCP server path (T-26+) routes writes through HTTP, so
# this constraint disappears once Claude Desktop is the caller. For the
# scripted acceptance we sequence CLI writes first, then bind the server.
# -----------------------------------------------------------------------------
step "Step 2: localmem write ingests a capture"
WRITE_OUT=$(run write \
    --content "I prefer functional Rust and avoid macros where possible." \
    --source repl)
echo "$WRITE_OUT" | grep -q "event_id\|01" || fail "write did not echo an event_id"
ok "capture written"

# -----------------------------------------------------------------------------
# Step 3: localmem serve binds the HTTP server
# -----------------------------------------------------------------------------
step "Step 3: localmem serve binds the HTTP server"
LOCALMEM_HOME="$TMP_HOME" "$BIN" serve --addr "$SERVER_ADDR" >/dev/null 2>&1 &
SERVER_PID=$!
sleep 1

if ! curl -fsS "http://$SERVER_ADDR/health" >/dev/null; then
    fail "server did not respond on $SERVER_ADDR"
fi
ok "server listening on $SERVER_ADDR (pid $SERVER_PID)"

# Free the lexical writer lock before the CLI commands below run.
kill "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true
unset SERVER_PID

# -----------------------------------------------------------------------------
# Step 4: localmem search (hybrid by default after T-24)
#
# The exact query depends on what is available locally. If the BGE model is
# cached, hybrid mode handles the semantic phrasing "code style preferences";
# otherwise we fall back to a BM25-friendly query against a literal token in
# the capture so lex-only mode also surfaces the result.
# -----------------------------------------------------------------------------
step "Step 4: localmem search returns the captured memory"
if [ -n "${LOCALMEM_MODEL_DIR:-}" ]; then
    SEARCH_OUT=$(run search "code style preferences")
else
    SEARCH_OUT=$(run search "functional rust" --mode lex)
fi
echo "$SEARCH_OUT" | grep -qi "functional rust\|prefer" \
    || fail "search did not return the capture text"
ok "search returned the captured preference"

# -----------------------------------------------------------------------------
# Step 5: localmem journal --since 1h
# -----------------------------------------------------------------------------
step "Step 5: localmem journal shows the policy decision"
JOURNAL_OUT=$(run journal --since 1h)
echo "$JOURNAL_OUT" | grep -qE "action=COMMIT|\"action\":\"COMMIT\"|COMMIT" \
    || fail "journal did not contain a COMMIT entry"
ok "journal contains the write decision"

# -----------------------------------------------------------------------------
# Step 6: prove recomputability — delete derived/ and replay
# -----------------------------------------------------------------------------
step "Step 6: localmem replay rebuilds derived stores"
rm -rf "$TMP_HOME/derived"
run replay
[ -d "$TMP_HOME/derived" ] || fail "replay did not recreate derived/"
ok "derived/ rebuilt from events.jsonl"

# -----------------------------------------------------------------------------
# Step 7: search again, expect identical answer
# -----------------------------------------------------------------------------
step "Step 7: search returns the same result after replay"
SEARCH_OUT_2=$(run search "code style preferences")
echo "$SEARCH_OUT_2" | grep -qi "functional rust\|prefer" \
    || fail "search after replay did not return the captured text"
ok "post-replay search is consistent with pre-replay search"

# -----------------------------------------------------------------------------
# Summary
# -----------------------------------------------------------------------------
echo
printf "\033[32m=== v0.1 acceptance: PASS ===\033[0m\n"
echo "All 7 scriptable steps from SPEC.md passed."
echo "Manual step remaining: configure Claude Desktop MCP to point at"
echo "the localmem-mcp binary and verify a recall via Claude."
