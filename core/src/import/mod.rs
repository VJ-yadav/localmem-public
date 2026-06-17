//! Bulk importers for foreign memory exports.
//!
//! Each importer parses a vendor-specific JSON file into a stream of
//! capture events and appends them to `events.jsonl`. The captures
//! preserve their original timestamps (so bitemporal queries reflect
//! when the thought actually happened, not when the import ran). One
//! `import` marker event lands at the head of the batch so replay can
//! attribute the captures back to their source.
//!
//! Importers DO NOT run the write policy or the extractor inline. The
//! pipeline is:
//!
//! 1. `localmem import FORMAT PATH` appends the marker + captures.
//! 2. The CLI invokes `localmem replay` automatically (or the user
//!    runs it manually) which then exercises policy + extractor +
//!    derived stores against every event in chronological order.
//!
//! This keeps the import step fast and idempotent: a partial import
//! that crashes mid-way leaves the log truncated at a complete event
//! boundary, and re-running replay rebuilds derived state cleanly.

pub mod chatgpt;
pub mod claude;
pub mod claude_code;

use crate::event::{CapturePayload, Event, EventKind, ImportPayload, Source};
use crate::event_id::EventId;
use crate::event_log::EventLog;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// Payload-level field (lives in `CapturePayload.extra`) carrying the
/// content hash of an imported message, so re-importing the same export is
/// idempotent: a message whose hash already appears in the log is skipped.
const IMPORT_HASH_FIELD: &str = "import_hash";

/// Stable content hash for an imported message: `(format, title, text,
/// original timestamp)`. Deterministic, so the same message from the same
/// export always hashes identically across re-imports.
fn import_hash(format: &str, msg: &ImportedMessage) -> String {
    let mut h = Sha256::new();
    h.update(format.as_bytes());
    h.update([0]);
    h.update(msg.conversation_title.as_deref().unwrap_or("").as_bytes());
    h.update([0]);
    h.update(msg.text.as_bytes());
    h.update([0]);
    h.update(msg.timestamp.to_rfc3339().as_bytes());
    format!("{:x}", h.finalize())
}

/// Outcome of a bulk import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportStats {
    pub format: String,
    pub batch_id: String,
    pub events_appended: u64,
    pub conversations_seen: u64,
    pub messages_skipped: u64,
    /// Messages skipped because an identical one was already imported
    /// (content-hash match). Makes re-import safe and repeatable.
    pub messages_deduped: u64,
}

/// A normalized message extracted from a foreign export. The importer
/// is responsible for filtering down to user-authored content (we don't
/// want to ingest the AI's own replies as our user's memories).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedMessage {
    pub text: String,
    pub timestamp: DateTime<Utc>,
    pub conversation_title: Option<String>,
    /// Container tags applied to the resulting capture. Importers that know the
    /// source project (e.g. Claude Code transcripts carry a cwd) populate
    /// `project` / `project_path` here so imported memory is project-scoped like
    /// live hook captures. Empty for sources without a project (ChatGPT/Claude.ai).
    pub tags: BTreeMap<String, String>,
}

/// Common ingest path. Importers parse their format into a `Vec<ImportedMessage>`
/// and hand the list here. We:
///
/// 1. Append one `Import` marker event so replay knows this batch landed.
/// 2. Append one `Capture` event per message with `source.app = format`.
///
/// Returns the stats reported to the CLI / MCP caller.
pub fn ingest_messages(
    home: &Path,
    format: &str,
    messages: Vec<ImportedMessage>,
    conversations_seen: u64,
    messages_skipped: u64,
) -> Result<ImportStats> {
    let event_log = EventLog::open(home).context("open event log for import")?;
    let batch_id = EventId::new().to_string();

    // Idempotent re-import: collect the content hashes already in the log so a
    // message imported before is skipped rather than duplicated. This is what
    // makes "commit events.jsonl, pull, re-import" safe (and re-running an
    // interrupted import a no-op for the part that landed).
    let mut seen = collect_import_hashes(&event_log).context("collect existing import hashes")?;

    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let user = std::env::var("USER").ok().filter(|s| !s.is_empty());

    let marker = Event::new(
        EventKind::Import(ImportPayload {
            source_format: format.to_string(),
            count: messages.len() as u64,
            batch_id: batch_id.clone(),
            extra: Map::new(),
        }),
        Source {
            app: format.to_string(),
            host: host.clone(),
            user: user.clone(),
        },
    );
    event_log
        .append(&marker)
        .context("append import marker event")?;

    let mut appended = 1u64;
    let mut deduped = 0u64;
    for msg in messages {
        let hash = import_hash(format, &msg);
        if !seen.insert(hash.clone()) {
            // Already imported (or a duplicate within this batch): skip.
            deduped += 1;
            continue;
        }
        let mut extra = Map::new();
        extra.insert(IMPORT_HASH_FIELD.to_string(), Value::String(hash));
        let mut capture = Event::new(
            EventKind::Capture(CapturePayload {
                text: msg.text,
                rewritten_text: None,
                kind: Default::default(),
                mime: None,
                attachments: vec![],
                tags: msg.tags,
                // Imported captures carry an envelope built from the source's
                // original instant (mirrors `capture.ts` below), so bitemporal
                // queries answer "what was true at T", not import time.
                time: Some(crate::temporal::TimeEnvelope::from_instant(msg.timestamp)),
                extra,
            }),
            Source {
                app: format.to_string(),
                host: host.clone(),
                user: user.clone(),
            },
        );
        // Preserve original wall-clock time so bitemporal queries answer
        // "what was true at T" honestly, not "when did I run the import".
        capture.ts = msg.timestamp;
        event_log
            .append(&capture)
            .context("append imported capture")?;
        appended += 1;
    }

    Ok(ImportStats {
        format: format.to_string(),
        batch_id,
        events_appended: appended,
        conversations_seen,
        messages_skipped,
        messages_deduped: deduped,
    })
}

/// A parsed-but-not-yet-ingested import. Adapters produce this so the CLI can
/// either ingest it or preview it (`--dry-run`) without writing.
#[derive(Debug, Clone)]
pub struct ParsedImport {
    pub messages: Vec<ImportedMessage>,
    pub conversations_seen: u64,
    pub messages_skipped: u64,
}

/// What a `--dry-run` import would do, computed without writing anything.
#[derive(Debug, Clone, Serialize)]
pub struct PreviewStats {
    pub format: String,
    pub total_messages: u64,
    /// Messages that would be newly appended.
    pub new_messages: u64,
    /// Messages already in the log (content-hash match) that would be skipped.
    pub already_imported: u64,
    pub conversations_seen: u64,
    /// Messages the adapter dropped while parsing (no timestamp, AI replies…).
    pub messages_skipped: u64,
}

/// Convenience: ingest an already-parsed import.
pub fn ingest_parsed(home: &Path, format: &str, parsed: ParsedImport) -> Result<ImportStats> {
    ingest_messages(
        home,
        format,
        parsed.messages,
        parsed.conversations_seen,
        parsed.messages_skipped,
    )
}

/// Collect the content hashes of every already-imported capture in the log.
/// Shared by [`ingest_messages`] (to skip duplicates) and [`preview_import`].
fn collect_import_hashes(event_log: &EventLog) -> Result<HashSet<String>> {
    let mut seen = HashSet::new();
    for ev in event_log
        .iter()
        .context("scan event log for existing imports")?
    {
        let ev = ev.context("read event during import dedup scan")?;
        if let EventKind::Capture(p) = &ev.kind {
            if let Some(h) = p.extra.get(IMPORT_HASH_FIELD).and_then(|v| v.as_str()) {
                seen.insert(h.to_string());
            }
        }
    }
    Ok(seen)
}

/// Compute what importing `parsed` would do, without writing. Powers
/// `localmem import --dry-run` and, by extension, a guided import wizard in the
/// dashboard ("found N messages, M are new").
pub fn preview_import(home: &Path, format: &str, parsed: &ParsedImport) -> Result<PreviewStats> {
    let event_log = EventLog::open(home).context("open event log for import preview")?;
    let mut seen = collect_import_hashes(&event_log).context("collect existing import hashes")?;
    let (mut new_messages, mut already_imported) = (0u64, 0u64);
    for msg in &parsed.messages {
        if seen.insert(import_hash(format, msg)) {
            new_messages += 1;
        } else {
            already_imported += 1;
        }
    }
    Ok(PreviewStats {
        format: format.to_string(),
        total_messages: parsed.messages.len() as u64,
        new_messages,
        already_imported,
        conversations_seen: parsed.conversations_seen,
        messages_skipped: parsed.messages_skipped,
    })
}

/// Best-effort auto-detection of an export's format from its contents, so the
/// user can run `localmem import <path>` with no `--format`. Recognizes the
/// ChatGPT and Claude conversation exports and a localmem `archive`
/// (events.jsonl, e.g. a re-imported `localmem export`). Returns `None` when it
/// can't tell, so the caller asks the user to specify.
pub fn detect_format(path: &Path) -> Option<&'static str> {
    // A directory only makes sense as a Claude Code transcript tree
    // (`~/.claude/projects`); the vendor/archive formats are single files.
    if path.is_dir() {
        return claude_code::dir_looks_like_transcripts(path).then_some("claude-code");
    }
    let raw = std::fs::read_to_string(path).ok()?;
    // Vendor exports are a single JSON array of conversations.
    if let Ok(v) = serde_json::from_str::<Value>(&raw) {
        if let Some(first) = v.as_array().and_then(|a| a.first()) {
            if first.get("mapping").is_some() {
                return Some("chatgpt");
            }
            if first.get("chat_messages").is_some() {
                return Some("claude");
            }
        }
    }
    // JSONL formats: a Claude Code session transcript, or a localmem archive
    // (header or bare event). A transcript may open with a `summary` line that
    // lacks `sessionId`, so scan the first several lines for the signature
    // before falling back to the archive check on the first line.
    let mut nonempty = raw.lines().filter(|l| !l.trim().is_empty());
    if nonempty
        .clone()
        .take(30)
        .any(claude_code::looks_like_transcript_line)
    {
        return Some("claude-code");
    }
    if let Some(line) = nonempty.next() {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.get("archive_version").is_some() {
                return Some("archive");
            }
        }
        if serde_json::from_str::<Event>(line).is_ok() {
            return Some("archive");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn msg(text: &str, secs: i64) -> ImportedMessage {
        ImportedMessage {
            text: text.into(),
            timestamp: DateTime::<Utc>::from_timestamp(secs, 0).unwrap(),
            conversation_title: Some("conv".into()),
            tags: Default::default(),
        }
    }

    #[test]
    fn reimport_is_idempotent_via_content_hash() {
        let tmp = tempdir().unwrap();
        let msgs = vec![msg("a", 1000), msg("b", 2000)];
        let s1 = ingest_messages(tmp.path(), "chatgpt", msgs.clone(), 1, 0).unwrap();
        assert_eq!(s1.events_appended, 3, "1 marker + 2 captures");
        assert_eq!(s1.messages_deduped, 0);
        // Re-import the same export: every message dedupes; only the marker lands.
        let s2 = ingest_messages(tmp.path(), "chatgpt", msgs, 1, 0).unwrap();
        assert_eq!(s2.messages_deduped, 2);
        assert_eq!(s2.events_appended, 1, "marker only; no duplicate captures");
    }

    #[test]
    fn new_messages_after_reimport_are_appended() {
        let tmp = tempdir().unwrap();
        ingest_messages(tmp.path(), "chatgpt", vec![msg("a", 1000)], 1, 0).unwrap();
        // Same "a" plus a new "c": only "c" is appended.
        let s = ingest_messages(
            tmp.path(),
            "chatgpt",
            vec![msg("a", 1000), msg("c", 3000)],
            1,
            0,
        )
        .unwrap();
        assert_eq!(s.messages_deduped, 1, "the repeated message dedupes");
        assert_eq!(s.events_appended, 2, "marker + the one new capture");
    }

    #[test]
    fn duplicate_within_a_single_batch_is_deduped() {
        let tmp = tempdir().unwrap();
        let s = ingest_messages(
            tmp.path(),
            "chatgpt",
            vec![msg("a", 1000), msg("a", 1000)],
            1,
            0,
        )
        .unwrap();
        assert_eq!(s.messages_deduped, 1);
        assert_eq!(s.events_appended, 2, "marker + one 'a'");
    }

    #[test]
    fn detect_format_recognizes_vendor_and_archive() {
        let tmp = tempdir().unwrap();
        let cg = tmp.path().join("cg.json");
        std::fs::write(&cg, r#"[{"title":"t","mapping":{}}]"#).unwrap();
        assert_eq!(detect_format(&cg), Some("chatgpt"));

        let cl = tmp.path().join("cl.json");
        std::fs::write(&cl, r#"[{"name":"t","chat_messages":[]}]"#).unwrap();
        assert_eq!(detect_format(&cl), Some("claude"));

        let unknown = tmp.path().join("u.json");
        std::fs::write(&unknown, r#"{"hello":"world"}"#).unwrap();
        assert_eq!(detect_format(&unknown), None);
    }

    #[test]
    fn preview_import_counts_new_vs_already_imported() {
        let tmp = tempdir().unwrap();
        let parsed = ParsedImport {
            messages: vec![msg("a", 1000), msg("b", 2000)],
            conversations_seen: 1,
            messages_skipped: 0,
        };
        // Nothing imported yet: both messages are new.
        let pv = preview_import(tmp.path(), "chatgpt", &parsed).unwrap();
        assert_eq!(pv.new_messages, 2);
        assert_eq!(pv.already_imported, 0);

        // Import them, then preview the same set again: both already-imported.
        ingest_messages(tmp.path(), "chatgpt", parsed.messages.clone(), 1, 0).unwrap();
        let pv2 = preview_import(tmp.path(), "chatgpt", &parsed).unwrap();
        assert_eq!(pv2.new_messages, 0);
        assert_eq!(pv2.already_imported, 2);
    }
}
