//! ChatGPT data export importer. T-32.
//!
//! Format: OpenAI's "Export data" feature gives you a ZIP that, among
//! other files, contains `conversations.json`. The JSON is an array of
//! conversation objects, each carrying a `mapping` (node-id -> node)
//! that forms a tree of messages. We walk every node, keep only
//! `author.role == "user"` messages, and emit one capture per kept
//! message ordered by `create_time`.
//!
//! The user is expected to extract the ZIP first and pass us the path
//! to `conversations.json`. v0.1 does not bundle a zip decoder; this
//! keeps the importer dependency-free and lets users skim the JSON
//! before ingestion.
//!
//! Format notes (observed empirically; may shift):
//!   - `create_time` is a float seconds-since-epoch. Sometimes null
//!     for system-injected nodes; we skip those.
//!   - `content.content_type` is `text` for the messages we care about.
//!     Other types (multimodal, code_interpreter, ...) are skipped in
//!     v0.1; their text representation would need transcoding rules
//!     this importer does not own.
//!   - `parts` is an array of strings. We join with "\n\n" preserving
//!     the original chunking so the source-text round-trip is honest.

use super::{ingest_parsed, ImportStats, ImportedMessage, ParsedImport};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct ConversationFile(Vec<Conversation>);

#[derive(Debug, Deserialize)]
struct Conversation {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    mapping: HashMap<String, Node>,
}

#[derive(Debug, Deserialize)]
struct Node {
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    author: Option<Author>,
    #[serde(default)]
    content: Option<Content>,
    #[serde(default)]
    create_time: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Author {
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Content {
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    parts: Vec<serde_json::Value>,
}

/// Parse a `conversations.json` file and ingest every user message.
///
/// Returns `ImportStats` with `format = "chatgpt"`.
/// Ingest a ChatGPT export into `home` (parse + dedup + append).
pub fn import_chatgpt(home: &Path, json_path: &Path) -> Result<ImportStats> {
    ingest_parsed(home, "chatgpt", parse_chatgpt(json_path)?)
}

/// Parse a ChatGPT `conversations.json` into normalized messages without
/// writing anything (so the CLI can `--dry-run` / preview).
pub fn parse_chatgpt(json_path: &Path) -> Result<ParsedImport> {
    let raw = std::fs::read_to_string(json_path)
        .with_context(|| format!("read ChatGPT export at {}", json_path.display()))?;
    let file: ConversationFile = serde_json::from_str(&raw).context("parse conversations.json")?;

    let mut messages: Vec<ImportedMessage> = Vec::new();
    let mut skipped: u64 = 0;
    let conversations = file.0.len() as u64;

    for conv in file.0 {
        let title = conv.title;
        let mut local: Vec<ImportedMessage> = Vec::new();
        for (_id, node) in conv.mapping {
            let Some(msg) = node.message else {
                continue;
            };
            let role = msg
                .author
                .as_ref()
                .and_then(|a| a.role.as_deref())
                .unwrap_or("");
            if role != "user" {
                continue;
            }
            let Some(create_time) = msg.create_time else {
                skipped += 1;
                continue;
            };
            let Some(content) = msg.content else {
                skipped += 1;
                continue;
            };
            // Only plain-text messages in v0.1; multimodal would need
            // image/audio attachment plumbing this importer doesn't
            // (yet) own.
            if content.content_type.as_deref().is_some_and(|t| t != "text") {
                skipped += 1;
                continue;
            }
            let text = content
                .parts
                .iter()
                .filter_map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
                .trim()
                .to_string();
            if text.is_empty() {
                skipped += 1;
                continue;
            }
            let ts = epoch_to_datetime(create_time)?;
            local.push(ImportedMessage {
                text,
                timestamp: ts,
                conversation_title: title.clone(),
                tags: Default::default(),
            });
        }
        // Sort within conversation by timestamp; concatenate across
        // conversations so the final ingest stream is chronological.
        local.sort_by_key(|m| m.timestamp);
        messages.extend(local);
    }

    // Cross-conversation final sort. A user may have had two chats
    // interleaved; bitemporal correctness wants strictly ascending ts.
    messages.sort_by_key(|m| m.timestamp);

    Ok(ParsedImport {
        messages,
        conversations_seen: conversations,
        messages_skipped: skipped,
    })
}

fn epoch_to_datetime(secs: f64) -> Result<DateTime<Utc>> {
    let whole = secs.trunc() as i64;
    let frac_nanos = ((secs - secs.trunc()) * 1e9).round() as u32;
    DateTime::<Utc>::from_timestamp(whole, frac_nanos)
        .with_context(|| format!("convert epoch {secs} to datetime"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::init::init_home;
    use crate::event::EventKind;
    use crate::event_log::EventLog;
    use anyhow::Result;
    use tempfile::tempdir;

    /// Minimal sample mirroring ChatGPT's actual export shape.
    fn sample_conversations() -> serde_json::Value {
        serde_json::json!([
            {
                "title": "Rust questions",
                "mapping": {
                    "n1": {
                        "id": "n1",
                        "message": {
                            "id": "m1",
                            "author": { "role": "system" },
                            "content": { "content_type": "text", "parts": ["Be helpful."] },
                            "create_time": 1700000000.0
                        }
                    },
                    "n2": {
                        "id": "n2",
                        "message": {
                            "id": "m2",
                            "author": { "role": "user" },
                            "content": {
                                "content_type": "text",
                                "parts": ["How do I parse JSON in Rust?"]
                            },
                            "create_time": 1700000001.5
                        }
                    },
                    "n3": {
                        "id": "n3",
                        "message": {
                            "id": "m3",
                            "author": { "role": "assistant" },
                            "content": { "content_type": "text", "parts": ["Use serde_json."] },
                            "create_time": 1700000002.0
                        }
                    },
                    "n4": {
                        "id": "n4",
                        "message": {
                            "id": "m4",
                            "author": { "role": "user" },
                            "content": {
                                "content_type": "text",
                                "parts": ["Thanks. Show an example?"]
                            },
                            "create_time": 1700000003.0
                        }
                    }
                }
            }
        ])
    }

    #[test]
    fn import_chatgpt_emits_user_messages_in_chronological_order() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let json_path = tmp.path().join("conversations.json");
        std::fs::write(&json_path, sample_conversations().to_string()).unwrap();

        let stats = import_chatgpt(tmp.path(), &json_path).unwrap();
        assert_eq!(stats.format, "chatgpt");
        assert_eq!(stats.conversations_seen, 1);
        // 1 Import marker + 2 user captures = 3 appended events.
        assert_eq!(stats.events_appended, 3);

        let log = EventLog::open(tmp.path()).unwrap();
        let events: Vec<_> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        // First event is the import marker.
        assert!(matches!(events[0].kind, EventKind::Import(_)));
        // Then captures in time order.
        let captures: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Capture(c) => Some((e.ts, c.text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(captures.len(), 2);
        assert!(captures[0].1.starts_with("How do I parse"));
        assert!(captures[1].1.starts_with("Thanks."));
        assert!(
            captures[0].0 < captures[1].0,
            "captures must be chronological"
        );
    }

    #[test]
    fn skips_non_text_content_types() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let json = serde_json::json!([{
            "title": null,
            "mapping": {
                "n1": {
                    "id": "n1",
                    "message": {
                        "author": { "role": "user" },
                        "content": { "content_type": "multimodal_text", "parts": ["[image]"] },
                        "create_time": 1700000000.0
                    }
                }
            }
        }]);
        let json_path = tmp.path().join("c.json");
        std::fs::write(&json_path, json.to_string()).unwrap();
        let stats = import_chatgpt(tmp.path(), &json_path).unwrap();
        assert_eq!(stats.events_appended, 1, "only the marker, no captures");
        assert_eq!(stats.messages_skipped, 1);
    }

    #[test]
    fn missing_create_time_skips_message() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let json = serde_json::json!([{
            "mapping": {
                "n1": {
                    "message": {
                        "author": { "role": "user" },
                        "content": { "content_type": "text", "parts": ["no time"] }
                    }
                }
            }
        }]);
        let json_path = tmp.path().join("c.json");
        std::fs::write(&json_path, json.to_string()).unwrap();
        let stats = import_chatgpt(tmp.path(), &json_path).unwrap();
        assert_eq!(stats.events_appended, 1);
        assert_eq!(stats.messages_skipped, 1);
    }

    #[test]
    fn empty_file_yields_marker_only() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let json_path = tmp.path().join("c.json");
        std::fs::write(&json_path, "[]").unwrap();
        let stats = import_chatgpt(tmp.path(), &json_path).unwrap();
        assert_eq!(stats.conversations_seen, 0);
        assert_eq!(stats.events_appended, 1, "marker only");
    }

    #[test]
    fn malformed_json_errors_clearly() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let json_path = tmp.path().join("c.json");
        std::fs::write(&json_path, "this is not json").unwrap();
        let err = import_chatgpt(tmp.path(), &json_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse conversations.json"), "got: {msg}");
    }
}
