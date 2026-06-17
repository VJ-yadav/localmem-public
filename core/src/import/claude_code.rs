//! Claude Code session-history importer.
//!
//! Claude Code stores every session as an append-only JSONL transcript under
//! `~/.claude/projects/<slugified-cwd>/<session-id>.jsonl`. Each line is one
//! event: user turns, assistant turns, tool results, queue ops, snapshots, etc.
//!
//! Unlike the clean ChatGPT / Claude.ai exports (which are already just
//! user/assistant pairs), a Claude Code transcript is mostly machine noise:
//! in a typical session a handful of genuine typed prompts sit among hundreds
//! of `tool_result` entries (which are also `type: "user"`), assistant turns,
//! sidechain (subagent) turns, and command/system-reminder wrappers. The
//! importer's real job is to keep ONLY what the human actually typed:
//!
//! - `type == "user"` and `message.role == "user"`
//! - not a sidechain (subagent) turn, not a meta entry
//! - content reduced to its `text` parts (tool_result / image / document parts
//!   dropped); empty after that → skipped
//! - not a slash-command / local-command-output / system-reminder wrapper
//! - carries a parseable RFC3339 `timestamp` (the real moment you typed it)
//!
//! The path may be a single `.jsonl` session file OR a directory (e.g.
//! `~/.claude/projects`), in which case every `*.jsonl` under it is walked.

use super::{ingest_parsed, ImportStats, ImportedMessage, ParsedImport};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    message: Option<Msg>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "isSidechain")]
    is_sidechain: bool,
    #[serde(default, rename = "isMeta")]
    is_meta: bool,
}

#[derive(Debug, Deserialize)]
struct Msg {
    #[serde(default)]
    role: Option<String>,
    /// Either a plain string or an array of typed content blocks.
    #[serde(default)]
    content: Option<Value>,
}

/// Ingest a Claude Code transcript (file or directory) into `home`.
pub fn import_claude_code(home: &Path, path: &Path) -> Result<ImportStats> {
    ingest_parsed(home, "claude-code", parse_claude_code(path)?)
}

/// Parse a Claude Code transcript into normalized messages without writing
/// (so the CLI can `--dry-run` / preview). `path` is a single `.jsonl` session
/// file or a directory of them (walked recursively).
pub fn parse_claude_code(path: &Path) -> Result<ParsedImport> {
    let files = collect_session_files(path)?;
    let mut messages: Vec<ImportedMessage> = Vec::new();
    let mut skipped: u64 = 0;
    let mut sessions_seen: u64 = 0;

    for file in &files {
        sessions_seen += 1;
        parse_session_file(file, &mut messages, &mut skipped)
            .with_context(|| format!("parse Claude Code session {}", file.display()))?;
    }

    // Chronological across all sessions so the replayed timeline is ordered.
    messages.sort_by_key(|m| m.timestamp);

    Ok(ParsedImport {
        messages,
        conversations_seen: sessions_seen,
        messages_skipped: skipped,
    })
}

/// Read one session `.jsonl`, appending its genuine user prompts to `messages`
/// and counting filtered-out lines in `skipped`. A line that fails to parse is
/// skipped (not fatal): transcripts are append-only and may end mid-write.
fn parse_session_file(
    file: &Path,
    messages: &mut Vec<ImportedMessage>,
    skipped: &mut u64,
) -> Result<()> {
    let f = std::fs::File::open(file).with_context(|| format!("open {}", file.display()))?;
    for line in BufReader::new(f).lines() {
        let line = line.context("read transcript line")?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(&line) else {
            // Unparseable / partial line: ignore rather than abort the import.
            continue;
        };
        match classify(&entry) {
            Some(msg) => messages.push(msg),
            None if is_candidate_user_turn(&entry) => *skipped += 1,
            None => {}
        }
    }
    Ok(())
}

/// True for entries that are a user turn we considered but dropped (tool
/// results, empty text, command wrappers, missing timestamp). Used only to
/// count `messages_skipped` honestly without counting assistant/system events.
fn is_candidate_user_turn(entry: &Entry) -> bool {
    entry.kind.as_deref() == Some("user") && !entry.is_sidechain && !entry.is_meta
}

/// Reduce one transcript entry to a genuine typed user prompt, or `None`.
fn classify(entry: &Entry) -> Option<ImportedMessage> {
    if entry.kind.as_deref() != Some("user") || entry.is_sidechain || entry.is_meta {
        return None;
    }
    let msg = entry.message.as_ref()?;
    if msg.role.as_deref() != Some("user") {
        return None;
    }
    let text = extract_text(msg.content.as_ref()?)?;
    let text = text.trim();
    if text.is_empty() || is_wrapper_noise(text) {
        return None;
    }
    // The real moment the prompt was typed (recorded-at == valid-time here).
    let ts = DateTime::parse_from_rfc3339(entry.timestamp.as_deref()?)
        .ok()?
        .with_timezone(&Utc);
    // Project tags from the cwd: `project` (readable basename) + `project_path`
    // (collision-proof full path), so imported sessions are scoped exactly like
    // the live hook captures — one store, partitioned per project.
    let mut tags = std::collections::BTreeMap::new();
    if let Some(cwd) = entry.cwd.as_deref().map(|c| c.trim_end_matches('/')) {
        if !cwd.is_empty() {
            tags.insert("project_path".to_string(), cwd.to_string());
            tags.insert("project".to_string(), project_label(cwd));
        }
    }
    Some(ImportedMessage {
        text: text.to_string(),
        timestamp: ts,
        // Project provenance: the cwd's last path segment. Stable, so re-import
        // dedup (which hashes the title) stays deterministic.
        conversation_title: entry.cwd.as_deref().map(project_label),
        tags,
    })
}

/// Pull the human-authored text out of a message's `content`, which is either a
/// plain string or an array of typed blocks. Only `text` blocks count; tool
/// results, images, documents, and thinking are dropped.
fn extract_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut buf = String::new();
            for p in parts {
                if p.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                        if !buf.is_empty() {
                            buf.push('\n');
                        }
                        buf.push_str(t);
                    }
                }
            }
            (!buf.is_empty()).then_some(buf)
        }
        _ => None,
    }
}

/// Slash-command, local-command output, and injected system text are not things
/// the user "remembers" — they are harness plumbing. Drop them.
fn is_wrapper_noise(text: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "<local-command-stdout>",
        "<local-command-stderr>",
        "<system-reminder>",
        "Caveat:",
        "[Request interrupted",
    ];
    PREFIXES.iter().any(|p| text.starts_with(p))
}

/// Last path segment of a cwd, used as the conversation/project label.
fn project_label(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(cwd)
        .to_string()
}

/// Resolve the import target to a list of session `.jsonl` files. A file yields
/// itself; a directory is walked recursively for `*.jsonl`.
fn collect_session_files(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        anyhow::bail!("{} is neither a file nor a directory", path.display());
    }
    let mut out = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("read dir {}", dir.display()))?;
        for entry in entries {
            let entry = entry.context("read dir entry")?;
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(p);
            }
        }
    }
    // Deterministic order so re-imports and tests are stable.
    out.sort();
    Ok(out)
}

/// Cheap signature test for "this `.jsonl` line is a Claude Code transcript
/// entry", used by `super::detect_format`. Every transcript line carries a
/// `sessionId` alongside a `type`; a localmem archive event has neither, and a
/// vendor export is not JSONL at all. (Not every line has `uuid` — `summary`
/// and `queue-operation` lines don't — so we key on `sessionId`.)
pub(super) fn looks_like_transcript_line(line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    v.get("sessionId").is_some() && v.get("type").is_some()
}

/// Probe a directory for a Claude Code transcript: is any line of any early
/// `*.jsonl` a transcript entry? Scans several lines of several files (not just
/// the first line of the first file — real sessions often open with a `summary`
/// or `queue-operation` line that carries no `sessionId`).
pub(super) fn dir_looks_like_transcripts(path: &Path) -> bool {
    let Ok(files) = collect_session_files(path) else {
        return false;
    };
    for f in files.iter().take(16) {
        if let Ok(file) = std::fs::File::open(f) {
            for line in BufReader::new(file).lines().take(50).map_while(Result::ok) {
                if !line.trim().is_empty() && looks_like_transcript_line(&line) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::init::init_home;
    use crate::event::EventKind;
    use crate::event_log::EventLog;
    use tempfile::tempdir;

    /// One line of a Claude Code transcript.
    fn line(v: serde_json::Value) -> String {
        v.to_string()
    }

    fn user_text(text: &str, ts: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "user", "sessionId": "s1", "uuid": "u-1", "cwd": "/Users/me/code/myproj",
            "timestamp": ts,
            "message": {"role": "user", "content": [{"type": "text", "text": text}]}
        })
    }

    fn write_transcript(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, lines.join("\n")).unwrap();
        p
    }

    #[test]
    fn keeps_typed_prompts_drops_tool_results_and_assistant() {
        let tmp = tempdir().unwrap();
        let lines = vec![
            line(user_text(
                "how do I migrate billing to postgres?",
                "2024-03-01T09:00:00Z",
            )),
            // tool_result-only user turn (the dominant noise) -> dropped
            line(serde_json::json!({
                "type": "user", "sessionId": "s1", "uuid": "u-2", "timestamp": "2024-03-01T09:00:05Z",
                "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}
            })),
            // assistant turn -> dropped
            line(serde_json::json!({
                "type": "assistant", "sessionId": "s1", "uuid": "u-3", "timestamp": "2024-03-01T09:00:10Z",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "use JSONB"}]}
            })),
            line(user_text(
                "great, use JSONB columns for the metadata",
                "2024-03-01T09:01:00Z",
            )),
        ];
        let file = write_transcript(tmp.path(), "session.jsonl", &lines);
        let parsed = parse_claude_code(&file).unwrap();
        assert_eq!(parsed.messages.len(), 2, "only the two typed prompts");
        assert!(parsed.messages[0].text.contains("migrate billing"));
        assert!(parsed.messages[1].text.contains("JSONB columns"));
        // The tool_result user turn is counted as a skip; assistant is not.
        assert_eq!(parsed.messages_skipped, 1);
        assert_eq!(
            parsed.messages[0].conversation_title.as_deref(),
            Some("myproj")
        );
    }

    #[test]
    fn drops_sidechain_meta_and_command_wrappers() {
        let tmp = tempdir().unwrap();
        let lines = vec![
            // sidechain (subagent) -> dropped
            {
                let mut v = user_text("internal subagent turn", "2024-03-01T09:00:00Z");
                v["isSidechain"] = serde_json::json!(true);
                line(v)
            },
            // slash-command wrapper -> dropped
            line(user_text(
                "<command-name>/clear</command-name>",
                "2024-03-01T09:00:01Z",
            )),
            // system reminder -> dropped
            line(user_text(
                "<system-reminder>be concise</system-reminder>",
                "2024-03-01T09:00:02Z",
            )),
            // genuine -> kept
            line(user_text(
                "remember I prefer tabs over spaces",
                "2024-03-01T09:00:03Z",
            )),
        ];
        let file = write_transcript(tmp.path(), "s.jsonl", &lines);
        let parsed = parse_claude_code(&file).unwrap();
        assert_eq!(parsed.messages.len(), 1);
        assert!(parsed.messages[0].text.contains("tabs over spaces"));
    }

    #[test]
    fn preserves_original_timestamp_end_to_end() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let lines = vec![line(user_text(
            "we shipped v2 on this day",
            "2023-07-04T12:00:00Z",
        ))];
        let file = write_transcript(tmp.path(), "s.jsonl", &lines);
        let stats = import_claude_code(tmp.path(), &file).unwrap();
        assert_eq!(stats.format, "claude-code");
        assert_eq!(stats.events_appended, 2, "1 marker + 1 capture");

        let log = EventLog::open(tmp.path()).unwrap();
        let events: Vec<_> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        let capture = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::Capture(c) => Some((c, e.ts)),
                _ => None,
            })
            .expect("a capture");
        // Valid-time is the original prompt instant, not import-now.
        assert_eq!(
            capture.0.effective_capture_instant(capture.1).to_rfc3339(),
            "2023-07-04T12:00:00+00:00"
        );
    }

    #[test]
    fn walks_a_directory_of_sessions_and_sorts_chronologically() {
        let tmp = tempdir().unwrap();
        let projdir = tmp.path().join("projects").join("proj-a");
        std::fs::create_dir_all(&projdir).unwrap();
        write_transcript(
            &projdir,
            "later.jsonl",
            &[line(user_text("second thing", "2024-05-02T00:00:00Z"))],
        );
        write_transcript(
            &projdir,
            "earlier.jsonl",
            &[line(user_text("first thing", "2024-05-01T00:00:00Z"))],
        );
        let parsed = parse_claude_code(&tmp.path().join("projects")).unwrap();
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.conversations_seen, 2);
        assert!(parsed.messages[0].text.contains("first"));
        assert!(parsed.messages[1].text.contains("second"));
    }

    #[test]
    fn detects_transcript_line_and_directory() {
        let tmp = tempdir().unwrap();
        let l = line(user_text("hi", "2024-01-01T00:00:00Z"));
        assert!(looks_like_transcript_line(&l));
        assert!(!looks_like_transcript_line(r#"{"archive_version":1}"#));
        write_transcript(tmp.path(), "s.jsonl", &[l]);
        assert!(dir_looks_like_transcripts(tmp.path()));
    }
}
