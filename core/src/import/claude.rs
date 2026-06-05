//! Claude / Anthropic data export importer. T-33.
//!
//! Anthropic's "Export data" feature gives a ZIP that contains
//! `conversations.json`. Schema differs from ChatGPT's: each
//! conversation has a `chat_messages` array (not a node-graph), each
//! message carries `sender` (`human` | `assistant`), `text`, and
//! `created_at` as RFC3339. Same constraint as the ChatGPT importer:
//! v0.1 expects the user to extract the ZIP first and pass the path
//! to `conversations.json`.

use super::{ingest_messages, ImportStats, ImportedMessage};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct ConversationFile(Vec<Conversation>);

#[derive(Debug, Deserialize)]
struct Conversation {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    chat_messages: Vec<ChatMessage>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

/// Parse a Claude export `conversations.json` file and ingest every
/// human message as a capture.
pub fn import_claude(home: &Path, json_path: &Path) -> Result<ImportStats> {
    let raw = std::fs::read_to_string(json_path)
        .with_context(|| format!("read Claude export at {}", json_path.display()))?;
    let file: ConversationFile =
        serde_json::from_str(&raw).context("parse Claude conversations.json")?;

    let conversations = file.0.len() as u64;
    let mut messages: Vec<ImportedMessage> = Vec::new();
    let mut skipped: u64 = 0;

    for conv in file.0 {
        let title = conv.name;
        for msg in conv.chat_messages {
            if msg.sender.as_deref() != Some("human") {
                continue;
            }
            let Some(text_raw) = msg.text else {
                skipped += 1;
                continue;
            };
            let text = text_raw.trim().to_string();
            if text.is_empty() {
                skipped += 1;
                continue;
            }
            let Some(ts_str) = msg.created_at else {
                skipped += 1;
                continue;
            };
            let Ok(ts) = DateTime::parse_from_rfc3339(&ts_str) else {
                skipped += 1;
                continue;
            };
            messages.push(ImportedMessage {
                text,
                timestamp: ts.with_timezone(&Utc),
                conversation_title: title.clone(),
            });
        }
    }
    messages.sort_by_key(|m| m.timestamp);

    ingest_messages(home, "claude", messages, conversations, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::init::init_home;
    use crate::event::EventKind;
    use crate::event_log::EventLog;
    use anyhow::Result;
    use tempfile::tempdir;

    fn sample_export() -> serde_json::Value {
        serde_json::json!([
            {
                "name": "Functional Rust ideas",
                "chat_messages": [
                    {
                        "sender": "human",
                        "text": "I keep reaching for macros. Is that idiomatic in Rust?",
                        "created_at": "2026-05-14T10:00:00Z"
                    },
                    {
                        "sender": "assistant",
                        "text": "Generally no; prefer trait objects.",
                        "created_at": "2026-05-14T10:00:30Z"
                    },
                    {
                        "sender": "human",
                        "text": "Got it, I prefer functional patterns then.",
                        "created_at": "2026-05-14T10:01:00Z"
                    }
                ]
            }
        ])
    }

    #[test]
    fn imports_only_human_messages_with_correct_timestamps() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let json_path = tmp.path().join("conv.json");
        std::fs::write(&json_path, sample_export().to_string()).unwrap();
        let stats = import_claude(tmp.path(), &json_path).unwrap();
        assert_eq!(stats.format, "claude");
        assert_eq!(stats.conversations_seen, 1);
        // 1 marker + 2 human messages.
        assert_eq!(stats.events_appended, 3);

        let log = EventLog::open(tmp.path()).unwrap();
        let events: Vec<_> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        let captures: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Capture(c) => Some(c.text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(captures.len(), 2);
        assert!(captures[0].contains("macros"));
        assert!(captures[1].contains("functional"));
    }

    #[test]
    fn skips_messages_with_no_timestamp() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let json = serde_json::json!([{
            "chat_messages": [
                { "sender": "human", "text": "no time" }
            ]
        }]);
        let json_path = tmp.path().join("c.json");
        std::fs::write(&json_path, json.to_string()).unwrap();
        let stats = import_claude(tmp.path(), &json_path).unwrap();
        assert_eq!(stats.events_appended, 1, "marker only");
        assert_eq!(stats.messages_skipped, 1);
    }

    #[test]
    fn empty_export_yields_marker_only() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let json_path = tmp.path().join("c.json");
        std::fs::write(&json_path, "[]").unwrap();
        let stats = import_claude(tmp.path(), &json_path).unwrap();
        assert_eq!(stats.conversations_seen, 0);
        assert_eq!(stats.events_appended, 1);
    }

    #[test]
    fn malformed_rfc3339_skips_message() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let json = serde_json::json!([{
            "chat_messages": [
                { "sender": "human", "text": "x", "created_at": "not a date" }
            ]
        }]);
        let json_path = tmp.path().join("c.json");
        std::fs::write(&json_path, json.to_string()).unwrap();
        let stats = import_claude(tmp.path(), &json_path).unwrap();
        assert_eq!(stats.messages_skipped, 1);
    }
}
