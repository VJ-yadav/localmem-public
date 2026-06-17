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
pub fn run_import(
    home: Option<&str>,
    src: &str,
    format: Option<&str>,
    dry_run: bool,
    as_json: bool,
) -> Result<()> {
    let home = resolve_home(home)?;
    let src_path = Path::new(src);

    // Auto-detect the format from the file contents unless the user forced one.
    // Keeps the common case a one-liner: `localmem import <path>`.
    let fmt = match format {
        Some(f) => f.to_string(),
        None => crate::import::detect_format(src_path)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "could not auto-detect the import format for {src}; \
                     pass --format chatgpt|claude|claude-code|archive"
                )
            })?
            .to_string(),
    };

    // --dry-run: parse + report what WOULD happen, without writing. This is the
    // engine behind a guided import wizard in the dashboard.
    if dry_run {
        match fmt.as_str() {
            "chatgpt" => {
                let parsed = crate::import::chatgpt::parse_chatgpt(src_path)?;
                emit_preview(
                    &crate::import::preview_import(&home, "chatgpt", &parsed)?,
                    as_json,
                );
            }
            "claude" => {
                let parsed = crate::import::claude::parse_claude(src_path)?;
                emit_preview(
                    &crate::import::preview_import(&home, "claude", &parsed)?,
                    as_json,
                );
            }
            "claude-code" => {
                let parsed = crate::import::claude_code::parse_claude_code(src_path)?;
                emit_preview(
                    &crate::import::preview_import(&home, "claude-code", &parsed)?,
                    as_json,
                );
            }
            "archive" => {
                let n = count_archive_events(src_path)?;
                if as_json {
                    println!(
                        "{}",
                        serde_json::json!({"ok": true, "dry_run": true, "format": "archive", "events": n})
                    );
                } else {
                    println!("dry-run: archive `{src}` contains {n} events.");
                }
            }
            other => {
                bail!("unsupported import format `{other}` (expected chatgpt, claude, claude-code, archive)")
            }
        }
        return Ok(());
    }

    match fmt.as_str() {
        "archive" => {
            let out = import_from(&home, src_path)?;
            if as_json {
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "format": "archive", "events_count": out.events_count})
                );
            } else {
                println!(
                    "imported {} events into {}",
                    out.events_count,
                    home.display()
                );
            }
        }
        "chatgpt" => {
            let stats = crate::import::chatgpt::import_chatgpt(&home, src_path)?;
            emit_vendor_stats(&stats, &home, as_json)?;
        }
        "claude" => {
            let stats = crate::import::claude::import_claude(&home, src_path)?;
            emit_vendor_stats(&stats, &home, as_json)?;
        }
        "claude-code" => {
            let stats = crate::import::claude_code::import_claude_code(&home, src_path)?;
            emit_vendor_stats(&stats, &home, as_json)?;
        }
        other => bail!(
            "unsupported import format `{other}` (expected chatgpt, claude, claude-code, archive)"
        ),
    }
    Ok(())
}

/// Human/JSON output for a `--dry-run` import preview.
fn emit_preview(pv: &crate::import::PreviewStats, as_json: bool) {
    if as_json {
        if let Ok(v) = serde_json::to_value(pv) {
            let mut wrapped = serde_json::json!({"ok": true, "dry_run": true});
            if let (Some(m), Some(o)) = (wrapped.as_object_mut(), v.as_object()) {
                for (k, val) in o {
                    m.insert(k.clone(), val.clone());
                }
            }
            println!("{wrapped}");
        }
    } else {
        println!(
            "dry-run: {} export — {} messages across {} conversations",
            pv.format, pv.total_messages, pv.conversations_seen
        );
        println!(
            "  {} new, {} already imported, {} skipped while parsing",
            pv.new_messages, pv.already_imported, pv.messages_skipped
        );
        println!("Run the same command without --dry-run to import.");
    }
}

/// Count events in a localmem archive without importing (for `--dry-run`).
/// Uses the header's `events_count` when present, else counts non-empty lines.
fn count_archive_events(path: &Path) -> Result<u64> {
    let f = File::open(path).with_context(|| format!("open archive {}", path.display()))?;
    let mut lines = BufReader::new(f).lines();
    let Some(first) = lines.next() else {
        return Ok(0);
    };
    let first = first.context("read first archive line")?;
    if let Ok(h) = serde_json::from_str::<ArchiveHeader>(&first) {
        return Ok(h.events_count);
    }
    // No header (a raw events.jsonl): count this line + the rest.
    let mut n = if first.trim().is_empty() { 0 } else { 1 };
    for line in lines {
        if !line.context("read archive line")?.trim().is_empty() {
            n += 1;
        }
    }
    Ok(n)
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
            "imported {} ({} conversations, {} skipped, {} already imported) into {}",
            stats.events_appended,
            stats.conversations_seen,
            stats.messages_skipped,
            stats.messages_deduped,
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
                time: None,
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
