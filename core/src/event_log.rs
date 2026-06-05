//! Append-only event log.
//!
//! Per ARCHITECTURE.md, `events.jsonl` is the source of truth for all
//! derived stores. It is:
//! - Append-only (no in-place edits, ever)
//! - One JSON object per line
//! - Crash-safe: each append is durable before the call returns
//!
//! Concurrent appenders from multiple threads in the same process are
//! serialized via an internal `Mutex`. Multi-process appenders are NOT yet
//! coordinated (v0.1 ships a single-process daemon model). A future task
//! can add `flock` for cross-process safety without changing the public API.

use crate::event::Event;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const EVENTS_FILE: &str = "events.jsonl";

pub struct EventLog {
    path: PathBuf,
    writer: Mutex<BufWriter<File>>,
}

impl EventLog {
    /// Open (or create) the event log under `home`. On-disk path is
    /// `<home>/events.jsonl`.
    pub fn open(home: impl AsRef<Path>) -> Result<Self> {
        let home = home.as_ref();
        std::fs::create_dir_all(home)
            .with_context(|| format!("create localmem home at {}", home.display()))?;
        let path = home.join(EVENTS_FILE);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open event log at {}", path.display()))?;
        Ok(Self {
            path,
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event. Durable on return.
    ///
    /// The on-wire format is `<json>\n`. The JSON encoder never emits a raw
    /// newline inside the line (string newlines are escaped as `\n`), so the
    /// JSONL one-event-per-line invariant holds.
    pub fn append(&self, event: &Event) -> Result<()> {
        let line = serde_json::to_string(event).context("serialize event for append")?;
        debug_assert!(
            !line.contains('\n'),
            "event line must not contain a raw newline"
        );

        // If the mutex was poisoned (a previous appender panicked), we
        // recover the inner writer. The on-disk state is still consistent
        // because every successful append already flushed + fsynced.
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        writer
            .write_all(line.as_bytes())
            .context("write event line")?;
        writer.write_all(b"\n").context("write line terminator")?;
        writer.flush().context("flush BufWriter")?;
        fsync_durable(writer.get_ref())?;
        Ok(())
    }

    /// Stream events from disk. Returns the current file state at call time;
    /// later appends are not visible to an existing iterator.
    pub fn iter(&self) -> Result<impl Iterator<Item = Result<Event>>> {
        let file = File::open(&self.path)
            .with_context(|| format!("open event log for read: {}", self.path.display()))?;
        let reader = BufReader::new(file);
        Ok(reader.lines().enumerate().map(|(idx, line_result)| {
            let line = line_result.with_context(|| format!("read event log line {}", idx + 1))?;
            let event: Event = serde_json::from_str(&line)
                .with_context(|| format!("parse event log line {}", idx + 1))?;
            Ok(event)
        }))
    }
}

#[cfg(target_os = "macos")]
fn fsync_durable(file: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    // F_FULLFSYNC forces the disk to flush its internal write cache. This
    // is stronger than regular fsync (which only ensures the OS page cache
    // is flushed to the disk's cache) and is what SQLite uses on macOS for
    // true crash-on-power-loss durability.
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
    if ret == -1 {
        return Err(std::io::Error::last_os_error()).context("F_FULLFSYNC");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn fsync_durable(file: &File) -> Result<()> {
    // fdatasync (no metadata sync). Per ARCHITECTURE.md: events.jsonl is
    // append-only and metadata stability is not load-bearing for replay.
    file.sync_data().context("fsync_data")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CapturePayload, EventKind, Source};
    use serde_json::Map;
    use std::sync::Arc;
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
    fn append_then_read_roundtrips() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();

        let ev1 = capture("hello");
        let ev2 = capture("world");
        log.append(&ev1).unwrap();
        log.append(&ev2).unwrap();

        let events: Vec<Event> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], ev1);
        assert_eq!(events[1], ev2);
    }

    #[test]
    fn empty_log_iter_is_empty() {
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        assert_eq!(log.iter().unwrap().count(), 0);
    }

    #[test]
    fn events_persist_across_reopen() {
        let tmp = tempdir().unwrap();
        let ev = capture("persistent");
        {
            let log = EventLog::open(tmp.path()).unwrap();
            log.append(&ev).unwrap();
            // log dropped here; on-disk state is durable because every
            // append already fsynced.
        }
        let log2 = EventLog::open(tmp.path()).unwrap();
        let events: Vec<Event> = log2.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], ev);
    }

    #[test]
    fn concurrent_appends_dont_interleave() {
        // Multiple threads hammering the same log. If our mutex + atomic
        // O_APPEND discipline fails, two writes could interleave on disk
        // and produce a corrupted (unparseable) JSON line.
        let tmp = tempdir().unwrap();
        let log = Arc::new(EventLog::open(tmp.path()).unwrap());

        const THREADS: usize = 8;
        const PER_THREAD: usize = 50;

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let ev = capture(&format!("thread {t} write {i}"));
                        log.append(&ev).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        // Every line must parse. Interleaving would cause parse errors.
        let events: Vec<Event> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(events.len(), THREADS * PER_THREAD);
    }

    #[test]
    fn malformed_line_returns_error_without_skipping() {
        // Simulate a corrupted log: write a garbage line directly.
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join(EVENTS_FILE), b"this is not json\n").unwrap();

        let log = EventLog::open(tmp.path()).unwrap();
        let mut iter = log.iter().unwrap();
        let first = iter.next().expect("expected one item");
        assert!(
            first.is_err(),
            "malformed line must yield Err, got {first:?}"
        );
    }

    #[test]
    fn append_with_embedded_newline_in_content_still_one_line() {
        // The serialized JSON for a capture whose text contains newlines
        // must remain a single line on disk (newlines escaped as \n).
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        let ev = capture("multi\nline\ntext");
        log.append(&ev).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join(EVENTS_FILE)).unwrap();
        // Exactly one trailing newline (the line terminator), no internal ones.
        assert_eq!(raw.matches('\n').count(), 1);

        let events: Vec<Event> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], ev);
    }

    #[test]
    fn append_throughput_sanity() {
        // Not a benchmark, just a smoke test that F_FULLFSYNC isn't
        // catastrophically slow. The actual benchmark goal (10k/sec on
        // M-series) lives in a future criterion-based bench task.
        let tmp = tempdir().unwrap();
        let log = EventLog::open(tmp.path()).unwrap();
        let ev = capture("throughput");

        const N: usize = 200;
        let start = std::time::Instant::now();
        for _ in 0..N {
            log.append(&ev).unwrap();
        }
        let elapsed = start.elapsed();

        // Generous bound to tolerate slow CI disks. We mainly want to fail
        // if something is wrong (e.g. accidentally re-opening the file per
        // write, or fsync taking seconds).
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "{N} appends took {elapsed:?}, above sanity threshold"
        );
    }
}
