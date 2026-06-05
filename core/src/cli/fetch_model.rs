//! `localmem fetch-model [name]` handler (T-62).
//!
//! Downloads named ML model files into `<home>/models/<slug>/` so the
//! embedder + future local-LLM extractor + rewriter can find them at
//! runtime. Today's only consumer is the BGE-small embedder; LLM
//! models (Llama 3.2 3B, Qwen 2.5 7B) are registered as future
//! consumers (T-58 + T-62) but harmless to pre-fetch.
//!
//! Discipline (per CLAUDE.md "no bandaid"):
//! - **Idempotent.** If files exist AND verify against the registry's
//!   SHA256, skip with a clear message; we don't re-download just to
//!   prove we still can.
//! - **SHA256 verification when armed.** Each known file in the
//!   registry carries an expected SHA256. Empty hash = "not yet
//!   armed" (the value is TBD until we generate a verified hash from
//!   a clean download). Empty-hash entries log a WARN at fetch time
//!   so the user knows verification didn't run — explicit state, not
//!   a silent skip.
//! - **Disk-space check.** Refuse before downloading if free disk is
//!   less than 2× the model's total declared size.
//! - **`--dry-run`** prints what would happen without downloading.
//! - **Atomic file landing.** Download to a `.partial` sidecar, then
//!   rename; a crash mid-download leaves a recoverable partial, not
//!   a half-baked target file the next run will trust.
//! - **No silent fallback for SHA mismatch.** A hash failure deletes
//!   the partial and returns an error; we never trust a corrupted
//!   download.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Directory (relative to localmem home) where model files live.
/// Matches what `core/src/embed.rs::resolve_model_dir` looks for.
pub const MODELS_DIR: &str = "models";

/// One downloadable file inside a `KnownModel`. Models that span
/// multiple files (BGE-small = ONNX + tokenizer) carry a Vec of these.
#[derive(Debug, Clone)]
pub struct ModelFile {
    /// Filename written to `<home>/models/<slug>/<filename>`. Stable
    /// across versions — the runtime loader expects these exact names.
    pub filename: &'static str,
    /// HuggingFace (or other HTTPS) URL the file is fetched from.
    pub url: &'static str,
    /// Expected SHA-256 of the file. Empty string = "not yet armed";
    /// fetch logs a WARN and accepts the file without verification.
    /// Fill in once we've cleanly downloaded + hashed an authoritative
    /// copy and committed the hex. Tracked as a v0.2.1 follow-up.
    pub sha256_hex: &'static str,
    /// Declared file size in bytes (best-effort; used for the
    /// disk-space precheck). `0` means "unknown" → skip the precheck.
    pub size_bytes: u64,
}

/// One model the registry can fetch.
#[derive(Debug, Clone)]
pub struct KnownModel {
    /// Slug used for `--model <name>` and as the per-model
    /// subdirectory name under `<home>/models/`.
    pub slug: &'static str,
    /// Human description for `--dry-run` and `--json` output.
    pub description: &'static str,
    pub files: &'static [ModelFile],
}

impl KnownModel {
    pub fn total_size_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size_bytes).sum()
    }
}

/// Built-in registry. v0.2 ships three entries:
/// - `bge-small-en-v1.5`: the embedder we already use at runtime.
///   Today users have to populate `<home>/models/bge-small-en-v1.5/`
///   manually; T-62 makes it one command.
/// - `llama3.2:3b`, `qwen2.5:7b`: future-LLM placeholders for T-58
///   `local-llm` extractor + T-55 `local-llm` rewriter mode. Pre-
///   fetching today is harmless; consumers stub-bail until their
///   real impls land.
///
/// SHA256 fields are currently empty on every entry — `localmem
/// fetch-model` warns about unverified downloads until a v0.2.1
/// follow-up fills in authoritative hashes.
pub const KNOWN_MODELS: &[KnownModel] = &[
    KnownModel {
        slug: "bge-small-en-v1.5",
        description: "BGE-small-en-v1.5 ONNX embedder (~130 MB). Required for vector search.",
        files: &[
            ModelFile {
                filename: "model.onnx",
                url: "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main/onnx/model.onnx",
                sha256_hex: "",
                size_bytes: 0,
            },
            ModelFile {
                filename: "tokenizer.json",
                url: "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main/tokenizer.json",
                sha256_hex: "",
                size_bytes: 0,
            },
        ],
    },
    KnownModel {
        slug: "llama3.2:3b",
        description:
            "Llama 3.2 3B Instruct (GGUF Q4_K_M, ~2 GB). For the future local-LLM extractor + rewriter. Today's stubs do not consume this.",
        files: &[ModelFile {
            filename: "model.gguf",
            url: "https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/resolve/main/Llama-3.2-3B-Instruct-Q4_K_M.gguf",
            sha256_hex: "",
            size_bytes: 0,
        }],
    },
    KnownModel {
        slug: "qwen2.5:7b",
        description:
            "Qwen 2.5 7B Instruct (GGUF Q4_K_M, ~4.5 GB). Power-user choice for the future local-LLM extractor + rewriter.",
        files: &[ModelFile {
            filename: "model.gguf",
            url: "https://huggingface.co/bartowski/Qwen2.5-7B-Instruct-GGUF/resolve/main/Qwen2.5-7B-Instruct-Q4_K_M.gguf",
            sha256_hex: "",
            size_bytes: 0,
        }],
    },
];

/// Default model when the user runs `localmem fetch-model` with no
/// `--model` flag. The BGE embedder is the one production consumer
/// today; making it the default means a fresh install can be made
/// fully functional with one command.
pub const DEFAULT_MODEL_SLUG: &str = "bge-small-en-v1.5";

/// Find a known model by slug. None when no match — caller can
/// inspect this to dispatch to `--url` custom-fetch logic.
pub fn lookup(slug: &str) -> Option<&'static KnownModel> {
    KNOWN_MODELS.iter().find(|m| m.slug == slug)
}

/// Outcome reported in `--json` output and surfaced in tests.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileOutcome {
    /// File was downloaded and (if armed) SHA256-verified.
    Downloaded,
    /// File already existed and either verified (if armed) or was
    /// accepted without verification (empty SHA in registry).
    SkippedExisting,
    /// `--dry-run`; nothing was written.
    WouldDownload,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileReport {
    pub filename: String,
    pub url: String,
    pub destination: String,
    pub size_bytes: u64,
    pub sha256_verified: bool,
    pub outcome: FileOutcome,
}

#[derive(Debug, Clone, Serialize)]
struct JsonOutput {
    ok: bool,
    model: String,
    home: String,
    dry_run: bool,
    files: Vec<FileReport>,
}

/// Entry point for the `fetch-model` subcommand.
///
/// `model` is either a [`KNOWN_MODELS`] slug or the literal value
/// passed to `--url` (in which case `custom_url` is `Some` and `model`
/// becomes the filename slug). At least one of (known-model OR
/// custom-url) must be set; both being absent defaults to
/// [`DEFAULT_MODEL_SLUG`].
pub fn run(
    home: Option<&str>,
    model: Option<&str>,
    custom_url: Option<&str>,
    dry_run: bool,
    as_json: bool,
) -> Result<()> {
    let home_path = resolve_home(home)?;
    let mut out = io::stdout().lock();
    let report = match custom_url {
        Some(url) => fetch_custom(&home_path, model.unwrap_or("custom"), url, dry_run)?,
        None => {
            let slug = model.unwrap_or(DEFAULT_MODEL_SLUG);
            let known = lookup(slug).ok_or_else(|| {
                anyhow!(
                    "unknown model {slug:?}; pass --url for a custom HuggingFace URL, \
                     or pick one of: {}",
                    KNOWN_MODELS
                        .iter()
                        .map(|m| m.slug)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            fetch_known(&home_path, known, dry_run)?
        }
    };
    write_output(&mut out, &report, as_json)
}

fn fetch_known(home: &Path, model: &KnownModel, dry_run: bool) -> Result<JsonOutput> {
    if !dry_run {
        check_disk_space(home, model.total_size_bytes())?;
    }
    let dir = home.join(MODELS_DIR).join(model.slug);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create model dir at {}", dir.display()))?;

    let mut files: Vec<FileReport> = Vec::with_capacity(model.files.len());
    for f in model.files {
        let dest = dir.join(f.filename);
        let report = process_file(f, &dest, dry_run)?;
        files.push(report);
    }
    Ok(JsonOutput {
        ok: true,
        model: model.slug.to_string(),
        home: home.display().to_string(),
        dry_run,
        files,
    })
}

fn fetch_custom(
    home: &Path,
    slug: &str,
    url: &str,
    dry_run: bool,
) -> Result<JsonOutput> {
    // For `--url`, we don't know the file's size or SHA upfront, so
    // skip the disk-space precheck and verification — the user
    // accepts that responsibility when bypassing the registry.
    let filename = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("model.bin")
        .to_string();
    let dir = home.join(MODELS_DIR).join(slug);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create model dir at {}", dir.display()))?;
    let dest = dir.join(&filename);
    let f = ModelFile {
        filename: Box::leak(filename.clone().into_boxed_str()),
        url: Box::leak(url.to_string().into_boxed_str()),
        sha256_hex: "",
        size_bytes: 0,
    };
    let report = process_file(&f, &dest, dry_run)?;
    Ok(JsonOutput {
        ok: true,
        model: slug.to_string(),
        home: home.display().to_string(),
        dry_run,
        files: vec![report],
    })
}

fn process_file(file: &ModelFile, dest: &Path, dry_run: bool) -> Result<FileReport> {
    let already_exists = dest.is_file();
    let armed = !file.sha256_hex.is_empty();

    // Dry-run: report what would happen and exit.
    if dry_run {
        let outcome = if already_exists {
            FileOutcome::SkippedExisting
        } else {
            FileOutcome::WouldDownload
        };
        return Ok(FileReport {
            filename: file.filename.to_string(),
            url: file.url.to_string(),
            destination: dest.display().to_string(),
            size_bytes: file.size_bytes,
            sha256_verified: false,
            outcome,
        });
    }

    // Idempotency: if the file is already there AND verifies (or is
    // unarmed), skip the download. Mismatched hash on an existing
    // file is a loud failure — we don't silently overwrite a file
    // the user might have placed intentionally.
    if already_exists {
        if armed {
            let actual = hash_file(dest).context("hash existing file")?;
            if !hash_matches(&actual, file.sha256_hex) {
                bail!(
                    "{}: existing file's SHA-256 ({actual}) does not match registry \
                     ({expected}). Refusing to overwrite. Delete the file and re-run \
                     if you want to re-download, or update the registry if the file is \
                     intentionally pinned.",
                    dest.display(),
                    actual = actual,
                    expected = file.sha256_hex,
                );
            }
            return Ok(FileReport {
                filename: file.filename.to_string(),
                url: file.url.to_string(),
                destination: dest.display().to_string(),
                size_bytes: file.size_bytes,
                sha256_verified: true,
                outcome: FileOutcome::SkippedExisting,
            });
        }
        tracing::warn!(
            file = file.filename,
            "existing file accepted without SHA verification \
             (registry hash empty; track v0.2.1 follow-up to arm it)"
        );
        return Ok(FileReport {
            filename: file.filename.to_string(),
            url: file.url.to_string(),
            destination: dest.display().to_string(),
            size_bytes: file.size_bytes,
            sha256_verified: false,
            outcome: FileOutcome::SkippedExisting,
        });
    }

    // Streaming download to a `.partial` sidecar then atomic rename.
    let partial = dest.with_extension("partial");
    if partial.exists() {
        let _ = std::fs::remove_file(&partial);
    }
    download_stream(file.url, &partial)
        .with_context(|| format!("download {} → {}", file.url, partial.display()))?;

    if armed {
        let actual = hash_file(&partial).context("hash freshly-downloaded file")?;
        if !hash_matches(&actual, file.sha256_hex) {
            let _ = std::fs::remove_file(&partial);
            bail!(
                "downloaded {}: SHA-256 mismatch (got {actual}, expected {expected}). \
                 Partial file deleted; re-run to retry. If the upstream URL changed \
                 hash legitimately, update the registry first.",
                dest.display(),
                actual = actual,
                expected = file.sha256_hex,
            );
        }
        tracing::info!(file = file.filename, sha256 = %actual, "model file verified");
    } else {
        tracing::warn!(
            file = file.filename,
            "downloaded without SHA verification (registry hash empty; \
             v0.2.1 follow-up will arm this)"
        );
    }
    std::fs::rename(&partial, dest)
        .with_context(|| format!("atomic rename {} → {}", partial.display(), dest.display()))?;
    Ok(FileReport {
        filename: file.filename.to_string(),
        url: file.url.to_string(),
        destination: dest.display().to_string(),
        size_bytes: file.size_bytes,
        sha256_verified: armed,
        outcome: FileOutcome::Downloaded,
    })
}

/// Refuse to start a download if available disk is less than 2× the
/// declared model size. The 2× margin accounts for the `.partial`
/// sidecar living alongside the final destination during download
/// (so peak disk usage is `2 * size`).
///
/// A model with `size_bytes = 0` skips the check (legitimate
/// "unknown" state — `--url` and v0.2 registry entries with
/// unfilled sizes). Free-space query uses `statvfs` via libc on
/// unix-like systems; a probe failure logs WARN and proceeds rather
/// than blocking a working install on a diagnostic that itself
/// failed.
fn check_disk_space(home: &Path, declared_total: u64) -> Result<()> {
    if declared_total == 0 {
        return Ok(());
    }
    let target = home.join(MODELS_DIR);
    let probe_dir = if target.is_dir() { target } else { home.to_path_buf() };
    match free_space_bytes(&probe_dir) {
        Some(free) => {
            if free < declared_total.saturating_mul(2) {
                bail!(
                    "free disk at {} is {} bytes; need at least {} for this model \
                     (declared size {} + partial sidecar). Free up space and re-run.",
                    probe_dir.display(),
                    free,
                    declared_total.saturating_mul(2),
                    declared_total,
                );
            }
        }
        None => {
            tracing::warn!(
                dir = %probe_dir.display(),
                "could not query free disk space; skipping precheck"
            );
        }
    }
    Ok(())
}

#[cfg(target_family = "unix")]
fn free_space_bytes(dir: &Path) -> Option<u64> {
    use std::ffi::CString;
    let path = CString::new(dir.as_os_str().to_str()?).ok()?;
    // SAFETY: `statvfs` reads from a writable struct we own and
    // accepts the C string we built. We check the return code below
    // before reading the struct.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(path.as_ptr(), &mut buf) };
    if ret != 0 {
        return None;
    }
    Some((buf.f_bavail as u64).saturating_mul(buf.f_frsize as u64))
}

#[cfg(not(target_family = "unix"))]
fn free_space_bytes(_dir: &Path) -> Option<u64> {
    None
}

fn download_stream(url: &str, dest: &Path) -> Result<()> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(120))
        .call()
        .map_err(|e| anyhow!("HTTP request failed: {e}"))?;
    if resp.status() != 200 {
        bail!("HTTP {} on {}", resp.status(), url);
    }
    let mut writer = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dest)
        .with_context(|| format!("open partial file {}", dest.display()))?;
    io::copy(&mut resp.into_reader(), &mut writer)
        .with_context(|| format!("stream body to {}", dest.display()))?;
    writer.flush().ok();
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_matches(actual_hex: &str, expected_hex: &str) -> bool {
    actual_hex.eq_ignore_ascii_case(expected_hex)
}

fn write_output<W: Write>(out: &mut W, report: &JsonOutput, as_json: bool) -> Result<()> {
    if as_json {
        serde_json::to_writer(&mut *out, report).context("serialize fetch-model JSON")?;
        out.write_all(b"\n").context("write JSON newline")?;
        return Ok(());
    }
    writeln!(
        out,
        "model {} in home {}{}",
        report.model,
        report.home,
        if report.dry_run { " (dry-run)" } else { "" },
    )
    .ok();
    for f in &report.files {
        let outcome = match f.outcome {
            FileOutcome::Downloaded => "downloaded",
            FileOutcome::SkippedExisting => "skipped (existing)",
            FileOutcome::WouldDownload => "would download",
        };
        let verified = if f.sha256_verified {
            " [SHA-256 verified]"
        } else {
            " [unverified]"
        };
        writeln!(out, "  {} :: {}{}", f.filename, outcome, verified).ok();
        writeln!(out, "    url:  {}", f.url).ok();
        writeln!(out, "    dest: {}", f.destination).ok();
    }
    Ok(())
}

fn resolve_home(override_: Option<&str>) -> Result<PathBuf> {
    if let Some(h) = override_.filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var("HOME")
        .context("HOME environment variable is not set; pass --home explicitly")?;
    Ok(PathBuf::from(home).join(".localmem"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::Response;
    use axum::routing::get;
    use axum::Router;
    use std::net::SocketAddr;
    use tempfile::tempdir;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// Spin up a tiny HTTP server serving in-memory bodies under given
    /// paths. Returns the bound address + a shutdown signal so the
    /// test can `await` cleanup. Each path serves a fixed byte
    /// payload — keeps tests offline and deterministic.
    async fn spawn_server(
        files: Vec<(&'static str, Vec<u8>)>,
    ) -> (SocketAddr, oneshot::Sender<()>) {
        let mut router = Router::new();
        for (path, bytes) in files {
            let body = bytes.clone();
            router = router.route(
                path,
                get(move || {
                    let body = body.clone();
                    async move {
                        Response::builder()
                            .status(StatusCode::OK)
                            .body(Body::from(body))
                            .unwrap()
                    }
                }),
            );
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        // Yield once so the listener is actually accepting before the
        // test calls into ureq.
        tokio::task::yield_now().await;
        (addr, tx)
    }

    fn sha256(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    async fn run_with_model(home: &Path, model: KnownModel, dry_run: bool) -> Result<JsonOutput> {
        // Bypass the registry lookup so we can use a synthetic
        // KnownModel pointing at a local test server. Mirrors the
        // production code path otherwise.
        //
        // `fetch_known` calls into sync `ureq`, which would block
        // the test's tokio runtime and starve the in-process axum
        // server. Production `localmem fetch-model` runs on the
        // main thread (CLI is sync over a #[tokio::main] dispatch
        // boundary) so the issue is test-only. `spawn_blocking`
        // gives ureq its own thread without changing the production
        // call shape.
        let home_buf = home.to_path_buf();
        tokio::task::spawn_blocking(move || fetch_known(&home_buf, &model, dry_run))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn dry_run_reports_would_download_without_touching_disk() {
        let tmp = tempdir().unwrap();
        let (addr, shutdown) =
            spawn_server(vec![("/m.bin", b"abc".to_vec())]).await;
        let url = Box::leak(format!("http://{addr}/m.bin").into_boxed_str());
        let model = KnownModel {
            slug: "test",
            description: "test",
            files: Box::leak(Box::new([ModelFile {
                filename: "m.bin",
                url,
                sha256_hex: "",
                size_bytes: 3,
            }])),
        };
        let report = run_with_model(tmp.path(), model, true).await.unwrap();
        assert!(report.dry_run);
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].outcome, FileOutcome::WouldDownload);
        // Nothing landed on disk.
        assert!(!tmp.path().join(MODELS_DIR).join("test").join("m.bin").exists());
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn downloads_and_verifies_sha256() {
        let tmp = tempdir().unwrap();
        let body = b"hello model payload".to_vec();
        let expected_sha = sha256(&body);
        let (addr, shutdown) = spawn_server(vec![("/m.bin", body)]).await;
        let url = Box::leak(format!("http://{addr}/m.bin").into_boxed_str());
        let sha = Box::leak(expected_sha.clone().into_boxed_str());
        let model = KnownModel {
            slug: "verified",
            description: "test",
            files: Box::leak(Box::new([ModelFile {
                filename: "m.bin",
                url,
                sha256_hex: sha,
                size_bytes: 0, // skip disk-space precheck
            }])),
        };
        let report = run_with_model(tmp.path(), model, false).await.unwrap();
        assert_eq!(report.files[0].outcome, FileOutcome::Downloaded);
        assert!(report.files[0].sha256_verified);
        let dest = tmp.path().join(MODELS_DIR).join("verified").join("m.bin");
        assert!(dest.exists());
        // The atomic-rename invariant: no leftover .partial.
        assert!(!dest.with_extension("partial").exists());
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn sha_mismatch_aborts_and_deletes_partial() {
        let tmp = tempdir().unwrap();
        let body = b"actual content".to_vec();
        let wrong_sha = sha256(b"different content");
        let (addr, shutdown) = spawn_server(vec![("/m.bin", body)]).await;
        let url = Box::leak(format!("http://{addr}/m.bin").into_boxed_str());
        let sha = Box::leak(wrong_sha.into_boxed_str());
        let model = KnownModel {
            slug: "mismatch",
            description: "test",
            files: Box::leak(Box::new([ModelFile {
                filename: "m.bin",
                url,
                sha256_hex: sha,
                size_bytes: 0,
            }])),
        };
        let err = run_with_model(tmp.path(), model, false).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("SHA-256 mismatch"), "got: {msg}");
        let dir = tmp.path().join(MODELS_DIR).join("mismatch");
        // Neither the final file nor the partial should remain.
        assert!(!dir.join("m.bin").exists(), "final file must not land on mismatch");
        assert!(
            !dir.join("m.partial").exists(),
            "partial must be cleaned up on mismatch"
        );
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn existing_file_with_matching_sha_is_skipped() {
        let tmp = tempdir().unwrap();
        let body = b"existing payload".to_vec();
        let expected_sha = sha256(&body);
        // Pre-populate the destination.
        let dir = tmp.path().join(MODELS_DIR).join("preexisting");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m.bin"), &body).unwrap();

        // Server is registered but should never be hit on the
        // happy idempotency path — assert via the outcome below.
        let (addr, shutdown) = spawn_server(vec![("/m.bin", body)]).await;
        let url = Box::leak(format!("http://{addr}/m.bin").into_boxed_str());
        let sha = Box::leak(expected_sha.into_boxed_str());
        let model = KnownModel {
            slug: "preexisting",
            description: "test",
            files: Box::leak(Box::new([ModelFile {
                filename: "m.bin",
                url,
                sha256_hex: sha,
                size_bytes: 0,
            }])),
        };
        let report = run_with_model(tmp.path(), model, false).await.unwrap();
        assert_eq!(report.files[0].outcome, FileOutcome::SkippedExisting);
        assert!(report.files[0].sha256_verified);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn existing_file_with_wrong_sha_errors_loudly() {
        // Don't silently re-download a file the user might have
        // placed intentionally. Don't silently accept a corrupted
        // existing file. Bail.
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(MODELS_DIR).join("conflict");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m.bin"), b"wrong content").unwrap();

        let (addr, shutdown) =
            spawn_server(vec![("/m.bin", b"different".to_vec())]).await;
        let url = Box::leak(format!("http://{addr}/m.bin").into_boxed_str());
        let sha = Box::leak(sha256(b"different").into_boxed_str());
        let model = KnownModel {
            slug: "conflict",
            description: "test",
            files: Box::leak(Box::new([ModelFile {
                filename: "m.bin",
                url,
                sha256_hex: sha,
                size_bytes: 0,
            }])),
        };
        let err = run_with_model(tmp.path(), model, false).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not match"), "got: {msg}");
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn empty_registry_sha_accepts_without_verification() {
        // The registry ships with empty SHAs initially; fetch must
        // still work, just with a WARN log that verification didn't
        // run. The test asserts the path-level outcome, not the log.
        let tmp = tempdir().unwrap();
        let (addr, shutdown) =
            spawn_server(vec![("/m.bin", b"unverified".to_vec())]).await;
        let url = Box::leak(format!("http://{addr}/m.bin").into_boxed_str());
        let model = KnownModel {
            slug: "unarmed",
            description: "test",
            files: Box::leak(Box::new([ModelFile {
                filename: "m.bin",
                url,
                sha256_hex: "",
                size_bytes: 0,
            }])),
        };
        let report = run_with_model(tmp.path(), model, false).await.unwrap();
        assert_eq!(report.files[0].outcome, FileOutcome::Downloaded);
        assert!(!report.files[0].sha256_verified);
        let _ = shutdown.send(());
    }

    #[test]
    fn lookup_returns_none_for_unknown_slug() {
        assert!(lookup("nope:1.0").is_none());
    }

    #[test]
    fn lookup_returns_default_model() {
        let m = lookup(DEFAULT_MODEL_SLUG).expect("default model must be registered");
        assert_eq!(m.slug, DEFAULT_MODEL_SLUG);
        assert!(!m.files.is_empty());
    }

    #[test]
    fn registry_includes_all_three_v0_2_entries() {
        // Sanity check on the public registry — the user-facing
        // surface depends on these slugs being stable.
        let slugs: Vec<&str> = KNOWN_MODELS.iter().map(|m| m.slug).collect();
        assert!(slugs.contains(&"bge-small-en-v1.5"));
        assert!(slugs.contains(&"llama3.2:3b"));
        assert!(slugs.contains(&"qwen2.5:7b"));
    }

    #[test]
    fn hash_matches_is_case_insensitive() {
        assert!(hash_matches("ABC123", "abc123"));
        assert!(!hash_matches("abc124", "abc123"));
    }
}
