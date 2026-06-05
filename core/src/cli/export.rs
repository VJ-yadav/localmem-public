//! `localmem export` and `localmem import` handlers.
//!
//! Single-file portable archive format. See SPEC.md "localmem export"
//! and TASKS.md T-42.
//!
//! Format (JSONL, one object per line):
//!
//! ```text
//! {"archive_version":1,"events_count":N,"exported_at":"...","localmem_version":"..."}
//! <event 1>
//! <event 2>
//! ...
//! ```
//!
//! Why single-file JSONL and not tar.gz: zero new dependencies, parseable
//! with any line-by-line tool, and `facts.duckdb` / `journal.log` are
//! recomputable from `events.jsonl` per ARCHITECTURE.md invariant 2, so
//! shipping only events is enough for "identical memory state" after
//! `localmem replay`. Tar+gzip stays available as a v0.2 polish (one
//! more dependency, materially smaller archives for large logs).

use crate::event::Event;
use crate::event_log::EventLog;
use anyhow::{bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

const ARCHIVE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct ArchiveHeader {
    archive_version: u32,
    events_count: u64,
    exported_at: String,
    localmem_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportOutput {
    pub path: String,
    pub events_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportOutput {
    pub events_count: u64,
}

/// Entry point for `export`.
pub fn run_export(home: Option<&str>, dest: &str, as_json: bool) -> Result<()> {
    let home = resolve_home(home)?;
    let out = export_to(&home, Path::new(dest))?;
    if as_json {
        let json = serde_json::json!({
            "ok": true,
            "path": out.path,
            "events_count": out.events_count,
        });
        println!("{json}");
    } else {
        println!("exported {} events to {}", out.events_count, out.path);
    }
    Ok(())
}

/// Entry point for `import`. Dispatches on `format`:
/// - `archive`: portable localmem export (this module).
/// - `chatgpt`: ChatGPT data export (`conversations.json`).
/// - `claude` : Claude/Anthropic data export (`conversations.json`).
///
/// Vendor imports append raw events to `events.jsonl`; they do NOT
/// rebuild derived stores. Run `localmem replay` afterward (the CLI
/// prints a reminder).
pub fn run_import(home: Option<&str>, format: &str, src: &str, as_json: bool) -> Result<()> {
    let home = resolve_home(home)?;
    match format {
        "archive" => {
            let out = import_from(&home, Path::new(src))?;
            if as_json {
                let json = serde_json::json!({
                    "ok": true,
                    "format": "archive",
                    "events_count": out.events_count,
                });
                println!("{json}");
            } else {
                println!(
                    "imported {} events into {}",
                    out.events_count,
                    home.display()
                );
            }
        }
        "chatgpt" => {
            let stats = crate::import::chatgpt::import_chatgpt(&home, Path::new(src))?;
            emit_vendor_stats(&stats, &home, as_json)?;
        }
        "claude" => {
            let stats = crate::import::claude::import_claude(&home, Path::new(src))?;
            emit_vendor_stats(&stats, &home, as_json)?;
        }
        other => bail!(
            "unsupported import format `{other}`. Supported in v0.1: \
             archive, chatgpt, claude."
        ),
    }
    Ok(())
}

fn emit_vendor_stats(
    stats: &crate::import::ImportStats,
    home: &std::path::Path,
    as_json: bool,
) -> Result<()> {
    if as_json {
        let json = serde_json::to_value(stats).context("serialize import stats")?;
        let mut wrapped = serde_json::json!({ "ok": true });
        if let (Some(map), Some(stats_obj)) = (wrapped.as_object_mut(), json.as_object()) {
            for (k, v) in stats_obj {
                map.insert(k.clone(), v.clone());
            }
        }
        println!("{wrapped}");
    } else {
        println!(
            "imported {} ({} conversations, {} messages skipped) into {}",
            stats.events_appended,
            stats.conversations_seen,
            stats.messages_skipped,
            home.display()
        );
        println!("Next: run `localmem replay` to rebuild derived stores.");
    }
    Ok(())
}

/// Write a portable archive to `dest`. Existing file is overwritten.
pub fn export_to(home: &Path, dest: &Path) -> Result<ExportOutput> {
    let event_log = EventLog::open(home).context("open event log for export")?;
    let events: Vec<Event> = event_log
        .iter()
        .context("open event log iterator")?
        .collect::<Result<Vec<_>>>()
        .context("read events from log")?;

    let header = ArchiveHeader {
        archive_version: ARCHIVE_VERSION,
        events_count: events.len() as u64,
        exported_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        localmem_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir {}", parent.display()))?;
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dest)
        .with_context(|| format!("create archive at {}", dest.display()))?;
    let mut writer = BufWriter::new(file);
    writeln!(
        writer,
        "{}",
        serde_json::to_string(&header).context("serialize archive header")?
    )
    .context("write archive header")?;
    for event in &events {
        writeln!(
            writer,
            "{}",
            serde_json::to_string(event).context("serialize event for archive")?
        )
        .context("write event line")?;
    }
    writer.flush().context("flush archive writer")?;

    Ok(ExportOutput {
        path: dest.display().to_string(),
        events_count: events.len() as u64,
    })
}

/// Restore an archive into `home`. Refuses to clobber an existing
/// non-empty `events.jsonl` so a misfired import never overwrites a
/// real memory directory. The caller is expected to `init` a fresh
/// home first.
pub fn import_from(home: &Path, src: &Path) -> Result<ImportOutput> {
    let file = File::open(src).with_context(|| format!("open archive at {}", src.display()))?;
    let mut reader = BufReader::new(file).lines();

    let header_line = reader
        .next()
        .ok_or_else(|| anyhow::anyhow!("archive is empty"))?
        .context("read archive header line")?;
    let header: ArchiveHeader =
        serde_json::from_str(&header_line).context("parse archive header as JSON")?;
    if header.archive_version != ARCHIVE_VERSION {
        bail!(
            "unsupported archive_version {} (this binary handles {})",
            header.archive_version,
            ARCHIVE_VERSION
        );
    }

    let events_path = home.join(crate::event_log::EVENTS_FILE);
    let already = events_path.metadata().map(|m| m.len() > 0).unwrap_or(false);
    if already {
        bail!(
            "{} is not empty; refusing to clobber. Run `localmem init` to a fresh home first.",
            events_path.display()
        );
    }

    std::fs::create_dir_all(home)
        .with_context(|| format!("create home dir at {}", home.display()))?;
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&events_path)
        .with_context(|| format!("open events.jsonl at {}", events_path.display()))?;
    let mut writer = BufWriter::new(out);

    let mut count = 0u64;
    for line in reader {
        let line = line.context("read archive event line")?;
        if line.trim().is_empty() {
            continue;
        }
        // Validate by deserializing; a malformed archive surfaces here
        // rather than later at replay time.
        let _: Event = serde_json::from_str(&line)
            .with_context(|| format!("parse event line {} from archive", count + 2))?;
        writer
            .write_all(line.as_bytes())
            .context("write event line to events.jsonl")?;
        writer.write_all(b"\n").context("write event terminator")?;
        count += 1;
    }
    writer.flush().context("flush events.jsonl writer")?;

    if count != header.events_count {
        bail!(
            "archive header declared {} events but {} were present",
            header.events_count,
            count
        );
    }

    Ok(ImportOutput {
        events_count: count,
    })
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
    use crate::event::{CapturePayload, EventKind, Source};
    use serde_json::Map;
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
                host: "h".into(),
                user: None,
            },
        )
    }

    #[test]
    fn export_then_import_round_trips_events() {
        let src_home = tempdir().unwrap();
        init_home(src_home.path()).unwrap();
        let log = EventLog::open(src_home.path()).unwrap();
        let a = capture("first event for export");
        let b = capture("second event for export");
        log.append(&a).unwrap();
        log.append(&b).unwrap();
        drop(log);

        let archive_tmp = tempdir().unwrap();
        let archive_path = archive_tmp.path().join("archive.jsonl");
        let export_out = export_to(src_home.path(), &archive_path).unwrap();
        assert_eq!(export_out.events_count, 2);
        assert!(archive_path.exists());

        let dst_home = tempdir().unwrap();
        init_home(dst_home.path()).unwrap();
        let import_out = import_from(dst_home.path(), &archive_path).unwrap();
        assert_eq!(import_out.events_count, 2);

        let dst_log = EventLog::open(dst_home.path()).unwrap();
        let events: Vec<Event> = dst_log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], a);
        assert_eq!(events[1], b);
    }

    #[test]
    fn import_refuses_to_clobber_non_empty_log() {
        let src = tempdir().unwrap();
        init_home(src.path()).unwrap();
        let log = EventLog::open(src.path()).unwrap();
        log.append(&capture("x")).unwrap();
        drop(log);
        let archive = src.path().join("a.jsonl");
        export_to(src.path(), &archive).unwrap();

        let dst = tempdir().unwrap();
        init_home(dst.path()).unwrap();
        // Plant a non-empty events.jsonl in the destination.
        let dst_log = EventLog::open(dst.path()).unwrap();
        dst_log.append(&capture("already here")).unwrap();
        drop(dst_log);

        let err = import_from(dst.path(), &archive).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not empty"),
            "expected clobber error, got: {msg}"
        );
    }

    #[test]
    fn import_rejects_unknown_archive_version() {
        let dst = tempdir().unwrap();
        init_home(dst.path()).unwrap();
        // Hand-write an archive with version=999.
        let archive = dst.path().join("bad.jsonl");
        let header = serde_json::json!({
            "archive_version": 999u32,
            "events_count": 0u64,
            "exported_at": "2026-05-15T00:00:00.000Z",
            "localmem_version": "future",
        });
        std::fs::write(&archive, format!("{header}\n")).unwrap();
        let err = import_from(dst.path(), &archive).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("archive_version"), "got: {msg}");
    }

    #[test]
    fn import_detects_count_mismatch() {
        let dst = tempdir().unwrap();
        init_home(dst.path()).unwrap();
        // Header says 5 events but the body has 0.
        let archive = dst.path().join("count-mismatch.jsonl");
        let header = serde_json::json!({
            "archive_version": ARCHIVE_VERSION,
            "events_count": 5u64,
            "exported_at": "2026-05-15T00:00:00.000Z",
            "localmem_version": "0.0.1",
        });
        std::fs::write(&archive, format!("{header}\n")).unwrap();
        let err = import_from(dst.path(), &archive).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("declared 5"),
            "expected mismatch error, got: {msg}"
        );
    }
}
