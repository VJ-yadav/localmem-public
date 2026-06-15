# Installing localmem

## TL;DR

```sh
curl -fsSL https://localmem.org/install | sh
```

That command downloads the right release binary for your OS and CPU,
verifies its SHA256 against the published manifest, and drops it into
`~/.local/bin/localmem`.

It refuses to run as root. It needs `curl`, `tar`, and one of
`sha256sum` / `shasum` — all of which ship by default on macOS and
every mainstream Linux distribution.

## What it installs

`~/.local/bin/localmem` — the Rust core binary. Everything else
(the BGE-small ONNX embedding model, the MCP wiring for each AI
client, the per-home init) is set up by `localmem` itself once you run
`localmem init`.

`localmem` will not write to `/usr/local` or anywhere outside `$HOME`.
This is deliberate: the binary is per-user state, never a system
service.

## Verifying without piping to sh

If you don't want to pipe curl to a shell, download and read first:

```sh
curl -fsSL https://localmem.org/install -o install.sh
less install.sh                # read what you are about to run
sh install.sh
```

Every download in `install.sh` is verified against the release's
`SHA256SUMS` manifest. A failed checksum aborts the install — there is
no `--insecure` flag.

## Pinning a version

```sh
curl -fsSL https://github.com/VJ-yadav/localmem-community/releases/download/v0.2.0/install.sh | sh -s -- --version v0.2.0
```

Or via env var:

```sh
LOCALMEM_VERSION=v0.2.0 curl -fsSL https://localmem.org/install | sh
```

## Custom install directory

```sh
LOCALMEM_INSTALL_DIR="$HOME/bin" curl -fsSL https://localmem.org/install | sh
```

## Supported platforms (v0.2.0)

| OS | Architecture | Status |
|---|---|---|
| macOS | Apple Silicon (M1/M2/M3/M4) | ✅ prebuilt binary |
| macOS | Intel (x86_64) | ⏳ build from source for now |
| Linux | x86_64 | ⏳ build from source |
| Linux | aarch64 | ⏳ build from source |
| Windows | any | ⏳ planned for a future release |

Cross-compiled binaries for Intel Mac + Linux ship in a follow-up
release once the CI release workflow is online. Building from source
in the meantime is straightforward — see "Building from source" below.

## Updating

Re-run the install command. It is idempotent and a no-op if the same
version is already present.

```sh
curl -fsSL https://localmem.org/install | sh
```

To downgrade or pin, use `--version` as shown above.

## Uninstalling

```sh
rm "$(command -v localmem)"
rm -rf ~/.localmem          # only if you want to discard your memory
```

`~/.localmem/` is your data. The install script never touches it; you
have to remove it yourself.

## Building from source

If you are on a platform without a prebuilt binary, or you would rather
build the binary yourself (no network call to GitHub Releases), you
need a Rust toolchain (stable 1.83+):

```sh
git clone https://github.com/VJ-yadav/localmem-community
cd localmem-community/core
cargo build --release
cp target/release/localmem ~/.local/bin/
```

The build downloads the ONNX Runtime shared library and compiles
DuckDB from source via the `bundled` feature, so the resulting binary
needs no system-level libraries to run. Expect ~5–10 minutes on first
build, ~30s on incremental rebuilds.

Tests:

```sh
cargo test --lib
```

## Troubleshooting

**"missing required command: curl/tar"**

Install the missing command via your package manager. macOS ships both
by default; Linux distros may need `apt-get install curl tar` or
`dnf install curl tar`.

**"$HOME/.local/bin is not in your PATH"**

Add this line to `~/.bashrc` or `~/.zshrc`:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Then reload: `source ~/.bashrc` or open a new shell.

**"SHA256 mismatch"**

Something tampered with the tarball between GitHub Releases and your
machine. Stop. Don't bypass. File an issue at
`https://github.com/VJ-yadav/localmem-community/issues` with the version
and your platform.

**"no such release tag" / 404 on the tarball**

The version you asked for doesn't ship a binary for your platform.
Check the [Releases page](https://github.com/VJ-yadav/localmem-community/releases)
to see which targets are available. As of v0.2.0 only macOS arm64
ships as a prebuilt; other platforms build from source.

**Anti-virus quarantining the binary on first run (macOS)**

The binary is unsigned in v0.2.0. The install script handles this
automatically by stripping the `com.apple.quarantine` xattr after
SHA256 verification. If you somehow end up with a quarantined binary
anyway (e.g., copied the binary manually), the manual fix is one
line:

```sh
xattr -d com.apple.quarantine ~/.local/bin/localmem
```

If you see processes stuck in state `UE` (uninterruptible + exit
pending) with 0 CPU time after first exec, that's macOS syspolicyd
stalling on a notarization scan of an unsigned ~150 MB binary. Only a
reboot frees them. Once freed, the quarantine fix above plus
`sudo spctl --add ~/.local/bin/localmem` prevents recurrence.
Developer-ID-signed + notarized binaries are on the roadmap for a
future release.
