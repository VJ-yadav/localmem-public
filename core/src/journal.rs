//! Append-only journal of policy decisions.
//!
//! One line per `Policy::evaluate()` call. Format defined in ARCHITECTURE.md
//! (Derived stores -> `journal.log`):
//!
//! ```text
//! ts=2026-05-14T12:34:57.000Z input=01HXYZ... action=COMMIT rule=high_signal reasoning="..."
//! ```
//!
//! Fields appear in a stable order (`ts`, `input`, `action`, `rule`, optional
//! `reasoning`). Reasoning is JSON-quoted so it can carry spaces, quotes, and
//! newlines without breaking the one-line-per-entry invariant. Implementation
//! task: T-17.

use crate::event::PolicyAction;
use crate::event_id::EventId;
use crate::policy::Decision;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// On-disk location relative to the localmem home.
pub const JOURNAL_FILE: &str = "derived/journal.log";

/// Append-only writer for policy decision entries. Mirrors the fsync
/// discipline of [`crate::event_log::EventLog`] so the journal can be
/// trusted as an audit trail even after a power loss mid-write.
pub struct Journal {
    path: PathBuf,
    writer: Mutex<BufWriter<File>>,
}

/// One row in `journal.log`. The on-wire format is the key=value line
/// defined in the module docstring; the in-memory representation uses
/// typed fields so callers do not have to parse strings.
///
/// The `Serialize` derive is used by the `localmem journal --json` CLI
/// (T-18) and by the future MCP `memory_journal` tool. The JSON shape
/// matches SPEC.md (action UPPERCASE, ts as RFC3339, reasoning optional).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JournalEntry {
    pub ts: DateTime<Utc>,
    pub action: PolicyAction,
    pub rule: String,
    pub input_id: EventId,
    pub reasoning: Option<String>,
}

impl JournalEntry {
    /// Build a journal entry from the policy decision for `input_id` at `ts`.
    ///
    /// An empty `decision.reasoning` becomes `None` so the line on disk omits
    /// the `reasoning=` field entirely (rather than writing `reasoning=""`).
    pub fn from_decision(decision: &Decision, input_id: EventId, ts: DateTime<Utc>) -> Self {
        let reasoning = if decision.reasoning.is_empty() {
            None
        } else {
            Some(decision.reasoning.clone())
        };
        Self {
            ts,
            action: decision.action,
            rule: decision.rule_id.clone(),
            input_id,
            reasoning,
        }
    }

    /// Serialize this entry to a single line (no trailing newline).
    pub fn format_line(&self) -> String {
        let ts = self.ts.to_rfc3339_opts(SecondsFormat::Millis, true);
        let action = action_label(self.action);
        let mut out = format!(
            "ts={ts} input={input} action={action} rule={rule}",
            input = self.input_id,
            rule = self.rule,
        );
        if let Some(reason) = &self.reasoning {
            let quoted = serde_json::to_string(reason).unwrap_or_else(|_| String::from("\"\""));
            out.push(' ');
            out.push_str("reasoning=");
            out.push_str(&quoted);
        }
        out
    }

    /// Parse one line in the format produced by [`Self::format_line`].
    pub fn parse_line(line: &str) -> Result<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        // `reasoning=` is the only field whose value can contain spaces.
        // Split it off first so the rest can be tokenized on whitespace.
        let (head, reasoning_part) = match line.find(" reasoning=") {
            Some(idx) => (&line[..idx], Some(&line[idx + " reasoning=".len()..])),
            None => (line, None),
        };

        let mut ts: Option<&str> = None;
        let mut input: Option<&str> = None;
        let mut action: Option<&str> = None;
        let mut rule: Option<&str> = None;

        for token in head.split_whitespace() {
            let (k, v) = token
                .split_once('=')
                .ok_or_else(|| anyhow!("malformed journal field: {token:?}"))?;
            match k {
                "ts" => ts = Some(v),
                "input" => input = Some(v),
                "action" => action = Some(v),
                "rule" => rule = Some(v),
                // Forward-compat: unknown fields are tolerated so a v0.2
                // binary's journal can be read by a v0.1 binary.
                _ => {}
            }
        }

        let ts: DateTime<Utc> = ts
            .ok_or_else(|| anyhow!("journal line missing `ts`: {line:?}"))?
            .parse::<DateTime<chrono::FixedOffset>>()
            .with_context(|| format!("parse ts in: {line:?}"))?
            .with_timezone(&Utc);
        let input_id: EventId = input
            .ok_or_else(|| anyhow!("journal line missing `input`: {line:?}"))?
            .parse()
            .with_context(|| format!("parse input id in: {line:?}"))?;
        let action_str =
            action.ok_or_else(|| anyhow!("journal line missing `action`: {line:?}"))?;
        let action = parse_action(action_str)
            .with_context(|| format!("parse action `{action_str}` in: {line:?}"))?;
        let rule = rule
            .ok_or_else(|| anyhow!("journal line missing `rule`: {line:?}"))?
            .to_string();
        let reasoning = match reasoning_part {
            Some(s) => Some(
                serde_json::from_str::<String>(s)
                    .with_context(|| format!("parse reasoning JSON in: {line:?}"))?,
            ),
            None => None,
        };

        Ok(Self {
            ts,
            action,
            rule,
            input_id,
            reasoning,
        })
    }
}

impl Journal {
    /// Open (or create) `<home>/derived/journal.log`. Creates any missing
    /// parent directories.
    pub fn open(home: impl AsRef<Path>) -> Result<Self> {
        let home = home.as_ref();
        let path = home.join(JOURNAL_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create journal dir at {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open journal at {}", path.display()))?;
        Ok(Self {
            path,
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Filesystem path of the journal file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry. Durable on return.
    pub fn append(&self, entry: &JournalEntry) -> Result<()> {
        let line = entry.format_line();
        debug_assert!(
            !line.contains('\n'),
            "journal line must not contain a raw newline; got {line:?}"
        );
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        writer
            .write_all(line.as_bytes())
            .context("write journal line")?;
        writer
            .write_all(b"\n")
            .context("write journal terminator")?;
        writer.flush().context("flush journal writer")?;
        fsync_durable(writer.get_ref())?;
        Ok(())
    }

    /// Stream entries from disk in append order. Snapshot at call time;
    /// later appends are not visible to an existing iterator (same semantics
    /// as [`crate::event_log::EventLog::iter`]).
    pub fn iter(&self) -> Result<impl Iterator<Item = Result<JournalEntry>>> {
        let file = File::open(&self.path)
            .with_context(|| format!("open journal for read: {}", self.path.display()))?;
        let reader = BufReader::new(file);
        Ok(reader.lines().enumerate().map(|(idx, line_result)| {
            let line = line_result.with_context(|| format!("read journal line {}", idx + 1))?;
            if line.trim().is_empty() {
                return Err(anyhow!("empty line at journal position {}", idx + 1));
            }
            JournalEntry::parse_line(&line)
                .with_context(|| format!("parse journal line {}", idx + 1))
        }))
    }
}

fn action_label(action: PolicyAction) -> &'static str {
    match action {
        PolicyAction::Commit => "COMMIT",
        PolicyAction::Update => "UPDATE",
        PolicyAction::Dedup => "DEDUP",
        PolicyAction::Skip => "SKIP",
        PolicyAction::Forget => "FORGET",
    }
}

fn parse_action(s: &str) -> Result<PolicyAction> {
    Ok(match s {
        "COMMIT" => PolicyAction::Commit,
        "UPDATE" => PolicyAction::Update,
        "DEDUP" => PolicyAction::Dedup,
        "SKIP" => PolicyAction::Skip,
        "FORGET" => PolicyAction::Forget,
        other => return Err(anyhow!("unknown policy action: {other}")),
    })
}

#[cfg(target_os = "macos")]
fn fsync_durable(file: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    // Same F_FULLFSYNC discipline as event_log: a journal entry must survive
    // a power loss, otherwise the audit trail is missing its load-bearing
    // property.
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
    if ret == -1 {
        return Err(std::io::Error::last_os_error()).context("F_FULLFSYNC");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn fsync_durable(file: &File) -> Result<()> {
    file.sync_data().context("fsync_data")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn fixed_id() -> EventId {
        "01HXY00000000000000000000Z".parse().unwrap()
    }

    fn fixed_ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 14, 12, 34, 56).unwrap()
    }

    fn sample_entry() -> JournalEntry {
        JournalEntry {
            ts: fixed_ts(),
            action: PolicyAction::Commit,
            rule: "high_signal".into(),
            input_id: fixed_id(),
            reasoning: Some("single declarative preference".into()),
        }
    }

    #[test]
    fn format_line_matches_expected_shape() {
        let line = sample_entry().format_line();
        // Field order is fixed.
        assert!(
            line.starts_with("ts="),
            "line should start with ts=, got: {line}"
        );
        assert!(line.contains(" input=01HXY00000000000000000000Z"));
        assert!(line.contains(" action=COMMIT "));
        assert!(line.contains(" rule=high_signal"));
        assert!(line.ends_with("reasoning=\"single declarative preference\""));
        // No raw newlines in the rendered form.
        assert!(!line.contains('\n'));
    }

    #[test]
    fn format_line_omits_reasoning_when_none() {
        let mut entry = sample_entry();
        entry.reasoning = None;
        let line = entry.format_line();
        assert!(
            !line.contains("reasoning="),
            "no-reasoning entry should not emit reasoning=, got: {line}"
        );
    }

    #[test]
    fn parse_line_round_trips_with_format_line() {
        let entry = sample_entry();
        let line = entry.format_line();
        let parsed = JournalEntry::parse_line(&line).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn parse_line_round_trips_without_reasoning() {
        let mut entry = sample_entry();
        entry.reasoning = None;
        let line = entry.format_line();
        let parsed = JournalEntry::parse_line(&line).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn parse_line_handles_quotes_and_spaces_in_reasoning() {
        let entry = JournalEntry {
            ts: fixed_ts(),
            action: PolicyAction::Skip,
            rule: "default_skip".into(),
            input_id: fixed_id(),
            reasoning: Some(r#"user said "ignore this" verbatim"#.into()),
        };
        let line = entry.format_line();
        // The escaped quotes appear in the on-wire form.
        assert!(
            line.contains("\\\""),
            "reasoning should be JSON-escaped: {line}"
        );
        // And parsing recovers the original string exactly.
        let parsed = JournalEntry::parse_line(&line).unwrap();
        assert_eq!(parsed.reasoning, entry.reasoning);
    }

    #[test]
    fn parse_line_tolerates_unknown_fields() {
        // A future schema may add fields. v0.1 must skip them rather than
        // erroring, so a v0.2-written journal stays readable.
        let line = format!(
            "ts={ts} input={id} action=COMMIT rule=high_signal new_field=foo reasoning=\"ok\"",
            ts = fixed_ts().to_rfc3339_opts(SecondsFormat::Millis, true),
            id = fixed_id(),
        );
        let parsed = JournalEntry::parse_line(&line).unwrap();
        assert_eq!(parsed.action, PolicyAction::Commit);
        assert_eq!(parsed.rule, "high_signal");
        assert_eq!(parsed.reasoning.as_deref(), Some("ok"));
    }

    #[test]
    fn parse_line_errors_on_missing_required_field() {
        // No ts.
        let line = "input=01HXY00000000000000000000Z action=COMMIT rule=x";
        let err = JournalEntry::parse_line(line).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ts"), "expected ts error, got: {msg}");
    }

    #[test]
    fn parse_line_errors_on_unknown_action() {
        let line = format!(
            "ts={ts} input={id} action=YELLOW rule=x",
            ts = fixed_ts().to_rfc3339_opts(SecondsFormat::Millis, true),
            id = fixed_id(),
        );
        let err = JournalEntry::parse_line(&line).unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("action"));
    }

    #[test]
    fn append_then_iter_round_trips() {
        let tmp = tempdir().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        let a = sample_entry();
        let mut b = sample_entry();
        b.action = PolicyAction::Skip;
        b.rule = "default_skip".into();
        b.reasoning = Some("no rule matched".into());

        journal.append(&a).unwrap();
        journal.append(&b).unwrap();

        let entries: Vec<JournalEntry> =
            journal.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries, vec![a, b]);
    }

    #[test]
    fn open_creates_derived_dir_if_missing() {
        let tmp = tempdir().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        // derived/ must exist after open(), even with no entries appended yet.
        assert!(tmp.path().join("derived").is_dir());
        // And the journal file itself is created.
        assert!(journal.path().exists());
    }

    #[test]
    fn entries_persist_across_reopen() {
        let tmp = tempdir().unwrap();
        let entry = sample_entry();
        {
            let j = Journal::open(tmp.path()).unwrap();
            j.append(&entry).unwrap();
        }
        let j2 = Journal::open(tmp.path()).unwrap();
        let entries: Vec<JournalEntry> = j2.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries, vec![entry]);
    }

    #[test]
    fn concurrent_appends_do_not_corrupt_lines() {
        // Same property as event_log: a malformed line would fail parsing.
        let tmp = tempdir().unwrap();
        let journal = Arc::new(Journal::open(tmp.path()).unwrap());

        const THREADS: usize = 8;
        const PER_THREAD: usize = 25;

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let journal = Arc::clone(&journal);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let entry = JournalEntry {
                            ts: fixed_ts(),
                            action: PolicyAction::Commit,
                            rule: "high_signal".into(),
                            input_id: EventId::new(),
                            reasoning: Some(format!("t{t}-i{i}")),
                        };
                        journal.append(&entry).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let entries: Vec<JournalEntry> =
            journal.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries.len(), THREADS * PER_THREAD);
    }

    #[test]
    fn from_decision_drops_empty_reasoning() {
        let dec = Decision {
            action: PolicyAction::Skip,
            rule_id: "default_skip".into(),
            reasoning: String::new(),
        };
        let entry = JournalEntry::from_decision(&dec, fixed_id(), fixed_ts());
        assert!(entry.reasoning.is_none());
    }

    #[test]
    fn from_decision_preserves_nonempty_reasoning() {
        let dec = Decision {
            action: PolicyAction::Commit,
            rule_id: "high_signal".into(),
            reasoning: "looks declarative".into(),
        };
        let entry = JournalEntry::from_decision(&dec, fixed_id(), fixed_ts());
        assert_eq!(entry.reasoning.as_deref(), Some("looks declarative"));
        assert_eq!(entry.action, PolicyAction::Commit);
        assert_eq!(entry.rule, "high_signal");
    }
}
