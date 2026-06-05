//! `localmem subjects` handler (T-53).
//!
//! Lists distinct entity subjects with fact counts. The audit view: counts
//! include retired rows so the surface answers "what entities have we ever
//! seen?" rather than "what's live right now?" — keeping it stable across
//! smart-forgetting churn.

use crate::facts::FactsStore;
use anyhow::{Context, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
struct SubjectRow {
    subject: String,
    count: u64,
}

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    ok: bool,
    subjects: &'a [SubjectRow],
}

pub fn run(home: Option<&str>, as_json: bool) -> Result<()> {
    let home = resolve_home(home)?;
    let store = FactsStore::open(&home).context("open facts store")?;
    let rows = store.subjects().context("list subjects")?;
    let rows: Vec<SubjectRow> = rows
        .into_iter()
        .map(|(subject, count)| SubjectRow { subject, count })
        .collect();
    let mut out = io::stdout().lock();
    write_output(&mut out, &rows, as_json)
}

fn write_output<W: Write>(out: &mut W, rows: &[SubjectRow], as_json: bool) -> Result<()> {
    if as_json {
        let payload = JsonOutput {
            ok: true,
            subjects: rows,
        };
        serde_json::to_writer(&mut *out, &payload).context("serialize subjects JSON")?;
        out.write_all(b"\n").context("write JSON newline")?;
    } else if rows.is_empty() {
        writeln!(out, "no subjects yet").context("write empty subjects line")?;
    } else {
        for row in rows {
            writeln!(out, "{}\t{}", row.count, row.subject)
                .context("write subjects row")?;
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
    use crate::cli::init::init_home;
    use crate::event_id::EventId;
    use crate::facts::Fact;
    use chrono::{DateTime, Utc};
    use serde_json::Value;
    use tempfile::tempdir;

    fn ts(epoch: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(epoch, 0).unwrap()
    }

    fn sample(subject: &str) -> Fact {
        Fact {
            id: EventId::new(),
            subject: subject.into(),
            predicate: "is".into(),
            object: "x".into(),
            confidence: 0.8,
            valid_from: ts(1_700_000_000),
            valid_to: None,
            recorded_at: ts(1_700_000_000),
            retired_at: None,
            source_events: vec![],
            policy_id: None,
            tags: Default::default(),
            kind: Default::default(),
        }
    }

    #[test]
    fn human_output_is_one_line_per_subject() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        store.insert(&sample("user")).unwrap();
        store.insert(&sample("user")).unwrap();
        store.insert(&sample("alice")).unwrap();
        let rows: Vec<SubjectRow> = store
            .subjects()
            .unwrap()
            .into_iter()
            .map(|(s, c)| SubjectRow {
                subject: s,
                count: c,
            })
            .collect();
        let mut buf = Vec::new();
        write_output(&mut buf, &rows, false).unwrap();
        let text = String::from_utf8(buf).unwrap();
        // user (count 2) appears before alice (count 1).
        let user_pos = text.find("user").unwrap();
        let alice_pos = text.find("alice").unwrap();
        assert!(user_pos < alice_pos);
        assert!(text.contains("2\tuser"));
        assert!(text.contains("1\talice"));
    }

    #[test]
    fn json_output_shape_is_stable() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        store.insert(&sample("user")).unwrap();
        let rows: Vec<SubjectRow> = store
            .subjects()
            .unwrap()
            .into_iter()
            .map(|(s, c)| SubjectRow {
                subject: s,
                count: c,
            })
            .collect();
        let mut buf = Vec::new();
        write_output(&mut buf, &rows, true).unwrap();
        let json: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(json["ok"], true);
        let arr = json["subjects"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["subject"], "user");
        assert_eq!(arr[0]["count"], 1);
    }

    #[test]
    fn empty_store_emits_friendly_message_in_human_mode() {
        let mut buf = Vec::new();
        write_output(&mut buf, &[], false).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("no subjects yet"));
    }
}
