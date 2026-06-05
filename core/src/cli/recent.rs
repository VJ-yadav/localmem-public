//! `localmem recent` handler (T-53).
//!
//! Streams the last N capture events from `events.jsonl`, newest first.
//! "Capture" events only: facts, updates, forgets, policy entries are
//! audit detail surfaced via other commands (`recall`, `audit`,
//! `journal`). Captures are the user-visible memory items, so this is
//! the "what was just remembered?" surface MCP Resources subscribe to.
//!
//! Forgotten captures are dropped from the listing: a `forget` event
//! targeting a capture id retires the memory from the user's view,
//! and `recent` mirrors that (otherwise users would see ghosts of
//! deleted memories on this surface). The event log is still intact
//! for audit; `localmem audit <id>` walks the full lineage.

use crate::event::{Event, EventKind, ForgetPayload};
use crate::event_id::EventId;
use crate::event_log::EventLog;
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::path::PathBuf;

/// Default cap matches SPEC_V0_2 "Last N captures (default 20)".
pub const DEFAULT_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct RecentCapture {
    pub event_id: String,
    pub ts: String,
    pub text: String,
    pub kind: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    pub source_app: String,
}

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    ok: bool,
    captures: &'a [RecentCapture],
}

pub fn run(home: Option<&str>, limit: usize, as_json: bool) -> Result<()> {
    let home = resolve_home(home)?;
    let log = EventLog::open(&home).context("open event log")?;
    let rows = load_recent(&log, limit)?;
    let mut out = io::stdout().lock();
    write_output(&mut out, &rows, as_json)
}

/// Walk the event log once, keep a fixed-size sliding window of the
/// most-recent N capture events, then reverse for newest-first
/// output. `forget` events targeting earlier captures drop those
/// captures from the window so the listing reflects the user's
/// current view, not raw log order.
///
/// `limit == 0` is allowed and returns an empty list; callers that
/// want the default cap should pass [`DEFAULT_LIMIT`].
pub fn load_recent(log: &EventLog, limit: usize) -> Result<Vec<RecentCapture>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    // First pass: collect forgotten ids so we can drop them in the
    // second pass. Two-pass keeps logic obvious; for the discovery
    // surface this is fine.
    let forgotten = collect_forgotten_capture_ids(log)?;
    let mut window: VecDeque<RecentCapture> = VecDeque::with_capacity(limit + 1);
    for ev in log.iter()? {
        let ev = ev?;
        if forgotten.contains(&ev.id) {
            continue;
        }
        if let Some(row) = capture_row(&ev) {
            if window.len() == limit {
                window.pop_front();
            }
            window.push_back(row);
        }
    }
    // VecDeque -> Vec, then reverse for newest-first.
    let mut out: Vec<RecentCapture> = window.into_iter().collect();
    out.reverse();
    Ok(out)
}

fn capture_row(ev: &Event) -> Option<RecentCapture> {
    let EventKind::Capture(p) = &ev.kind else {
        return None;
    };
    // Surface the user-visible text: rewritten when present (T-55),
    // else original. That matches what `localmem search` shows and
    // what MCP Resources will return on `localmem://recent`.
    let text = p.indexable_text().to_string();
    Some(RecentCapture {
        event_id: ev.id.to_string(),
        ts: format_ts(&ev.ts),
        text,
        kind: p.kind.as_str().to_string(),
        tags: p.tags.clone(),
        source_app: ev.source.app.clone(),
    })
}

fn collect_forgotten_capture_ids(log: &EventLog) -> Result<HashSet<EventId>> {
    let mut out = HashSet::new();
    for ev in log.iter()? {
        let ev = ev?;
        if let EventKind::Forget(ForgetPayload { target_id, .. }) = ev.kind {
            out.insert(target_id);
        }
    }
    Ok(out)
}

fn format_ts(ts: &DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn write_output<W: Write>(out: &mut W, rows: &[RecentCapture], as_json: bool) -> Result<()> {
    if as_json {
        let payload = JsonOutput {
            ok: true,
            captures: rows,
        };
        serde_json::to_writer(&mut *out, &payload).context("serialize recent JSON")?;
        out.write_all(b"\n").context("write JSON newline")?;
    } else if rows.is_empty() {
        writeln!(out, "no captures yet").context("write empty recent line")?;
    } else {
        for row in rows {
            // Trim text for readability in the CLI. Full text stays in
            // the JSON path so machine consumers see it intact.
            let snippet = trim_for_display(&row.text);
            writeln!(out, "{}  {}  {}", row.ts, row.event_id, snippet)
                .context("write recent row")?;
        }
    }
    Ok(())
}

/// Single-line snippet for the human-readable listing. Replaces
/// newlines (multi-paragraph captures otherwise span lines) and
/// truncates to a generous width. Width is generous (160) because
/// the MCP Resources path is the polished surface; the CLI is the
/// audit surface.
fn trim_for_display(text: &str) -> String {
    const MAX: usize = 160;
    let single_line: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if single_line.chars().count() <= MAX {
        return single_line;
    }
    let truncated: String = single_line.chars().take(MAX).collect();
    format!("{truncated}…")
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
    use crate::event::{CapturePayload, Source};
    use serde_json::{Map, Value};
    use tempfile::tempdir;

    fn capture(text: &str) -> Event {
        Event::new(
            EventKind::Capture(CapturePayload {
                text: text.into(),
                rewritten_text: None,
                kind: Default::default(),
                mime: None,
                attachments: vec![],
                tags: Default::default(),
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        )
    }

    #[test]
    fn newest_first_ordering() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        log.append(&capture("first")).unwrap();
        log.append(&capture("second")).unwrap();
        log.append(&capture("third")).unwrap();
        let rows = load_recent(&log, 5).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "third");
        assert_eq!(rows[1].text, "second");
        assert_eq!(rows[2].text, "first");
    }

    #[test]
    fn limit_caps_the_window() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        for i in 0..50 {
            log.append(&capture(&format!("msg{i}"))).unwrap();
        }
        let rows = load_recent(&log, 5).unwrap();
        assert_eq!(rows.len(), 5);
        // Newest is msg49, oldest in window is msg45.
        assert_eq!(rows[0].text, "msg49");
        assert_eq!(rows[4].text, "msg45");
    }

    #[test]
    fn limit_zero_returns_empty() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        log.append(&capture("a")).unwrap();
        let rows = load_recent(&log, 0).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn non_capture_events_are_excluded() {
        // facts and forgets must not appear in the recent listing.
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        log.append(&capture("real")).unwrap();
        let fact = Event::new(
            EventKind::Fact(crate::event::FactPayload {
                subject: "x".into(),
                predicate: "is".into(),
                object: "y".into(),
                confidence: 0.9,
                valid_from: Utc::now(),
                valid_to: None,
                derived_from: vec![],
                kind: Default::default(),
                tags: Default::default(),
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        );
        log.append(&fact).unwrap();
        let rows = load_recent(&log, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "real");
    }

    #[test]
    fn forgotten_captures_drop_from_listing() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        let cap = capture("erase me");
        let cap_id = cap.id;
        log.append(&cap).unwrap();
        log.append(&capture("keep me")).unwrap();
        let forget = Event::new(
            EventKind::Forget(ForgetPayload {
                target_id: cap_id,
                reason: "test".into(),
                scope: None,
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        );
        log.append(&forget).unwrap();
        let rows = load_recent(&log, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "keep me");
    }

    #[test]
    fn rewritten_text_surfaces_when_present() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        let mut ev = capture("they prefer rust");
        if let EventKind::Capture(ref mut p) = ev.kind {
            p.rewritten_text = Some("Vijay prefers rust".into());
        }
        log.append(&ev).unwrap();
        let rows = load_recent(&log, 5).unwrap();
        assert_eq!(rows[0].text, "Vijay prefers rust");
    }

    #[test]
    fn json_output_shape() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        log.append(&capture("hello")).unwrap();
        let rows = load_recent(&log, 5).unwrap();
        let mut buf = Vec::new();
        write_output(&mut buf, &rows, true).unwrap();
        let json: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(json["ok"], true);
        let arr = json["captures"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], "hello");
        assert!(!arr[0]["event_id"].as_str().unwrap().is_empty());
        assert!(arr[0]["ts"].as_str().unwrap().contains('T'));
    }

    #[test]
    fn trim_for_display_handles_newlines_and_truncation() {
        let s = trim_for_display("line one\nline two");
        assert_eq!(s, "line one line two");
        let long: String = "x".repeat(200);
        let trimmed = trim_for_display(&long);
        // 160 x's plus an ellipsis.
        assert!(trimmed.ends_with('…'));
        assert_eq!(trimmed.chars().count(), 161);
    }
}
