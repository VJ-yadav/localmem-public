#!/usr/bin/env sh
# localmem install script
#
# Detects OS + architecture, downloads the matching release tarball
# from GitHub Releases, verifies its SHA256 against SHA256SUMS, extracts
# the `localmem` binary into ~/.local/bin/, and prints next steps.
#
# Usage:
#   curl -fsSL https://localmem.co/install | sh
#
# Or with a version pin:
#   curl -fsSL https://localmem.co/install | sh -s -- --version v0.1.0
#
# Environment:
#   LOCALMEM_INSTALL_DIR  override install dir (default: $HOME/.local/bin)
#   LOCALMEM_VERSION      pin to a specific release tag (default: latest)
#   LOCALMEM_REPO         override the GitHub repo (default: VJ-yadav/localmem-public)
#
# Design notes:
#   * POSIX sh, not bash, so the same script works in BusyBox/Alpine.
#   * Idempotent: re-running upgrades in place; bails if the on-disk
#     binary is already the requested version.
#   * Refuses to run as root: the binary belongs in the user's $HOME,
#     not in /usr/local. Per-user installs avoid sudo and avoid the
#     "system-wide binary owns my events.jsonl" trap.
#   * SHA256 verification is mandatory; no -k / --insecure path. A
#     mirror that can't produce the matching checksum is not trusted.

set -eu

# ----- Configuration -------------------------------------------------------

REPO="${LOCALMEM_REPO:-VJ-yadav/localmem-public}"
VERSION="${LOCALMEM_VERSION:-}"
INSTALL_DIR="${LOCALMEM_INSTALL_DIR:-${HOME}/.local/bin}"
BIN_NAME="localmem"
TARBALL_PREFIX="localmem"

# Parse flags. We only recognize --version; everything else is forwarded
# in the error path so a typo is loud.
while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            VERSION="$2"
            shift 2
            ;;
        --version=*)
            VERSION="${1#--version=}"
            shift
            ;;
        -h|--help)
            sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            err "unknown argument: $1"
            ;;
    esac
done

# ----- Helpers --------------------------------------------------------------

# stderr-only logging keeps stdout reserved for any future flag like
# --print-bin that the script may emit (where caller wants only a path).
log()  { printf '\033[36m[localmem]\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[33m[localmem]\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[31m[localmem]\033[0m %s\n' "$*" >&2; exit 1; }

# Refuse root: the binary is per-user state and dropping it into
# /root/.local/bin during a sudo install is almost always a mistake.
if [ "$(id -u)" -eq 0 ]; then
    err "do not run as root. localmem installs into \$HOME and never needs sudo."
fi

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"
}
require_cmd curl
require_cmd tar
require_cmd uname

# ----- OS / arch detection --------------------------------------------------

uname_s="$(uname -s)"
uname_m="$(uname -m)"

case "$uname_s" in
    Darwin)
        case "$uname_m" in
            arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
            x86_64)        TARGET="x86_64-apple-darwin" ;;
            *) err "unsupported macOS architecture: $uname_m" ;;
        esac
        ;;
    Linux)
        case "$uname_m" in
            x86_64)        TARGET="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
            *) err "unsupported Linux architecture: $uname_m" ;;
        esac
        ;;
    *)
        err "unsupported OS: $uname_s. v0.1 ships macOS + Linux only."
        ;;
esac

# ----- Resolve version ------------------------------------------------------

if [ -z "$VERSION" ]; then
    # Hit GitHub's "redirect to latest release" pattern instead of the
    # JSON API to avoid the 60-req/hour unauthenticated rate limit.
    log "resolving latest release for $REPO ..."
    REDIRECT="$(curl -fsSL -o /dev/null -w '%{url_effective}' \
        "https://github.com/${REPO}/releases/latest")"
    VERSION="${REDIRECT##*/}"
    if [ -z "$VERSION" ] || [ "$VERSION" = "latest" ]; then
        err "could not resolve latest release. Pass --version vX.Y.Z explicitly."
    fi
    log "latest: $VERSION"
fi

# Versions in the script are stored with the v-prefix; tarball names
# strip it (cargo-style). Normalize both shapes so users can pass either.
VERSION_NO_V="${VERSION#v}"

TARBALL="${TARBALL_PREFIX}-${VERSION_NO_V}-${TARGET}.tar.gz"
RELEASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
TARBALL_URL="${RELEASE_URL}/${TARBALL}"
SHA_URL="${RELEASE_URL}/SHA256SUMS"

# ----- Idempotency: skip if already at this version -------------------------

if [ -x "$INSTALL_DIR/$BIN_NAME" ]; then
    EXISTING="$("$INSTALL_DIR/$BIN_NAME" --version 2>/dev/null | awk '{print $NF}' || true)"
    if [ -n "$EXISTING" ] && [ "v${EXISTING#v}" = "v${VERSION_NO_V}" ]; then
        log "localmem v${EXISTING#v} already installed at $INSTALL_DIR/$BIN_NAME"
        log "nothing to do."
        exit 0
    fi
fi

# ----- Download + verify ----------------------------------------------------

TMPDIR="$(mktemp -d 2>/dev/null || mktemp -d -t localmem-install)"
trap 'rm -rf "$TMPDIR"' EXIT

log "downloading $TARBALL ..."
if ! curl -fsSL "$TARBALL_URL" -o "$TMPDIR/$TARBALL"; then
    err "download failed: $TARBALL_URL
Check that release $VERSION publishes a tarball for $TARGET."
fi

log "downloading SHA256SUMS ..."
if ! curl -fsSL "$SHA_URL" -o "$TMPDIR/SHA256SUMS"; then
    err "could not fetch SHA256SUMS at $SHA_URL. Refusing to install without a checksum."
fi

# Extract the expected sha for our exact tarball name.
EXPECTED_SHA="$(awk -v want="$TARBALL" '$2 == want || $2 == "*"want { print $1; exit }' "$TMPDIR/SHA256SUMS")"
if [ -z "$EXPECTED_SHA" ]; then
    err "$TARBALL is not listed in SHA256SUMS for release $VERSION."
fi

# Compute the actual sha. Use shasum on macOS (always present), sha256sum on Linux.
if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_SHA="$(sha256sum "$TMPDIR/$TARBALL" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL_SHA="$(shasum -a 256 "$TMPDIR/$TARBALL" | awk '{print $1}')"
else
    err "no sha256sum or shasum available. Cannot verify the download."
fi

if [ "$EXPECTED_SHA" != "$ACTUAL_SHA" ]; then
    err "SHA256 mismatch for $TARBALL.
Expected: $EXPECTED_SHA
Actual:   $ACTUAL_SHA
Refusing to install a tampered tarball."
fi
log "checksum OK"

# ----- Extract + install ----------------------------------------------------

log "extracting ..."
tar -C "$TMPDIR" -xzf "$TMPDIR/$TARBALL"

if [ ! -f "$TMPDIR/$BIN_NAME" ]; then
    # The tarball might wrap the binary in a same-named directory. Find it.
    FOUND="$(find "$TMPDIR" -type f -name "$BIN_NAME" -perm -u+x 2>/dev/null | head -1)"
    if [ -z "$FOUND" ]; then
        err "tarball did not contain a $BIN_NAME executable."
    fi
    cp "$FOUND" "$TMPDIR/$BIN_NAME"
fi

mkdir -p "$INSTALL_DIR"
mv "$TMPDIR/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
chmod +x "$INSTALL_DIR/$BIN_NAME"

# macOS Gatekeeper: a binary downloaded via curl carries the
# `com.apple.quarantine` xattr. First exec then triggers a syspolicyd
# notarization scan that, on an unsigned 150 MB binary, can stall the
# process in `_dyld_start` (state `UE` per `ps`). Once stalled, only
# a reboot frees it. We've already done SHA256 verification above, so
# we strip the quarantine here to skip the scan.
# Once v0.2 ships Developer-ID-signed + notarized binaries, this strip
# becomes belt-and-suspenders rather than load-bearing.
if [ "$(uname -s)" = "Darwin" ] && command -v xattr >/dev/null 2>&1; then
    if xattr -p com.apple.quarantine "$INSTALL_DIR/$BIN_NAME" >/dev/null 2>&1; then
        xattr -d com.apple.quarantine "$INSTALL_DIR/$BIN_NAME" 2>/dev/null || true
        log "cleared macOS quarantine xattr"
    fi
fi

log "installed: $INSTALL_DIR/$BIN_NAME"

# ----- PATH check + next steps ----------------------------------------------

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        warn "$INSTALL_DIR is not in your PATH."
        warn "Add this to ~/.bashrc / ~/.zshrc:"
        warn "    export PATH=\"\$HOME/.local/bin:\$PATH\""
        ;;
esac

cat <<EOF

  ✓ localmem installed.

  Next steps:
    $BIN_NAME init                 # scaffold ~/.localmem/
    $BIN_NAME write --content "Hello, memory."
    $BIN_NAME search "memory"

  For Claude Desktop / Cursor / Codex setup:
    https://github.com/${REPO}/blob/main/docs/CLAUDE_DESKTOP_SETUP.md

EOF
