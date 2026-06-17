//! `localmem import-wizard` handler + first-run scan (capability #5).
//!
//! Per SPEC_V0_2 "First-run import wizard": on a fresh `localmem
//! init`, detect existing memory sources in common locations
//! (Downloads, Desktop, CWD) and offer to import them. Onboarding
//! becomes rich from minute one instead of opening on an empty home.
//!
//! v0.2 first cut covers the two sources that already have working
//! importers: ChatGPT exports (`conversations.json` from the OpenAI
//! "Export data" feature) and Claude exports. Obsidian, Notion,
//! Memento, Mem0, Supermemory are deferred to v0.2.1 — they need
//! their own importer modules first.
//!
//! Detection is conservative: we identify a candidate by its file
//! name + sibling files, then `Confidence::High` only when both the
//! filename and a sibling marker line up. The user always gets the
//! detected path so they can sanity-check before running an apply.

use crate::import::{chatgpt::import_chatgpt, claude::import_claude, ImportStats};
use anyhow::{Context, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// One detected source the wizard suggests importing.
#[derive(Debug, Clone, Serialize)]
pub struct Detection {
    /// Importer format slug. Matches the strings the existing
    /// `localmem import <format> <path>` CLI already accepts.
    pub format: String,
    /// Absolute path to the file the importer should read (the
    /// extracted `conversations.json`, not the original ZIP).
    pub path: PathBuf,
    /// Confidence the file actually matches the format. Useful for
    /// the human-readable wizard output; `Apply` only runs when the
    /// detector returns `High` so a mislabeled file does not silently
    /// ingest.
    pub confidence: Confidence,
    /// One-line hint shown to the user explaining what tipped the
    /// detector (e.g. "sibling user.json file indicates ChatGPT").
    pub hint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Filename + sibling marker both match.
    High,
    /// Filename pattern matches but no sibling marker. Surfaced so
    /// the user can manually confirm.
    Low,
}

/// Outcome of a per-detection import attempt.
#[derive(Debug, Clone, Serialize)]
struct ApplyResult {
    detection: Detection,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<ImportStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    ok: bool,
    detections: &'a [Detection],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    applied: Vec<ApplyResult>,
}

/// Entry point for the `import-wizard` subcommand.
///
/// `apply = false`: scan + print detections, do not modify the
/// event log. `apply = true`: scan, then run the appropriate
/// importer for every `Confidence::High` detection. `Confidence::Low`
/// detections always require an explicit `localmem import <format>
/// <path>` invocation; the wizard never auto-imports ambiguous files.
pub fn run(home: Option<&str>, apply: bool, as_json: bool) -> Result<()> {
    let home_path = resolve_home(home)?;
    let detections = scan_default_locations()?;
    let applied = if apply {
        run_apply(&home_path, &detections)
    } else {
        Vec::new()
    };
    let mut out = io::stdout().lock();
    write_output(&mut out, &detections, &applied, as_json)
}

/// Scan the default candidate roots (`~/Downloads`, `~/Desktop`, and
/// the current working directory) for known import-source files.
/// Used by both the `import-wizard` CLI and the `init` first-run hook.
pub fn scan_default_locations() -> Result<Vec<Detection>> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(&home).join("Downloads"));
        roots.push(PathBuf::from(&home).join("Desktop"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    scan_paths(&roots)
}

/// Scan an explicit list of roots. Pulled out for tests so we can
/// point at tempdirs without mutating `$HOME`.
pub fn scan_paths(roots: &[PathBuf]) -> Result<Vec<Detection>> {
    let mut detections: Vec<Detection> = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        // One level of recursion: the typical export shape is
        // `~/Downloads/chatgpt-export-YYYY-MM-DD/conversations.json`,
        // so we look for `conversations.json` both directly under
        // each root and one level deeper. Deeper trees (Notion,
        // Obsidian) need format-specific scanners we defer.
        scan_one(root, &mut detections)?;
        for entry in std::fs::read_dir(root)
            .with_context(|| format!("read dir {}", root.display()))?
            .flatten()
        {
            let p = entry.path();
            if p.is_dir() {
                scan_one(&p, &mut detections)?;
            }
        }
    }
    // De-duplicate by absolute path so the same file under multiple
    // roots (rare but possible via symlinks) only surfaces once.
    detections.sort_by(|a, b| a.path.cmp(&b.path));
    detections.dedup_by(|a, b| a.path == b.path);
    Ok(detections)
}

fn scan_one(dir: &Path, out: &mut Vec<Detection>) -> Result<()> {
    let conversations = dir.join("conversations.json");
    if !conversations.is_file() {
        return Ok(());
    }
    if let Some(det) = classify_conversations_json(&conversations) {
        out.push(det);
    }
    Ok(())
}

/// Classify a `conversations.json` file as ChatGPT, Claude, or
/// unknown. Heuristic:
/// 1. Look at the parent directory name: `*chatgpt*` -> chatgpt,
///    `*claude*` -> claude.
/// 2. Look for sibling marker files: `user.json` for ChatGPT,
///    `users.json` (plural, Claude's choice) for Claude.
/// 3. If both signal the same format, return `High`; if only one
///    signals, return `Low` so the user can confirm; otherwise the
///    file is skipped.
fn classify_conversations_json(path: &Path) -> Option<Detection> {
    let parent = path.parent()?;
    let parent_name = parent
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let has_user_json = parent.join("user.json").is_file();
    let has_users_json = parent.join("users.json").is_file();

    let parent_is_chatgpt = parent_name.contains("chatgpt") || parent_name.contains("openai");
    let parent_is_claude = parent_name.contains("claude") || parent_name.contains("anthropic");

    if parent_is_chatgpt && has_user_json {
        return Some(Detection {
            format: "chatgpt".into(),
            path: path.to_path_buf(),
            confidence: Confidence::High,
            hint: format!(
                "parent dir name + sibling user.json in {}",
                parent.display()
            ),
        });
    }
    if parent_is_claude && has_users_json {
        return Some(Detection {
            format: "claude".into(),
            path: path.to_path_buf(),
            confidence: Confidence::High,
            hint: format!(
                "parent dir name + sibling users.json in {}",
                parent.display()
            ),
        });
    }
    if has_user_json {
        return Some(Detection {
            format: "chatgpt".into(),
            path: path.to_path_buf(),
            confidence: Confidence::Low,
            hint: format!(
                "sibling user.json in {} (parent name does not include 'chatgpt')",
                parent.display()
            ),
        });
    }
    if has_users_json {
        return Some(Detection {
            format: "claude".into(),
            path: path.to_path_buf(),
            confidence: Confidence::Low,
            hint: format!(
                "sibling users.json in {} (parent name does not include 'claude')",
                parent.display()
            ),
        });
    }
    if parent_is_chatgpt {
        return Some(Detection {
            format: "chatgpt".into(),
            path: path.to_path_buf(),
            confidence: Confidence::Low,
            hint: "parent dir name contains 'chatgpt' but no sibling user.json found".into(),
        });
    }
    if parent_is_claude {
        return Some(Detection {
            format: "claude".into(),
            path: path.to_path_buf(),
            confidence: Confidence::Low,
            hint: "parent dir name contains 'claude' but no sibling users.json found".into(),
        });
    }
    None
}

fn run_apply(home: &Path, detections: &[Detection]) -> Vec<ApplyResult> {
    let mut out: Vec<ApplyResult> = Vec::with_capacity(detections.len());
    for d in detections {
        if d.confidence != Confidence::High {
            // Ambiguous matches are surfaced to the user but never
            // auto-imported. Forces an explicit `localmem import`
            // for the user to confirm.
            out.push(ApplyResult {
                detection: d.clone(),
                stats: None,
                error: Some("low confidence; pass to `localmem import` explicitly".into()),
            });
            continue;
        }
        let stats = match d.format.as_str() {
            "chatgpt" => import_chatgpt(home, &d.path),
            "claude" => import_claude(home, &d.path),
            other => Err(anyhow::anyhow!("unknown import format: {other}")),
        };
        match stats {
            Ok(s) => out.push(ApplyResult {
                detection: d.clone(),
                stats: Some(s),
                error: None,
            }),
            Err(e) => out.push(ApplyResult {
                detection: d.clone(),
                stats: None,
                error: Some(format!("{e:#}")),
            }),
        }
    }
    out
}

fn write_output<W: Write>(
    out: &mut W,
    detections: &[Detection],
    applied: &[ApplyResult],
    as_json: bool,
) -> Result<()> {
    if as_json {
        let payload = JsonOutput {
            ok: true,
            detections,
            applied: applied.to_vec(),
        };
        serde_json::to_writer(&mut *out, &payload).context("serialize wizard JSON")?;
        out.write_all(b"\n").context("write JSON newline")?;
        return Ok(());
    }
    if detections.is_empty() {
        writeln!(
            out,
            "No memory exports found in ~/Downloads, ~/Desktop, or the current directory."
        )
        .context("write empty-detections line")?;
        writeln!(
            out,
            "If you have an export elsewhere, run `localmem import <format> <path>` directly."
        )
        .context("write hint line")?;
        return Ok(());
    }
    writeln!(
        out,
        "Found {} potential memory source(s):",
        detections.len()
    )
    .context("write detections header")?;
    for d in detections {
        let conf = match d.confidence {
            Confidence::High => "HIGH",
            Confidence::Low => "low",
        };
        writeln!(out, "  [{conf}] {} :: {}", d.format, d.path.display())
            .context("write detection row")?;
        writeln!(out, "         {}", d.hint).context("write detection hint")?;
    }
    if applied.is_empty() {
        writeln!(out).context("blank")?;
        writeln!(
            out,
            "Re-run with --apply to import every HIGH-confidence source.",
        )
        .context("write apply hint")?;
        return Ok(());
    }
    writeln!(out).context("blank")?;
    writeln!(out, "Applied:").context("write apply header")?;
    for a in applied {
        if let Some(stats) = &a.stats {
            writeln!(
                out,
                "  {} from {} :: {} events ({} conversations, {} skipped)",
                a.detection.format,
                a.detection.path.display(),
                stats.events_appended,
                stats.conversations_seen,
                stats.messages_skipped,
            )
            .context("write apply ok row")?;
        } else if let Some(err) = &a.error {
            writeln!(
                out,
                "  {} from {} :: SKIPPED ({err})",
                a.detection.format,
                a.detection.path.display(),
            )
            .context("write apply skip row")?;
        }
    }
    writeln!(out).context("blank")?;
    writeln!(
        out,
        "Next: run `localmem replay` to rebuild derived stores against the imported events.",
    )
    .context("write next-step hint")?;
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
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn detects_chatgpt_export_with_high_confidence() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let dir = root.join("chatgpt-export-2026-05-17");
        write(&dir.join("conversations.json"), "[]");
        write(&dir.join("user.json"), "{}");

        let dets = scan_paths(&[root]).unwrap();
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].format, "chatgpt");
        assert_eq!(dets[0].confidence, Confidence::High);
    }

    #[test]
    fn detects_claude_export_with_high_confidence() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let dir = root.join("data-2026-05-17-claude");
        write(&dir.join("conversations.json"), "[]");
        write(&dir.join("users.json"), "[]");

        let dets = scan_paths(&[root]).unwrap();
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].format, "claude");
        assert_eq!(dets[0].confidence, Confidence::High);
    }

    #[test]
    fn ambiguous_match_returns_low_confidence() {
        // sibling user.json but parent dir name doesn't include
        // "chatgpt" -> Low confidence chatgpt detection.
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let dir = root.join("random-folder");
        write(&dir.join("conversations.json"), "[]");
        write(&dir.join("user.json"), "{}");

        let dets = scan_paths(&[root]).unwrap();
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].format, "chatgpt");
        assert_eq!(dets[0].confidence, Confidence::Low);
    }

    #[test]
    fn empty_root_returns_no_detections() {
        let tmp = tempdir().unwrap();
        let dets = scan_paths(&[tmp.path().to_path_buf()]).unwrap();
        assert!(dets.is_empty());
    }

    #[test]
    fn unrelated_conversations_json_is_skipped() {
        // No sibling marker AND no parent-name hint -> skip.
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let dir = root.join("misc");
        write(&dir.join("conversations.json"), "[]");
        let dets = scan_paths(&[root]).unwrap();
        assert!(dets.is_empty());
    }

    #[test]
    fn deduplicates_paths_across_overlapping_roots() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let dir = root.join("chatgpt-export");
        write(&dir.join("conversations.json"), "[]");
        write(&dir.join("user.json"), "{}");
        let dets = scan_paths(&[root.clone(), root]).unwrap();
        assert_eq!(dets.len(), 1, "duplicate root must not double-report");
    }

    #[test]
    fn run_apply_skips_low_confidence_detections() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("localmem-home");
        std::fs::create_dir_all(&home).unwrap();
        let dir = tmp.path().join("random");
        write(&dir.join("conversations.json"), "[]");
        write(&dir.join("user.json"), "{}");

        let dets = scan_paths(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(dets[0].confidence, Confidence::Low);
        let applied = run_apply(&home, &dets);
        assert_eq!(applied.len(), 1);
        assert!(applied[0].stats.is_none());
        assert!(applied[0]
            .error
            .as_deref()
            .map(|e| e.contains("low confidence"))
            .unwrap_or(false));
    }

    #[test]
    fn run_apply_imports_high_confidence_chatgpt_export() {
        // End-to-end: a high-confidence chatgpt detection runs the
        // real importer and writes events to the home's events.jsonl.
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("localmem-home");
        std::fs::create_dir_all(&home).unwrap();
        let dir = tmp.path().join("chatgpt-export-2026-05-17");
        // A minimal valid conversations.json with one user message.
        let conv = r#"[
            {
                "title": "test convo",
                "mapping": {
                    "n1": {
                        "message": {
                            "author": {"role": "user"},
                            "content": {"content_type": "text", "parts": ["hello"]},
                            "create_time": 1700000000.0
                        }
                    }
                }
            }
        ]"#;
        write(&dir.join("conversations.json"), conv);
        write(&dir.join("user.json"), "{}");

        let dets = scan_paths(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(dets[0].confidence, Confidence::High);
        let applied = run_apply(&home, &dets);
        assert_eq!(applied.len(), 1);
        let stats = applied[0]
            .stats
            .as_ref()
            .expect("expected import to succeed");
        assert_eq!(stats.format, "chatgpt");
        // 1 marker + 1 capture.
        assert!(stats.events_appended >= 2);
    }

    #[test]
    fn human_output_empty_message_when_no_detections() {
        let mut buf = Vec::new();
        write_output(&mut buf, &[], &[], false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("No memory exports found"));
    }

    #[test]
    fn json_output_shape() {
        let dets = vec![Detection {
            format: "chatgpt".into(),
            path: PathBuf::from("/tmp/foo/conversations.json"),
            confidence: Confidence::High,
            hint: "test".into(),
        }];
        let mut buf = Vec::new();
        write_output(&mut buf, &dets, &[], true).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(json["ok"], true);
        let arr = json["detections"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["format"], "chatgpt");
        assert_eq!(arr[0]["confidence"], "high");
    }
}
