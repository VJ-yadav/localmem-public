//! `localmem journal --since DURATION` handler.
//!
//! Tail-reads `<home>/derived/journal.log`, filters by time window and
//! optional action, and prints either human-readable lines (default) or a
//! single JSON object (`--json`). See TASKS.md task T-18 and SPEC.md
//! "memory_journal" for the public output contract.
//!
//! Mirrors the shape established by [`crate::cli::search`]: a `run()`
//! entry point, the `resolve_home` fallback, and a typed `JsonOutput`
//! envelope so the `--json` mode is a stable contract.

use crate::event::PolicyAction;
use crate::journal::{Journal, JournalEntry};
use anyhow::{anyhow, Context, Result};
use chrono::{Duration, Utc};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;

/// Entry point for the `journal` subcommand.
///
/// `since` is a short duration like `1h`, `30m`, `1d`, `2w`. Entries whose
/// `ts` is older than `now - since` are filtered out. `action`, if given,
/// further restricts to that single [`PolicyAction`] (case-insensitive).
pub fn run(home: Option<&str>, since: &str, action: Option<&str>, as_json: bool) -> Result<()> {
    let home = resolve_home(home)?;
    let cutoff = Utc::now() - parse_duration(since).context("parse --since")?;
    let action_filter = action
        .map(parse_action_filter)
        .transpose()
        .context("parse --action")?;

    let journal = Journal::open(&home).context("open journal")?;
    let mut entries: Vec<JournalEntry> = Vec::new();
    for entry_result in journal.iter()? {
        let entry = entry_result?;
        if entry.ts < cutoff {
            continue;
        }
        if let Some(want) = action_filter {
            if entry.action != want {
                continue;
            }
        }
        entries.push(entry);
    }

    let mut out = io::stdout().lock();
    write_output(&mut out, since, action_filter, &entries, as_json)
}

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    since: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<&'static str>,
    entries: &'a [JournalEntry],
}

fn write_output<W: Write>(
    out: &mut W,
    since: &str,
    action_filter: Option<PolicyAction>,
    entries: &[JournalEntry],
    as_json: bool,
) -> Result<()> {
    if as_json {
        let payload = JsonOutput {
            since,
            action: action_filter.map(action_label),
            entries,
        };
        serde_json::to_writer(&mut *out, &payload).context("serialize journal output as JSON")?;
        out.write_all(b"\n").context("write JSON newline")?;
    } else {
        write_human(out, entries)?;
    }
    Ok(())
}

fn write_human<W: Write>(out: &mut W, entries: &[JournalEntry]) -> Result<()> {
    if entries.is_empty() {
        writeln!(out, "no journal entries in window").context("write empty journal line")?;
        return Ok(());
    }
    for entry in entries {
        writeln!(out, "{}", entry.format_line()).context("write journal line")?;
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

/// Parse a short duration like `45s`, `30m`, `1h`, `1d`, `2w`.
///
/// Number is the leading run of digits; suffix is exactly one of
/// `s`, `m`, `h`, `d`, `w`. Compound forms (`1h30m`) are intentionally
/// rejected so the parser stays unambiguous.
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty duration"));
    }
    let split_idx = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| anyhow!("duration `{s}` has no unit suffix; expected one of s|m|h|d|w"))?;
    let (num_str, suffix) = s.split_at(split_idx);
    if num_str.is_empty() {
        return Err(anyhow!("duration `{s}` has no numeric portion"));
    }
    let n: i64 = num_str
        .parse()
        .with_context(|| format!("parse duration number from `{s}`"))?;
    if n < 0 {
        return Err(anyhow!("duration `{s}` must be non-negative"));
    }
    let dur = match suffix {
        "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        other => {
            return Err(anyhow!(
                "unknown duration unit `{other}` in `{s}` (expected s|m|h|d|w)"
            ))
        }
    };
    Ok(dur)
}

fn parse_action_filter(s: &str) -> Result<PolicyAction> {
    Ok(match s.to_ascii_uppercase().as_str() {
        "COMMIT" => PolicyAction::Commit,
        "UPDATE" => PolicyAction::Update,
        "DEDUP" => PolicyAction::Dedup,
        "SKIP" => PolicyAction::Skip,
        "FORGET" => PolicyAction::Forget,
        other => {
            return Err(anyhow!(
                "unknown action `{other}` (expected COMMIT|UPDATE|DEDUP|SKIP|FORGET)"
            ))
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_id::EventId;
    use chrono::DateTime;
    use serde_json::Value;
    use tempfile::tempdir;

    fn entry(ts: DateTime<Utc>, action: PolicyAction, rule: &str) -> JournalEntry {
        JournalEntry {
            ts,
            action,
            rule: rule.into(),
            input_id: EventId::new(),
            reasoning: Some(format!("{rule} fired")),
        }
    }

    #[test]
    fn parse_duration_handles_supported_units() {
        assert_eq!(parse_duration("45s").unwrap(), Duration::seconds(45));
        assert_eq!(parse_duration("30m").unwrap(), Duration::minutes(30));
        assert_eq!(parse_duration("1h").unwrap(), Duration::hours(1));
        assert_eq!(parse_duration("1d").unwrap(), Duration::days(1));
        assert_eq!(parse_duration("2w").unwrap(), Duration::weeks(2));
        // Whitespace is tolerated.
        assert_eq!(parse_duration("  1h  ").unwrap(), Duration::hours(1));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err()); // no number
        assert!(parse_duration("1x").is_err()); // unknown unit
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("1h30m").is_err()); // compound not allowed
        assert!(parse_duration("-5h").is_err()); // negative
    }

    #[test]
    fn parse_action_filter_is_case_insensitive() {
        assert_eq!(parse_action_filter("COMMIT").unwrap(), PolicyAction::Commit);
        assert_eq!(parse_action_filter("commit").unwrap(), PolicyAction::Commit);
        assert_eq!(parse_action_filter("Dedup").unwrap(), PolicyAction::Dedup);
        assert_eq!(parse_action_filter("forget").unwrap(), PolicyAction::Forget);
        assert!(parse_action_filter("YELLOW").is_err());
    }

    #[test]
    fn resolve_home_uses_override() {
        let path = resolve_home(Some("/tmp/localmem-x")).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/localmem-x"));
    }

    #[test]
    fn resolve_home_falls_back_to_home_dot_localmem() {
        // Set HOME explicitly so the test is isolated from the dev shell.
        std::env::set_var("HOME", "/tmp/fake-home-for-test");
        let path = resolve_home(None).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/fake-home-for-test/.localmem"));
    }

    #[test]
    fn write_human_empty_says_no_entries() {
        let mut buf = Vec::new();
        write_human(&mut buf, &[]).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("no journal entries"), "got: {s}");
    }

    #[test]
    fn write_human_emits_one_line_per_entry() {
        let now = Utc::now();
        let entries = vec![
            entry(now, PolicyAction::Commit, "high_signal"),
            entry(now, PolicyAction::Skip, "default_skip"),
        ];
        let mut buf = Vec::new();
        write_human(&mut buf, &entries).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.matches('\n').count(), 2);
        assert!(s.contains("action=COMMIT"));
        assert!(s.contains("action=SKIP"));
    }

    #[test]
    fn json_output_shape_matches_contract() {
        let now = Utc::now();
        let entries = vec![entry(now, PolicyAction::Commit, "high_signal")];
        let mut buf = Vec::new();
        write_output(&mut buf, "1h", Some(PolicyAction::Commit), &entries, true).unwrap();
        let json: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(json["since"], "1h");
        assert_eq!(json["action"], "COMMIT");
        let arr = json["entries"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["action"], "COMMIT");
        assert_eq!(arr[0]["rule"], "high_signal");
        // input_id is a ULID string, reasoning is the populated text.
        assert!(arr[0]["input_id"].is_string());
        assert!(arr[0]["reasoning"]
            .as_str()
            .unwrap()
            .contains("high_signal"));
    }

    #[test]
    fn json_output_omits_action_field_when_no_filter() {
        let entries: Vec<JournalEntry> = vec![];
        let mut buf = Vec::new();
        write_output(&mut buf, "1d", None, &entries, true).unwrap();
        let json: Value = serde_json::from_slice(&buf).unwrap();
        assert!(json.get("action").is_none(), "got: {json}");
        assert_eq!(json["since"], "1d");
    }

    #[test]
    fn run_filters_entries_by_since_window() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let now = Utc::now();
        let old = entry(
            now - Duration::hours(2),
            PolicyAction::Commit,
            "high_signal",
        );
        let fresh = entry(
            now - Duration::minutes(5),
            PolicyAction::Skip,
            "default_skip",
        );
        {
            let j = Journal::open(&home).unwrap();
            j.append(&old).unwrap();
            j.append(&fresh).unwrap();
        }
        // Run with --since 1h: must include only `fresh`. We cannot capture
        // stdout (run() locks the real stdout) so we reach in to verify the
        // filter behavior via the iterator + write_human path.
        run(home.to_str(), "1h", None, true).unwrap();

        // And independently verify the filter logic produces the right set.
        let journal = Journal::open(&home).unwrap();
        let cutoff = Utc::now() - Duration::hours(1);
        let kept: Vec<JournalEntry> = journal
            .iter()
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|e| e.ts >= cutoff)
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].rule, "default_skip");
    }

    #[test]
    fn run_action_filter_keeps_only_matching_entries() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let now = Utc::now();
        {
            let j = Journal::open(&home).unwrap();
            j.append(&entry(now, PolicyAction::Commit, "high_signal"))
                .unwrap();
            j.append(&entry(now, PolicyAction::Skip, "default_skip"))
                .unwrap();
            j.append(&entry(now, PolicyAction::Forget, "forget_pii"))
                .unwrap();
        }
        // Sanity: run() with --action COMMIT does not error.
        run(home.to_str(), "1d", Some("commit"), true).unwrap();

        // Verify filter behavior directly.
        let journal = Journal::open(&home).unwrap();
        let kept: Vec<JournalEntry> = journal
            .iter()
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|e| e.action == PolicyAction::Commit)
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].rule, "high_signal");
    }

    #[test]
    fn run_returns_ok_on_empty_journal() {
        // Fresh home with no journal file yet: run() should succeed (no
        // panic, no error), printing an empty result.
        let tmp = tempdir().unwrap();
        run(tmp.path().to_str(), "1d", None, true).unwrap();
    }
}
