// `localmem-mcp install` — the npm-side mirror of T-50.
//
// Goal: one-line bootstrap for users who reach for `npx` before
// `curl | sh`. The flow is:
//
//   1. Detect a localmem binary on PATH (or LOCALMEM_BIN env var).
//   2. If absent: run install.sh from the public release (downloads
//      the matching tarball into ~/.local/bin, verifies SHA256).
//   3. Dispatch to `localmem mcp install <client>` so the actual
//      config write is done by the Rust binary — single source of
//      truth, no duplication.
//   4. Print next steps for the user (start `localmem serve`, etc.).
//
// We intentionally don't reimplement the install.sh download flow
// in TS: that script is the canonical install path on Linux/macOS,
// SHA-verifies the tarball, and handles macOS Gatekeeper
// quarantine. Calling it via `sh -s` keeps both flows in sync.
//
// Surface:
//   npx localmem-mcp install --client claude
//   npx localmem-mcp install --client cursor --bin /path/to/localmem
//   npx localmem-mcp install --help

import { spawn, spawnSync } from "node:child_process"
import { existsSync } from "node:fs"
import { join } from "node:path"
import { homedir } from "node:os"

interface ParsedArgs {
  client?: string
  binOverride?: string
  skipDownload: boolean
  help: boolean
}

const SUPPORTED_CLIENTS = [
  "claude",
  "claude-code",
  "cursor",
  "windsurf",
  "cline",
  "codex",
  "aider",
]

/// `localmem-mcp install ...` is dispatched from `src/index.ts` when
/// argv[2] is `install`. We re-parse argv here because the MCP SDK's
/// stdio loop in the main entry doesn't have an argument parser.
export async function runInstall(argv: string[]): Promise<void> {
  const args = parseArgs(argv)
  if (args.help) {
    printHelp()
    return
  }
  // --client is OPTIONAL: `localmem setup` auto-detects and wires every client
  // it finds. A named client is only a hint we additionally guarantee below.
  if (args.client && !SUPPORTED_CLIENTS.includes(args.client)) {
    fail(
      `unknown client ${JSON.stringify(args.client)}. ` +
        `Supported: ${SUPPORTED_CLIENTS.join(", ")}`,
    )
  }

  const bin = await resolveLocalmemBinary(args)
  log(`localmem binary: ${bin}`)

  // ONE install path: run the FULL `localmem setup` (init + fetch model + start
  // the always-on service + wire detected clients + verify), so an npm install
  // lands in the SAME complete state as `curl | sh && localmem setup`. Any new
  // onboarding step lives in `setup`, never in a second command. The Rust CLI
  // streams the shared onboarding status (§8) via stdio:inherit.
  log("running localmem setup ...")
  const setup = spawnSync(bin, ["setup"], { stdio: "inherit", env: process.env })
  if (setup.status !== 0) {
    fail(`\`${bin} setup\` exited ${setup.status ?? "?"}`)
  }

  // If a specific client was named, guarantee it is wired even if setup did not
  // auto-detect it. Idempotent (mcp install backs up + rewrites one entry).
  if (args.client) {
    log(`ensuring ${args.client} is wired ...`)
    const wire = spawnSync(bin, ["mcp", "install", args.client], {
      stdio: "inherit",
      env: process.env,
    })
    if (wire.status !== 0) {
      fail(`\`${bin} mcp install ${args.client}\` exited ${wire.status ?? "?"}`)
    }
  }

  log("done.")
}

function parseArgs(argv: string[]): ParsedArgs {
  const out: ParsedArgs = { skipDownload: false, help: false }
  // argv shape from `npx localmem-mcp install --client ...`:
  //   argv[0] = node, argv[1] = .../localmem-mcp, argv[2] = install,
  //   argv[3..] = installer flags. Strip the leading three.
  const rest = argv.slice(3)
  for (let i = 0; i < rest.length; i++) {
    const arg = rest[i]!
    switch (arg) {
      case "--client":
        out.client = rest[++i]
        break
      case "-h":
      case "--help":
        out.help = true
        break
      case "--bin":
        out.binOverride = rest[++i]
        break
      case "--skip-download":
        out.skipDownload = true
        break
      default:
        if (arg.startsWith("--client=")) {
          out.client = arg.slice("--client=".length)
        } else if (arg.startsWith("--bin=")) {
          out.binOverride = arg.slice("--bin=".length)
        } else {
          fail(`unknown argument: ${arg}. Try --help.`)
        }
    }
  }
  return out
}

function printHelp(): void {
  const text =
    "localmem-mcp install — wire localmem into your MCP-compatible AI tool\n" +
    "\n" +
    "Usage:\n" +
    "  npx localmem-mcp install --client <name> [options]\n" +
    "\n" +
    "Clients:\n" +
    "  " +
    SUPPORTED_CLIENTS.join(", ") +
    "\n" +
    "\n" +
    "Options:\n" +
    "  --client <name>     The MCP client to configure (required).\n" +
    "  --bin <path>        Path to an existing localmem binary. Overrides\n" +
    "                      PATH + LOCALMEM_BIN; skips the download step.\n" +
    "  --skip-download     Refuse to fetch a binary; fail if none is on PATH.\n" +
    "  -h, --help          Show this help.\n"
  process.stdout.write(text + "\n")
}

async function resolveLocalmemBinary(args: ParsedArgs): Promise<string> {
  // Order of preference: --bin > $LOCALMEM_BIN > PATH lookup > download.
  if (args.binOverride) {
    if (!existsSync(args.binOverride)) {
      fail(`--bin path does not exist: ${args.binOverride}`)
    }
    return args.binOverride
  }
  const envBin = process.env.LOCALMEM_BIN
  if (envBin && existsSync(envBin)) {
    return envBin
  }
  const onPath = which("localmem")
  if (onPath) return onPath

  if (args.skipDownload) {
    fail(
      "no localmem binary found on PATH and --skip-download was set. " +
        "Install localmem first (https://localmem.co/install) or rerun without --skip-download.",
    )
  }

  return downloadLocalmem()
}

function which(cmd: string): string | undefined {
  // `command -v` is POSIX. We pipe to /bin/sh -c so the lookup works
  // on macOS/Linux without depending on `which` (which isn't part of
  // POSIX). `node:child_process.spawnSync` is fine here because the
  // installer flow is synchronous from the user's perspective.
  const r = spawnSync("/bin/sh", ["-c", `command -v ${cmd}`], { encoding: "utf-8" })
  if (r.status === 0) {
    const path = r.stdout.trim()
    if (path) return path
  }
  return undefined
}

async function downloadLocalmem(): Promise<string> {
  // We delegate to the canonical install.sh (which lives at the
  // public install URL). Streaming via `curl | sh` keeps the SHA
  // verification + Gatekeeper-quarantine handling in one place; we
  // would otherwise have to re-implement them here and drift.
  log("no localmem binary found. Downloading via install.sh ...")
  const installScriptUrl =
    process.env.LOCALMEM_INSTALL_SCRIPT_URL || "https://localmem.co/install"
  // Stream curl → sh. spawnSync with shell: true keeps the pipeline
  // semantics; both stdio streams inherit so the user sees the
  // progress lines install.sh writes to stderr.
  const cmd = `curl -fsSL ${shellQuote(installScriptUrl)} | sh`
  const r = spawnSync("/bin/sh", ["-c", cmd], { stdio: "inherit", env: process.env })
  if (r.status !== 0) {
    fail(
      `localmem download failed (exit ${r.status ?? "?"}). ` +
        `Try running the install script manually: ${installScriptUrl}`,
    )
  }
  // install.sh writes into $HOME/.local/bin by default.
  const expected = join(homedir(), ".local", "bin", "localmem")
  if (!existsSync(expected)) {
    fail(`install.sh did not produce a binary at ${expected}.`)
  }
  // Add ~/.local/bin to PATH for any subsequent spawn in this
  // process so `mcp install` finds the binary even if the user's
  // shell config hasn't been re-sourced.
  process.env.PATH = `${join(homedir(), ".local", "bin")}:${process.env.PATH ?? ""}`
  return expected
}

function clientDisplayName(slug: string): string {
  switch (slug) {
    case "claude":
      return "Claude Desktop"
    case "claude-code":
      return "Claude Code"
    case "cursor":
      return "Cursor"
    case "windsurf":
      return "Windsurf"
    case "cline":
      return "Cline"
    case "codex":
      return "Codex"
    case "aider":
      return "Aider"
    default:
      return slug
  }
}

function shellQuote(s: string): string {
  // Single-quote whatever's inside, escaping any embedded single quotes.
  return "'" + s.replace(/'/g, "'\\''") + "'"
}

function log(msg: string): void {
  process.stderr.write(`\x1b[36m[localmem-mcp]\x1b[0m ${msg}\n`)
}

function fail(msg: string): never {
  process.stderr.write(`\x1b[31m[localmem-mcp] error:\x1b[0m ${msg}\n`)
  process.exit(1)
}

// Silence unused-var warning for `spawn` (we use spawnSync above; the
// async `spawn` import is kept in scope for future expansion to
// non-blocking download progress).
void spawn
