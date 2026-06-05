# Installing localmem

## TL;DR

```sh
curl -fsSL https://localmem.co/install | sh
```

That command downloads the right release binary for your OS and CPU,
verifies its SHA256, and drops it into `~/.local/bin/localmem`.

It refuses to run as root. It needs `curl`, `tar`, and one of
`sha256sum` / `shasum`. All three ship by default on macOS and every
mainstream Linux distribution.

## What it installs

`~/.local/bin/localmem` — the Rust core binary. Everything else
(`localmem-mcp`, the BGE-small ONNX assets) is installed by `localmem`
itself once you run `localmem init`.

`localmem` will not write to `/usr/local` or anywhere outside `$HOME`.
This is deliberate: the binary is per-user state, never a system
service.

## Verifying without piping to sh

If you don't want to pipe curl to a shell, download and read first:

```sh
curl -fsSL https://localmem.co/install -o install.sh
less install.sh                # read what you're about to run
sh install.sh
```

Every download in `install.sh` is verified against the release's
`SHA256SUMS` manifest. A failed checksum aborts the install — there is
no `--insecure` flag.

## Pinning a version

```sh
curl -fsSL https://localmem.co/install | sh -s -- --version v0.1.0
```

Or via env var:

```sh
LOCALMEM_VERSION=v0.1.0 curl -fsSL https://localmem.co/install | sh
```

## Custom install directory

```sh
LOCALMEM_INSTALL_DIR="$HOME/bin" curl -fsSL https://localmem.co/install | sh
```

## Supported platforms

| OS | Architecture | Status |
|---|---|---|
| macOS | Apple Silicon (M1/M2/M3/M4) | ✅ supported |
| macOS | Intel (x86_64) | ✅ supported |
| Linux | x86_64 | ✅ supported |
| Linux | aarch64 | ✅ supported |
| Windows | any | ⏳ planned for a future release |

## Updating

Re-run the install command. It's idempotent and a no-op if the same
version is already present.

```sh
curl -fsSL https://localmem.co/install | sh
```

To downgrade or pin, use `--version`.

## Uninstalling

```sh
rm "$(command -v localmem)"
rm -rf ~/.localmem          # only if you want to discard your memory
```

`~/.localmem/` is your data. The install script never touches it.

## Building from source

If you'd rather build the binary yourself (no network call to GitHub
Releases), you need a Rust toolchain (stable 1.83+):

```sh
git clone https://github.com/VJ-yadav/localmem-public
cd localmem/core
cargo build --release
cp target/release/localmem ~/.local/bin/
```

The build downloads the ONNX Runtime shared library and compiles
DuckDB from source via the `bundled` feature, so the resulting binary
needs no system-level libraries to run. Expect ~10 minutes on first
build, ~30s on incremental rebuilds.

Tests:

```sh
cargo test --lib
```

The full library suite passes 221+ tests locally as of `ca3ac5a`. The
embedding tests download the BGE-small ONNX weights from HuggingFace
on first run and cache them under `$TMPDIR/localmem-test-models/`; they
skip cleanly on hosts without network access.

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
`https://github.com/VJ-yadav/localmem-public/issues` with the version and
your platform.

**"no such release tag" / 404 on the tarball**
The version you asked for doesn't ship a binary for your platform.
Check the [Releases page](https://github.com/VJ-yadav/localmem-public/releases)
to see which targets are available. As of v0.1 only macOS arm64/x86_64
and Linux x86_64/aarch64 ship.

**Anti-virus quarantining the binary on first run (macOS)**
Until v0.2 ships Developer-ID-signed + notarized binaries, the install
script handles this automatically by stripping the
`com.apple.quarantine` xattr after SHA256 verification (see install.sh
right after `mv`). If you somehow end up with a quarantined binary
anyway, the manual fix is one line:

```sh
xattr -d com.apple.quarantine ~/.local/bin/localmem
```

If you see processes stuck in state `UE` (uninterruptible + exit
pending) with 0 CPU time after first exec, that's macOS syspolicyd
stalling on a notarization scan of an unsigned 150 MB binary. Only a
reboot frees them. Once freed, the quarantine fix above plus
`sudo spctl --add ~/.local/bin/localmem` prevents recurrence.
