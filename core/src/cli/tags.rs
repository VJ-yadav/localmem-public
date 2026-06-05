//! `localmem tags` handler (T-53).
//!
//! Aggregates container tags across every committed capture. Walks
//! `events.jsonl` once, counts each `key=value` pair, and emits the
//! result sorted by count desc then key asc then value asc for
//! determinism. The event log is the source of truth (see
//! ARCHITECTURE.md "event log is the source of truth"); deriving tag
//! counts from a derived store would risk drift on stale rebuilds.
//!
//! Pre-T-25 the lex meta index could short-circuit this, but the
//! event log walk is O(N) over events anyway and stays simple to
//! reason about. If this becomes hot we can add a derived `tags`
//! table later; v0.2 first cut keeps it computed on demand.

use crate::event::{EventKind, ForgetPayload};
use crate::event_id::EventId;
use crate::event_log::EventLog;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
struct TagRow {
    key: String,
    value: String,
    count: u64,
}

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    ok: bool,
    tags: &'a [TagRow],
}

pub fn run(home: Option<&str>, as_json: bool) -> Result<()> {
    let home = resolve_home(home)?;
    let log = EventLog::open(&home).context("open event log")?;
    let rows = aggregate_tags(&log)?;
    let mut out = io::stdout().lock();
    write_output(&mut out, &rows, as_json)
}

/// Walk the event log and count `key=value` tag pairs across all
/// captures that have NOT been forgotten. Tags inherited onto facts
/// are ignored: the aggregation is "what tags are on captures the
/// user wrote", which mirrors how `localmem write --tags ...` lands
/// on disk.
///
/// A `Forget` event for a capture id removes that capture's
/// contribution to the totals. We resolve this in a single pass by
/// collecting forgotten ids first (rare), then re-iterating and
/// skipping. Two-pass is fine: events.jsonl iteration is fast and
/// this is a CLI discovery command, not a hot retrieval path.
fn aggregate_tags(log: &EventLog) -> Result<Vec<TagRow>> {
    let forgotten = collect_forgotten_capture_ids(log)?;
    let mut counts: BTreeMap<(String, String), u64> = BTreeMap::new();
    for ev in log.iter()? {
        let ev = ev?;
        if let EventKind::Capture(p) = &ev.kind {
            if forgotten.contains(&ev.id) {
                continue;
            }
            for (k, v) in &p.tags {
                *counts.entry((k.clone(), v.clone())).or_default() += 1;
            }
        }
    }
    let mut rows: Vec<TagRow> = counts
        .into_iter()
        .map(|((k, v), c)| TagRow {
            key: k,
            value: v,
            count: c,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.value.cmp(&b.value))
    });
    Ok(rows)
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

fn write_output<W: Write>(out: &mut W, rows: &[TagRow], as_json: bool) -> Result<()> {
    if as_json {
        let payload = JsonOutput { ok: true, tags: rows };
        serde_json::to_writer(&mut *out, &payload).context("serialize tags JSON")?;
        out.write_all(b"\n").context("write JSON newline")?;
    } else if rows.is_empty() {
        writeln!(out, "no tags yet").context("write empty tags line")?;
    } else {
        for row in rows {
            writeln!(out, "{}\t{}={}", row.count, row.key, row.value)
                .context("write tags row")?;
        }
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
    use crate::event::{CapturePayload, Event, Source};
    use serde_json::{Map, Value};
    use tempfile::tempdir;

    fn capture_with_tags(text: &str, pairs: &[(&str, &str)]) -> Event {
        let mut tags = BTreeMap::new();
        for (k, v) in pairs {
            tags.insert((*k).to_string(), (*v).to_string());
        }
        Event::new(
            EventKind::Capture(CapturePayload {
                text: text.into(),
                rewritten_text: None,
                kind: Default::default(),
                mime: None,
                attachments: vec![],
                tags,
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
    fn aggregate_counts_pairs_across_captures() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        log.append(&capture_with_tags("a", &[("project", "localmem")]))
            .unwrap();
        log.append(&capture_with_tags("b", &[("project", "localmem")]))
            .unwrap();
        log.append(&capture_with_tags(
            "c",
            &[("project", "other"), ("topic", "tags")],
        ))
        .unwrap();
        let rows = aggregate_tags(&log).unwrap();
        // project=localmem leads (count 2), then ties (count 1) by key/value asc.
        assert_eq!(rows[0].key, "project");
        assert_eq!(rows[0].value, "localmem");
        assert_eq!(rows[0].count, 2);
        // Remaining: project=other and topic=tags both at count 1.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].count, 1);
        assert_eq!(rows[2].count, 1);
    }

    #[test]
    fn forgotten_captures_drop_from_totals() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        let cap = capture_with_tags("a", &[("project", "localmem")]);
        let cap_id = cap.id;
        log.append(&cap).unwrap();
        // Emit a Forget for the capture id; it must drop from totals.
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
        let rows = aggregate_tags(&log).unwrap();
        assert!(rows.is_empty(), "forgotten capture must drop from tag totals");
    }

    #[test]
    fn untagged_captures_contribute_nothing() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        log.append(&capture_with_tags("a", &[])).unwrap();
        let rows = aggregate_tags(&log).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn human_output_emits_count_tab_key_value() {
        let rows = vec![TagRow {
            key: "project".into(),
            value: "localmem".into(),
            count: 3,
        }];
        let mut buf = Vec::new();
        write_output(&mut buf, &rows, false).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("3\tproject=localmem"));
    }

    #[test]
    fn json_output_shape_is_stable() {
        let rows = vec![TagRow {
            key: "project".into(),
            value: "localmem".into(),
            count: 2,
        }];
        let mut buf = Vec::new();
        write_output(&mut buf, &rows, true).unwrap();
        let json: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(json["ok"], true);
        let arr = json["tags"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["key"], "project");
        assert_eq!(arr[0]["value"], "localmem");
        assert_eq!(arr[0]["count"], 2);
    }
}
