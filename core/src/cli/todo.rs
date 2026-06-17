//! `localmem todo` handler (T-52b).
//!
//! Mutates the done state of a todo-kind capture by emitting an
//! `UpdateCapture` event. The event log stays append-only (the
//! original capture is never rewritten), so `localmem replay` can
//! reconstruct the latest state by walking forward through the log
//! and re-applying each `UpdateCapture` it sees.
//!
//! Surface:
//!   localmem todo done <event-id>    # mark a todo complete
//!   localmem todo open <event-id>    # reopen a completed todo
//!
//! `event-id` is the ULID of the original capture; `localmem audit`,
//! `localmem search --kind todo`, and `localmem profile` all surface
//! it. A non-todo capture errors loud rather than silently flipping
//! a flag that no view renders.

use crate::event::{Event, EventKind, Source, UpdateCapturePayload};
use crate::event_id::EventId;
use crate::event_log::EventLog;
use crate::kind::Kind;
use crate::lexical::LexicalIndex;
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;

/// Whether to mark the target todo as done or reopen it. Mirrors the
/// two top-level subcommands the user types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoAction {
    /// `localmem todo done <id>` — mark complete.
    Done,
    /// `localmem todo open <id>` — reopen.
    Open,
}

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    ok: bool,
    target_id: &'a str,
    update_event_id: String,
    done: bool,
}

/// Entry point. Resolves the target capture, emits an `UpdateCapture`
/// event, applies the change to the lex index, and returns. Profile
/// rendering picks up the new state from the lex index automatically.
pub fn run(
    home: Option<&str>,
    action: TodoAction,
    target_id_str: &str,
    reason: Option<&str>,
    as_json: bool,
) -> Result<()> {
    let home = resolve_home(home)?;
    let target_id = EventId::from_str(target_id_str)
        .map_err(|e| anyhow!("invalid ULID {target_id_str:?}: {e}"))?;

    // Walk the event log to find the target capture. We need to
    // confirm (a) the id exists, (b) it's a capture (not a fact /
    // update / etc.), and (c) its kind is `Todo`. Each is a separate
    // error to keep the failure messages actionable.
    let event_log = EventLog::open(&home).context("open event log")?;
    let mut target_capture: Option<Event> = None;
    for ev in event_log.iter().context("scan event log")? {
        let ev = ev.context("read event")?;
        if ev.id == target_id {
            target_capture = Some(ev);
            break;
        }
    }
    let target = target_capture
        .ok_or_else(|| anyhow!("no event with id {target_id_str} in this localmem home"))?;
    let cap = match &target.kind {
        EventKind::Capture(p) => p,
        _ => {
            return Err(anyhow!(
                "event {target_id_str} is not a capture (kind={:?}); \
                 `localmem todo` only flips done on capture-kind events",
                std::mem::discriminant(&target.kind)
            ))
        }
    };
    if !matches!(cap.kind, Kind::Todo) {
        return Err(anyhow!(
            "capture {target_id_str} has kind {:?}, not `todo`; \
             refusing to flip done on a non-todo capture",
            cap.kind.as_str()
        ));
    }

    // Emit the UpdateCapture event. The event id is a fresh ULID;
    // the target_id points at the capture. `done` is the only
    // mutable field today.
    let done = action == TodoAction::Done;
    let event = Event::new(
        EventKind::UpdateCapture(UpdateCapturePayload {
            target_id,
            done: Some(done),
            reason: reason.map(|s| s.to_string()),
            extra: serde_json::Map::new(),
        }),
        Source {
            app: "cli".into(),
            host: hostname(),
            user: None,
        },
    );
    event_log
        .append(&event)
        .context("append UpdateCapture event")?;

    // Apply the change to the lex index so `localmem profile` /
    // `localmem search --done` see it without waiting for replay.
    {
        let mut lex = LexicalIndex::open(&home).context("open lexical index for write")?;
        let EventKind::UpdateCapture(payload) = &event.kind else {
            unreachable!("event constructed as UpdateCapture above");
        };
        lex.apply_capture_update(payload)
            .context("apply update to lex index")?;
        lex.commit().context("commit lex index after update")?;
    }

    let payload = JsonOutput {
        ok: true,
        target_id: target_id_str,
        update_event_id: event.id.to_string(),
        done,
    };
    let mut out = io::stdout().lock();
    if as_json {
        serde_json::to_writer(&mut out, &payload).context("serialize todo JSON")?;
        out.write_all(b"\n").context("write JSON newline")?;
    } else {
        let state = if done { "done" } else { "open" };
        writeln!(
            out,
            "ok target={target_id_str} update={} state={state}",
            event.id
        )
        .context("write todo confirmation")?;
    }
    Ok(())
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "unknown".to_string())
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
    use crate::cli::init::run as init_run;
    use crate::event::CapturePayload;
    use tempfile::tempdir;

    // Bypass CLI write entirely and append a capture directly. The
    // CLI write path is covered by its own tests; here we want
    // deterministic event ids without spinning the write pipeline.
    fn append_capture(home: &std::path::Path, text: &str, kind: Kind) -> EventId {
        let log = EventLog::open(home).unwrap();
        let event = Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
                text: text.to_string(),
                rewritten_text: None,
                kind,
                mime: None,
                attachments: vec![],
                tags: Default::default(),
                extra: Default::default(),
            }),
            Source {
                app: "test".into(),
                host: "test".into(),
                user: None,
            },
        );
        log.append(&event).unwrap();
        // Also index in lex so apply_capture_update has something
        // to mutate.
        let mut lex = LexicalIndex::open(home).unwrap();
        lex.index_event(&event).unwrap();
        lex.commit().unwrap();
        event.id
    }

    #[test]
    fn done_flips_lex_done_field_for_a_todo() {
        let tmp = tempdir().unwrap();
        init_run(Some(tmp.path().to_str().unwrap()), true).unwrap();
        let id = append_capture(tmp.path(), "buy milk", Kind::Todo);

        let lex = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let before = lex.meta_for(&id.to_string()).unwrap().unwrap();
        assert!(!before.done, "fresh todo should be open");
        drop(lex);

        run(
            tmp.path().to_str(),
            TodoAction::Done,
            &id.to_string(),
            Some("got it from the store"),
            true,
        )
        .unwrap();

        let lex = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let after = lex.meta_for(&id.to_string()).unwrap().unwrap();
        assert!(after.done, "todo should be done after `todo done`");
    }

    #[test]
    fn open_reverses_done() {
        let tmp = tempdir().unwrap();
        init_run(Some(tmp.path().to_str().unwrap()), true).unwrap();
        let id = append_capture(tmp.path(), "finish report", Kind::Todo);

        run(
            tmp.path().to_str(),
            TodoAction::Done,
            &id.to_string(),
            None,
            true,
        )
        .unwrap();
        run(
            tmp.path().to_str(),
            TodoAction::Open,
            &id.to_string(),
            None,
            true,
        )
        .unwrap();

        let lex = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let meta = lex.meta_for(&id.to_string()).unwrap().unwrap();
        assert!(!meta.done, "todo should be open after `todo open`");
    }

    #[test]
    fn refuses_non_todo_capture() {
        let tmp = tempdir().unwrap();
        init_run(Some(tmp.path().to_str().unwrap()), true).unwrap();
        let id = append_capture(tmp.path(), "I prefer Rust", Kind::Preference);
        let err = run(
            tmp.path().to_str(),
            TodoAction::Done,
            &id.to_string(),
            None,
            true,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not `todo`"),
            "expected non-todo refusal, got: {msg}"
        );
    }

    #[test]
    fn refuses_unknown_target_id() {
        let tmp = tempdir().unwrap();
        init_run(Some(tmp.path().to_str().unwrap()), true).unwrap();
        let bogus = EventId::new().to_string();
        let err = run(tmp.path().to_str(), TodoAction::Done, &bogus, None, true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no event with id"),
            "expected unknown-id error, got: {msg}"
        );
    }
}
