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

use crate::event::{CapturePayload, Event, EventKind, ImportPayload, Source};
use crate::event_id::EventId;
use crate::event_log::EventLog;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Map;
use std::path::Path;

/// Outcome of a bulk import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportStats {
    pub format: String,
    pub batch_id: String,
    pub events_appended: u64,
    pub conversations_seen: u64,
    pub messages_skipped: u64,
}

/// A normalized message extracted from a foreign export. The importer
/// is responsible for filtering down to user-authored content (we don't
/// want to ingest the AI's own replies as our user's memories).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedMessage {
    pub text: String,
    pub timestamp: DateTime<Utc>,
    pub conversation_title: Option<String>,
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
    for msg in messages {
        let mut capture = Event::new(
            EventKind::Capture(CapturePayload {
                text: msg.text,
                rewritten_text: None,
                kind: Default::default(),
                mime: None,
                attachments: vec![],
                tags: Default::default(),
                extra: Map::new(),
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
    })
}
