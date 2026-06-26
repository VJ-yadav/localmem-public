//! `localmem doctor` — diagnostic summary for first-run troubles (T-48).
//!
//! Walks a fixed list of [`Check`]s and reports each one as PASS,
//! WARN, or FAIL. FAIL/WARN rows carry a one-line `fix` command the
//! user can copy-paste (or run automatically via `--fix`, gated on
//! per-check safety).
//!
//! The check set is deliberately short and covers the failures users
//! actually hit on a fresh install:
//! 1. The binary is on PATH (so `localmem doctor` worked at all).
//! 2. The home dir is initialised (`~/.localmem/events.jsonl` exists).
//! 3. The BGE embedder model is present (or absent, surfaced as a
//!    WARN since lex+facts still work without it).
//! 4. The HTTP server is reachable on the configured address.
//! 5. macOS Gatekeeper quarantine attr is stripped from the binary.
//! 6. Each MCP client either has localmem wired or doesn't —
//!    informational, not a hard failure.

use crate::cli::mcp_clients::{adapter, ClientId};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Outcome category for a single check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            // ASCII-only so the output renders cleanly on every
            // terminal, including CI logs without UTF-8 fonts.
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

/// Result of a single check. `detail` is the human-facing one-liner
/// shown next to the status glyph. `fix` is the command a user can
/// run to make the check pass; `None` means no fix is offered (e.g.
/// the check already passed).
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// Run every doctor check against the given localmem home + core
/// address. The function is pure data-in/data-out (no printing) so
/// the CLI handler can choose JSON vs human output.
pub fn run_checks(home: &Path, core_addr: &str) -> Vec<CheckResult> {
    let mut out = vec![
        check_binary_on_path(),
        check_home_initialised(home),
        check_model_present(home),
        check_reranker(home),
        check_stores_fresh(home),
        check_server_reachable(core_addr),
    ];
    if cfg!(target_os = "macos") {
        out.push(check_gatekeeper_quarantine());
    }
    // MCP client wiring: one row per known client. We surface even
    // unsupported clients (Codex, Aider) so the table is the
    // canonical "what does localmem think it knows about every
    // tool" answer.
    let host_home = std::env::var("HOME").ok().map(PathBuf::from);
    for id in ClientId::all() {
        out.push(check_mcp_client(*id, host_home.as_deref()));
    }
    out
}

/// Whether any check returned `Status::Fail`. Used by the CLI
/// handler to set a non-zero exit code so CI scripts can gate on
/// `localmem doctor`.
pub fn has_failures(results: &[CheckResult]) -> bool {
    results.iter().any(|r| r.status == Status::Fail)
}

// ---- individual checks -----------------------------------------------------

fn check_binary_on_path() -> CheckResult {
    // We're already running, so the binary is on *some* path. The
    // useful question is whether `which localmem` finds it — that's
    // what the install script sets up. We piggyback on the MCP
    // module's `which` helper to keep one implementation.
    use crate::cli::mcp_clients::which;
    match which("localmem") {
        Ok(p) => CheckResult {
            name: "binary on PATH",
            status: Status::Pass,
            detail: p.display().to_string(),
            fix: None,
        },
        Err(_) => {
            // Reaching this means we ran via an absolute path; future
            // `localmem` invocations from a different shell won't
            // resolve. Install script T-31 puts the binary at
            // ~/.local/bin, which is on PATH on most modern setups.
            let current = std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<unknown>".into());
            CheckResult {
                name: "binary on PATH",
                status: Status::Warn,
                detail: format!(
                    "running from {current}, not resolvable via $PATH; \
                     add the directory to your shell PATH"
                ),
                fix: Some("export PATH=\"$HOME/.local/bin:$PATH\"  # add to shell rc".into()),
            }
        }
    }
}

fn check_home_initialised(home: &Path) -> CheckResult {
    let events = home.join(crate::event_log::EVENTS_FILE);
    if events.exists() {
        CheckResult {
            name: "home initialised",
            status: Status::Pass,
            detail: home.display().to_string(),
            fix: None,
        }
    } else {
        CheckResult {
            name: "home initialised",
            status: Status::Fail,
            detail: format!("{} missing", events.display()),
            fix: Some(format!(
                "localmem init --home {}  # creates events.jsonl + derived/",
                home.display()
            )),
        }
    }
}

/// Staleness gate: the lexical index should hold roughly one doc per signal
/// capture (ephemeral tool-traces are excluded by design). Far fewer means the
/// derived stores drifted from the event log — the failure where a large import
/// never got a full `replay` and search silently missed most memories.
fn check_stores_fresh(home: &Path) -> CheckResult {
    let signal_captures = crate::event_log::EventLog::open(home).ok().and_then(|log| {
        log.iter().ok().map(|it| {
            it.filter_map(|e| e.ok())
                .filter(
                    |e| matches!(&e.kind, crate::event::EventKind::Capture(p) if !p.is_ephemeral()),
                )
                .count() as u64
        })
    });
    let lexical_docs = crate::lexical::LexicalIndex::open_reader_only(home)
        .ok()
        .map(|idx| idx.doc_count());
    match (signal_captures, lexical_docs) {
        (Some(caps), Some(docs)) if caps > 0 => {
            // 10% slack for in-flight writes / async indexing.
            if docs.saturating_mul(10) < caps.saturating_mul(9) {
                CheckResult {
                    name: "derived stores fresh",
                    status: Status::Warn,
                    detail: format!(
                        "lexical index holds {docs} docs but the log has {caps} signal captures; \
                         stores drifted — search will miss memories"
                    ),
                    fix: Some(
                        "localmem replay  # full rebuild of derived stores from the event log"
                            .into(),
                    ),
                }
            } else {
                CheckResult {
                    name: "derived stores fresh",
                    status: Status::Pass,
                    detail: format!("{docs} lexical docs vs {caps} signal captures"),
                    fix: None,
                }
            }
        }
        _ => CheckResult {
            name: "derived stores fresh",
            status: Status::Pass,
            detail: "no derived stores to check yet".into(),
            fix: None,
        },
    }
}

fn check_model_present(home: &Path) -> CheckResult {
    // The model dir defaults to <home>/models/bge-small-en-v1.5/ but
    // is overridable via LOCALMEM_MODEL_DIR. Mirror the same
    // resolution the CLI search uses so the doctor matches the real
    // load path.
    let model_dir = crate::embed::resolve_model_dir(home);
    let model_file = model_dir.join("model.onnx");
    let tok_file = model_dir.join("tokenizer.json");
    let have_model = model_file.is_file();
    let have_tokenizer = tok_file.is_file();
    if have_model && have_tokenizer {
        CheckResult {
            name: "embedder model",
            status: Status::Pass,
            detail: model_dir.display().to_string(),
            fix: None,
        }
    } else {
        // WARN, not FAIL: lex + facts still work without the model,
        // so this is a quality issue not a broken-install issue.
        let missing: Vec<&str> = [
            (!have_model).then_some("model.onnx"),
            (!have_tokenizer).then_some("tokenizer.json"),
        ]
        .into_iter()
        .flatten()
        .collect();
        CheckResult {
            name: "embedder model",
            status: Status::Warn,
            detail: format!(
                "missing {} under {}; hybrid search will fall back to lex-only",
                missing.join(", "),
                model_dir.display(),
            ),
            fix: Some(format!(
                "LOCALMEM_MODEL_DIR=/path/to/bge-small-en-v1.5  # or place files under {}",
                model_dir.display()
            )),
        }
    }
}

fn check_reranker(home: &Path) -> CheckResult {
    // Config coherence (the check whose absence let the 56% run silently skip
    // reranking): if rerank is ENABLED (config.toml or LOCALMEM_RETRIEVER_RERANK,
    // both folded in by Config::load), the cross-encoder MUST load AND run — else
    // /search degrades to first-stage with only a log line, and a whole benchmark
    // can pass through unreranked. Disabled rerank is a clean PASS.
    let cfg = crate::config::Config::load(home).unwrap_or_default();
    if !cfg.retriever.rerank {
        return CheckResult {
            name: "reranker",
            status: Status::Pass,
            detail: "rerank disabled (first-stage retrieval only)".into(),
            fix: None,
        };
    }
    let dir = std::env::var("LOCALMEM_RERANKER_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("models").join("reranker"));
    match crate::rerank::Reranker::load(&dir) {
        Ok(mut r) => match r.rerank("preflight query", &["a relevant preflight document"]) {
            Ok(scores) if scores.len() == 1 => CheckResult {
                name: "reranker",
                status: Status::Pass,
                detail: format!("rerank ON; cross-encoder loads + scores from {}", dir.display()),
                fix: None,
            },
            _ => CheckResult {
                name: "reranker",
                status: Status::Fail,
                detail: format!(
                    "rerank=true but the model at {} loaded yet failed to SCORE (incompatible ONNX?); search would silently degrade to first-stage",
                    dir.display()
                ),
                fix: Some(
                    "use a sequence-classification cross-encoder ONNX (e.g. ms-marco-MiniLM), or set [retriever].rerank=false".into(),
                ),
            },
        },
        Err(e) => CheckResult {
            name: "reranker",
            status: Status::Fail,
            detail: format!(
                "rerank=true but no loadable reranker model at {} ({}); search would silently degrade to first-stage",
                dir.display(),
                format!("{e:#}").lines().next().unwrap_or("load failed")
            ),
            fix: Some(format!(
                "fetch a reranker model.onnx + tokenizer.json into {}  (or LOCALMEM_RERANKER_DIR), or disable rerank",
                dir.display()
            )),
        },
    }
}

fn check_server_reachable(core_addr: &str) -> CheckResult {
    // Short 200ms probe so doctor stays snappy on a cold start. The
    // failure modes are "server not running" (typical) and "wrong
    // address" (rare); both render the same WARN with a fix.
    let url = format!("http://{core_addr}/health");
    let client = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_millis(200))
        .build();
    match client.get(&url).call() {
        Ok(r) if r.status() == 200 => CheckResult {
            name: "core server",
            status: Status::Pass,
            detail: url,
            fix: None,
        },
        Ok(r) => CheckResult {
            name: "core server",
            status: Status::Warn,
            detail: format!("{} returned HTTP {}", url, r.status()),
            fix: Some(format!("localmem serve --addr {core_addr}")),
        },
        Err(_) => CheckResult {
            name: "core server",
            status: Status::Warn,
            detail: format!("{} unreachable", url),
            fix: Some(format!(
                "localmem serve --addr {core_addr}  # start the core in another terminal"
            )),
        },
    }
}

fn check_gatekeeper_quarantine() -> CheckResult {
    // macOS only: a binary downloaded via `curl | bash` carries the
    // `com.apple.quarantine` xattr until `xattr -d` is run (or our
    // T-47 install script strips it). When the attr is present,
    // Gatekeeper stalls the first launch for a multi-second scan
    // that *looks* like a hang to the user.
    let Ok(exe) = std::env::current_exe() else {
        return CheckResult {
            name: "macOS Gatekeeper",
            status: Status::Warn,
            detail: "could not resolve current binary path".into(),
            fix: None,
        };
    };
    let out = std::process::Command::new("xattr")
        .arg("-p")
        .arg("com.apple.quarantine")
        .arg(&exe)
        .output();
    match out {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => CheckResult {
            name: "macOS Gatekeeper",
            status: Status::Warn,
            detail: format!("com.apple.quarantine set on {}", exe.display()),
            fix: Some(format!("xattr -d com.apple.quarantine {}", exe.display())),
        },
        _ => CheckResult {
            name: "macOS Gatekeeper",
            status: Status::Pass,
            detail: "no quarantine attribute".into(),
            fix: None,
        },
    }
}

fn check_mcp_client(id: ClientId, host_home: Option<&Path>) -> CheckResult {
    let name = match id {
        ClientId::ClaudeDesktop => "mcp: Claude Desktop",
        ClientId::ClaudeCode => "mcp: Claude Code",
        ClientId::Cursor => "mcp: Cursor",
        ClientId::Windsurf => "mcp: Windsurf",
        ClientId::Cline => "mcp: Cline",
        ClientId::Codex => "mcp: Codex",
        ClientId::Aider => "mcp: Aider",
    };
    let adapter = adapter(id);
    let Some(host) = host_home else {
        return CheckResult {
            name,
            status: Status::Warn,
            detail: "$HOME not set; cannot check MCP wiring".into(),
            fix: None,
        };
    };
    if let Some(msg) = adapter.unsupported_msg() {
        return CheckResult {
            name,
            status: Status::Warn,
            detail: msg.into(),
            fix: None,
        };
    }
    match adapter.is_installed(host) {
        Ok(true) => CheckResult {
            name,
            status: Status::Pass,
            detail: adapter.config_path(host).display().to_string(),
            fix: None,
        },
        Ok(false) => CheckResult {
            name,
            status: Status::Warn,
            detail: format!(
                "not wired ({} not present or no `localmem` entry)",
                adapter.config_path(host).display()
            ),
            fix: Some(format!("localmem mcp install {}", id.slug())),
        },
        Err(e) => CheckResult {
            name,
            status: Status::Warn,
            detail: format!("could not read config: {e:#}"),
            fix: None,
        },
    }
}

// ---- CLI entry point + rendering ------------------------------------------

/// `localmem doctor` handler. `home_override` mirrors the other CLI
/// commands; `core_addr` is resolved by the caller (main.rs) the
/// same way `serve` does. `apply_fixes` is the `--fix` flag — when
/// true, every check with a `safe_to_run` fix is executed after the
/// table prints.
pub fn run(
    home_override: Option<&str>,
    core_addr: &str,
    apply_fixes: bool,
    as_json: bool,
) -> Result<()> {
    let home = resolve_home(home_override)?;
    let results = run_checks(&home, core_addr);
    if as_json {
        let body = serde_json::json!({
            "ok": !has_failures(&results),
            "checks": results,
        });
        println!("{body}");
    } else {
        render_human(&results);
    }
    if apply_fixes {
        // For v0.2 first cut, the only auto-fix we run unattended is
        // `localmem init` when the home dir is missing. Everything
        // else gets its fix command printed for the user to run
        // themselves. Auto-running model fetch / xattr strip / mcp
        // install across every client is too invasive without per-
        // fix confirmation, which is a follow-up.
        if let Some(home_check) = results
            .iter()
            .find(|r| r.name == "home initialised" && r.status == Status::Fail)
        {
            println!("\n--fix: home not initialised; running `localmem init`...");
            crate::cli::init::run(home_override, /* as_json = */ false)
                .context("auto-fix: localmem init")?;
            // We deliberately don't re-run all checks; the user can
            // re-invoke `localmem doctor` to confirm the next round.
            let _ = home_check; // silence unused-var if logic changes
        } else {
            println!(
                "\n--fix: no safe auto-fixes available right now. \
                 Run the fix commands above manually."
            );
        }
    }
    if has_failures(&results) {
        anyhow::bail!("doctor: one or more checks FAILED — see output above");
    }
    Ok(())
}

fn render_human(results: &[CheckResult]) {
    println!("level  check                  detail");
    for r in results {
        println!("{:<6} {:<22} {}", r.status.glyph(), r.name, r.detail);
        if let Some(fix) = &r.fix {
            println!("       fix: {fix}");
        }
    }
    let fails = results.iter().filter(|r| r.status == Status::Fail).count();
    let warns = results.iter().filter(|r| r.status == Status::Warn).count();
    println!(
        "\nsummary: {} PASS / {warns} WARN / {fails} FAIL",
        results.len() - warns - fails
    );
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
    use std::fs;
    use tempfile::tempdir;

    fn write_events_file(home: &Path) {
        fs::create_dir_all(home).unwrap();
        fs::write(home.join(crate::event_log::EVENTS_FILE), "").unwrap();
    }

    #[test]
    fn home_initialised_passes_when_events_jsonl_exists() {
        let tmp = tempdir().unwrap();
        write_events_file(tmp.path());
        let r = check_home_initialised(tmp.path());
        assert_eq!(r.status, Status::Pass);
    }

    #[test]
    fn home_initialised_fails_with_init_command_when_missing() {
        let tmp = tempdir().unwrap();
        let r = check_home_initialised(tmp.path());
        assert_eq!(r.status, Status::Fail);
        let fix = r.fix.expect("fix command must be set on FAIL");
        assert!(
            fix.starts_with("localmem init"),
            "fix should suggest `localmem init`, got: {fix}",
        );
    }

    #[test]
    fn model_check_warns_when_model_files_missing() {
        let tmp = tempdir().unwrap();
        write_events_file(tmp.path());
        // Force the model dir to a path that doesn't exist so we
        // bypass any developer-side LOCALMEM_MODEL_DIR.
        std::env::set_var("LOCALMEM_MODEL_DIR", tmp.path().join("definitely-not-here"));
        let r = check_model_present(tmp.path());
        std::env::remove_var("LOCALMEM_MODEL_DIR");
        // WARN (not FAIL): lex + facts still work without the model.
        assert_eq!(r.status, Status::Warn);
        assert!(r.fix.is_some());
    }

    #[test]
    fn model_check_passes_when_both_files_present() {
        let tmp = tempdir().unwrap();
        let model_dir = tmp.path().join("models").join("bge-small-en-v1.5");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join("model.onnx"), b"fake").unwrap();
        fs::write(model_dir.join("tokenizer.json"), b"{}").unwrap();
        // Use the default path (no env override) so this test pins
        // the standard resolution.
        std::env::remove_var("LOCALMEM_MODEL_DIR");
        let r = check_model_present(tmp.path());
        assert_eq!(r.status, Status::Pass);
    }

    #[test]
    fn reranker_check_passes_when_rerank_disabled() {
        std::env::remove_var("LOCALMEM_RETRIEVER_RERANK");
        let tmp = tempdir().unwrap();
        write_events_file(tmp.path());
        // Rerank is default-on now, so disable it explicitly to exercise the
        // disabled path. check_reranker reads the rerank flag from config.
        std::fs::write(
            tmp.path().join("config.toml"),
            "[retriever]\nrerank = false\n",
        )
        .unwrap();
        let r = check_reranker(tmp.path());
        assert_eq!(r.status, Status::Pass);
        assert!(r.detail.contains("disabled"));
    }

    #[test]
    fn reranker_check_fails_when_enabled_but_model_unloadable() {
        let tmp = tempdir().unwrap();
        write_events_file(tmp.path());
        fs::write(
            tmp.path().join("config.toml"),
            "[retriever]\nrerank = true\n",
        )
        .unwrap();
        std::env::set_var("LOCALMEM_RERANKER_DIR", tmp.path().join("no-model-here"));
        let r = check_reranker(tmp.path());
        std::env::remove_var("LOCALMEM_RERANKER_DIR");
        // rerank=true but model absent => FAIL (this is the check that would have
        // caught the 56% silent-degrade), with an actionable fix.
        assert_eq!(r.status, Status::Fail);
        assert!(r.fix.is_some());
    }

    #[test]
    fn server_check_warns_when_addr_unreachable() {
        // Port 1 is privileged and reliably refuses to listen from
        // a non-root test process, so the probe times out within
        // the 200ms budget.
        let r = check_server_reachable("127.0.0.1:1");
        assert_eq!(r.status, Status::Warn);
        assert!(r.fix.is_some());
    }

    #[test]
    fn run_checks_returns_one_row_per_mcp_client_plus_core() {
        let tmp = tempdir().unwrap();
        write_events_file(tmp.path());
        let results = run_checks(tmp.path(), "127.0.0.1:1");
        // Required core checks: binary, home, model, server, plus
        // optional Gatekeeper on macOS, plus 7 MCP clients.
        let mcp_rows = results
            .iter()
            .filter(|r| r.name.starts_with("mcp: "))
            .count();
        assert_eq!(mcp_rows, ClientId::all().len());
    }

    #[test]
    fn has_failures_reports_true_only_when_a_fail_exists() {
        assert!(!has_failures(&[CheckResult {
            name: "x",
            status: Status::Pass,
            detail: "".into(),
            fix: None,
        }]));
        assert!(!has_failures(&[CheckResult {
            name: "x",
            status: Status::Warn,
            detail: "".into(),
            fix: None,
        }]));
        assert!(has_failures(&[CheckResult {
            name: "x",
            status: Status::Fail,
            detail: "".into(),
            fix: None,
        }]));
    }
}
