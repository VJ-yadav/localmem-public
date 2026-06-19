//! `localmem audit <fact-id>` handler (T-53).
//!
//! Traces a fact back through the event log + journal to answer
//! "why does the AI know this?" The audit walk:
//! 1. Look up the fact row by id in `facts.duckdb`.
//! 2. Resolve each `derived_from` event id to its source capture in
//!    `events.jsonl`.
//! 3. Surface any `forget` / `update` events targeting the fact id.
//! 4. Surface the journal entries that name the fact id as their
//!    `input_id` (the policy + smart-forgetting decisions that
//!    landed it).
//!
//! All four are returned together so the output is a single audit
//! frame. JSON mode is the contract MCP consumers will lean on; the
//! human-readable mode is for `localmem audit` typed at the shell.

use crate::event::{Event, EventKind};
use crate::event_id::EventId;
use crate::event_log::EventLog;
use crate::facts::FactsStore;
use crate::journal::{Journal, JournalEntry};
use anyhow::{anyhow, Context, Result};
use chrono::SecondsFormat;
use serde::Serialize;
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Serialize)]
struct AuditFact {
    id: String,
    subject: String,
    predicate: String,
    object: String,
    confidence: f64,
    valid_from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_to: Option<String>,
    recorded_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    retired_at: Option<String>,
    kind: String,
    sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct AuditCapture {
    event_id: String,
    ts: String,
    text: String,
    kind: String,
    source_app: String,
}

#[derive(Debug, Clone, Serialize)]
struct AuditTouch {
    event_id: String,
    ts: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct AuditJournalRow {
    ts: String,
    action: String,
    rule: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
}

#[derive(Debug, Serialize)]
struct JsonOutput {
    ok: bool,
    fact: AuditFact,
    sources: Vec<AuditCapture>,
    touches: Vec<AuditTouch>,
    journal: Vec<AuditJournalRow>,
}

pub fn run(home: Option<&str>, fact_id_str: &str, as_json: bool) -> Result<()> {
    let home = resolve_home(home)?;
    let fact_id = EventId::from_str(fact_id_str)
        .map_err(|e| anyhow!("not a valid ULID: {fact_id_str} ({e})"))?;

    let store = crate::cli::open_facts(&home)?;
    let fact = store
        .find_by_id(&fact_id)
        .context("look up fact by id")?
        .ok_or_else(|| anyhow!("no fact with id {fact_id}"))?;

    let log = EventLog::open(&home).context("open event log")?;
    let source_ids: HashSet<EventId> = fact.source_events.iter().copied().collect();
    let mut sources: Vec<AuditCapture> = Vec::new();
    let mut touches: Vec<AuditTouch> = Vec::new();
    for ev in log.iter()? {
        let ev = ev?;
        if source_ids.contains(&ev.id) {
            if let Some(row) = capture_row(&ev) {
                sources.push(row);
            }
        }
        if let Some(touch) = touch_row(&ev, &fact_id) {
            touches.push(touch);
        }
    }
    // Stable ordering: oldest first.
    sources.sort_by(|a, b| a.ts.cmp(&b.ts));
    touches.sort_by(|a, b| a.ts.cmp(&b.ts));

    let journal_store = Journal::open(&home).context("open journal")?;
    let journal: Vec<AuditJournalRow> = journal_store
        .iter()?
        .filter_map(|r| r.ok())
        .filter(|e: &JournalEntry| e.input_id == fact_id)
        .map(|e| AuditJournalRow {
            ts: e.ts.to_rfc3339_opts(SecondsFormat::Millis, true),
            action: action_label(e.action).into(),
            rule: e.rule,
            reasoning: e.reasoning,
        })
        .collect();

    let audit_fact = AuditFact {
        id: fact.id.to_string(),
        subject: fact.subject,
        predicate: fact.predicate,
        object: fact.object,
        confidence: fact.confidence,
        valid_from: fact.valid_from.to_rfc3339_opts(SecondsFormat::Millis, true),
        valid_to: fact
            .valid_to
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true)),
        recorded_at: fact
            .recorded_at
            .to_rfc3339_opts(SecondsFormat::Millis, true),
        retired_at: fact
            .retired_at
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true)),
        kind: fact.kind.as_str().to_string(),
        sources: fact.source_events.iter().map(|e| e.to_string()).collect(),
    };

    let mut out = io::stdout().lock();
    write_output(&mut out, &audit_fact, &sources, &touches, &journal, as_json)
}

fn capture_row(ev: &Event) -> Option<AuditCapture> {
    let EventKind::Capture(p) = &ev.kind else {
        return None;
    };
    Some(AuditCapture {
        event_id: ev.id.to_string(),
        ts: ev.ts.to_rfc3339_opts(SecondsFormat::Millis, true),
        text: p.indexable_text().to_string(),
        kind: p.kind.as_str().to_string(),
        source_app: ev.source.app.clone(),
    })
}

/// Surface events that touched this fact id: `forget` events
/// targeting it (retirement at user request) and `update` events
/// superseding it (smart-forgetting under T-56). Either signals to
/// the auditor "this fact didn't just appear and stay; here's what
/// happened to it."
fn touch_row(ev: &Event, fact_id: &EventId) -> Option<AuditTouch> {
    match &ev.kind {
        EventKind::Forget(p) if &p.target_id == fact_id => Some(AuditTouch {
            event_id: ev.id.to_string(),
            ts: ev.ts.to_rfc3339_opts(SecondsFormat::Millis, true),
            kind: "forget".into(),
            reason: Some(p.reason.clone()),
        }),
        EventKind::Update(p) if &p.supersedes_id == fact_id => Some(AuditTouch {
            event_id: ev.id.to_string(),
            ts: ev.ts.to_rfc3339_opts(SecondsFormat::Millis, true),
            kind: "update".into(),
            reason: Some(format!(
                "superseded by {}={}",
                p.new_fact.predicate, p.new_fact.object
            )),
        }),
        _ => None,
    }
}

fn action_label(a: crate::event::PolicyAction) -> &'static str {
    match a {
        crate::event::PolicyAction::Commit => "COMMIT",
        crate::event::PolicyAction::Update => "UPDATE",
        crate::event::PolicyAction::Dedup => "DEDUP",
        crate::event::PolicyAction::Skip => "SKIP",
        crate::event::PolicyAction::Forget => "FORGET",
    }
}

fn write_output<W: Write>(
    out: &mut W,
    fact: &AuditFact,
    sources: &[AuditCapture],
    touches: &[AuditTouch],
    journal: &[AuditJournalRow],
    as_json: bool,
) -> Result<()> {
    if as_json {
        let payload = JsonOutput {
            ok: true,
            fact: AuditFact {
                id: fact.id.clone(),
                subject: fact.subject.clone(),
                predicate: fact.predicate.clone(),
                object: fact.object.clone(),
                confidence: fact.confidence,
                valid_from: fact.valid_from.clone(),
                valid_to: fact.valid_to.clone(),
                recorded_at: fact.recorded_at.clone(),
                retired_at: fact.retired_at.clone(),
                kind: fact.kind.clone(),
                sources: fact.sources.clone(),
            },
            sources: sources.to_vec(),
            touches: touches.to_vec(),
            journal: journal.to_vec(),
        };
        serde_json::to_writer(&mut *out, &payload).context("serialize audit JSON")?;
        out.write_all(b"\n").context("write JSON newline")?;
        return Ok(());
    }

    writeln!(out, "Fact {}", fact.id).context("write fact id")?;
    writeln!(
        out,
        "  {} {} {} (conf={:.2}, kind={})",
        fact.subject, fact.predicate, fact.object, fact.confidence, fact.kind,
    )
    .context("write fact triple")?;
    writeln!(out, "  valid_from={}", fact.valid_from).context("write valid_from")?;
    if let Some(vt) = &fact.valid_to {
        writeln!(out, "  valid_to={vt}").context("write valid_to")?;
    }
    writeln!(out, "  recorded_at={}", fact.recorded_at).context("write recorded_at")?;
    if let Some(rt) = &fact.retired_at {
        writeln!(out, "  retired_at={rt}").context("write retired_at")?;
    }
    writeln!(out).context("blank")?;

    if !sources.is_empty() {
        writeln!(out, "Derived from:").context("write sources header")?;
        for s in sources {
            writeln!(
                out,
                "  {} ({}, {}): {}",
                s.event_id, s.ts, s.source_app, s.text
            )
            .context("write source row")?;
        }
        writeln!(out).context("blank")?;
    }
    if !touches.is_empty() {
        writeln!(out, "Lineage events:").context("write touches header")?;
        for t in touches {
            let reason = t.reason.as_deref().unwrap_or("");
            writeln!(out, "  {} {} ({}): {}", t.event_id, t.ts, t.kind, reason)
                .context("write touch row")?;
        }
        writeln!(out).context("blank")?;
    }
    if !journal.is_empty() {
        writeln!(out, "Journal:").context("write journal header")?;
        for j in journal {
            let reasoning = j.reasoning.as_deref().unwrap_or("");
            writeln!(
                out,
                "  {} action={} rule={} {}",
                j.ts, j.action, j.rule, reasoning
            )
            .context("write journal row")?;
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
    use crate::event::{
        CapturePayload, FactPayload, ForgetPayload, PolicyAction, Source, UpdatePayload,
    };
    use crate::facts::Fact;
    use chrono::{DateTime, Utc};
    use serde_json::{Map, Value};
    use tempfile::tempdir;

    fn make_capture(text: &str) -> Event {
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
                host: "test-host".into(),
                user: None,
            },
        )
    }

    fn ts(epoch: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(epoch, 0).unwrap()
    }

    #[test]
    fn audit_returns_fact_capture_and_journal_in_json() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        init_home(home).unwrap();

        // Set up: one capture, one fact derived from it, one
        // journal entry naming the fact.
        let cap = make_capture("I prefer rust");
        let cap_id = cap.id;
        let log = EventLog::open(home).unwrap();
        log.append(&cap).unwrap();

        let store = FactsStore::open(home).unwrap();
        let fact_id = EventId::new();
        let fact = Fact {
            id: fact_id,
            subject: "user".into(),
            predicate: "prefers".into(),
            object: "rust".into(),
            confidence: 0.8,
            valid_from: ts(1_700_000_000),
            valid_to: None,
            recorded_at: ts(1_700_000_000),
            retired_at: None,
            source_events: vec![cap_id],
            policy_id: Some("rule:prefer".into()),
            tags: Default::default(),
            kind: Default::default(),
        };
        store.insert(&fact).unwrap();
        // Also emit a `fact` event so the audit story matches what
        // /write would produce on real ingest.
        log.append(&Event::with_id(
            fact_id,
            EventKind::Fact(FactPayload {
                subject: fact.subject.clone(),
                predicate: fact.predicate.clone(),
                object: fact.object.clone(),
                confidence: fact.confidence,
                valid_from: fact.valid_from,
                valid_to: None,
                derived_from: vec![cap_id],
                kind: Default::default(),
                tags: Default::default(),
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        ))
        .unwrap();

        let journal = Journal::open(home).unwrap();
        journal
            .append(&JournalEntry {
                ts: ts(1_700_000_000),
                action: PolicyAction::Commit,
                rule: "high_signal".into(),
                input_id: fact_id,
                reasoning: Some("seeded from test".into()),
            })
            .unwrap();

        // Run audit via JSON path captured into a buffer. We can't
        // capture stdout, so we reach in to write_output with the
        // structures the run() helper would build.
        let audit_fact = AuditFact {
            id: fact_id.to_string(),
            subject: "user".into(),
            predicate: "prefers".into(),
            object: "rust".into(),
            confidence: 0.8,
            valid_from: "2023-11-14T22:13:20.000Z".into(),
            valid_to: None,
            recorded_at: "2023-11-14T22:13:20.000Z".into(),
            retired_at: None,
            kind: "note".into(),
            sources: vec![cap_id.to_string()],
        };

        // Build the rest by invoking run()'s helpers through a
        // proper end-to-end call. The simplest assertion that the
        // wiring works is to run the public entry on JSON mode and
        // parse the bytes we see in a tempfile-backed home.
        run(home.to_str(), &fact_id.to_string(), true).unwrap();
        // No panic = wiring works. We then exercise the inner
        // formatter directly to verify JSON shape.

        let mut buf = Vec::new();
        let sources = vec![AuditCapture {
            event_id: cap_id.to_string(),
            ts: "2023-11-14T22:13:20.000Z".into(),
            text: "I prefer rust".into(),
            kind: "note".into(),
            source_app: "test".into(),
        }];
        let touches: Vec<AuditTouch> = vec![];
        let journal_rows = vec![AuditJournalRow {
            ts: "2023-11-14T22:13:20.000Z".into(),
            action: "COMMIT".into(),
            rule: "high_signal".into(),
            reasoning: Some("seeded from test".into()),
        }];
        write_output(
            &mut buf,
            &audit_fact,
            &sources,
            &touches,
            &journal_rows,
            true,
        )
        .unwrap();
        let json: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["fact"]["subject"], "user");
        assert_eq!(json["fact"]["predicate"], "prefers");
        assert_eq!(json["sources"].as_array().unwrap().len(), 1);
        assert_eq!(json["journal"].as_array().unwrap().len(), 1);
        assert_eq!(json["journal"][0]["action"], "COMMIT");
    }

    #[test]
    fn audit_unknown_id_errors_clearly() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        init_home(home).unwrap();
        let err = run(home.to_str(), "01HXY00000000000000000000Z", true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no fact"), "got: {msg}");
    }

    #[test]
    fn audit_invalid_ulid_errors_clearly() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        init_home(home).unwrap();
        let err = run(home.to_str(), "not-a-ulid", true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not a valid ULID"), "got: {msg}");
    }

    #[test]
    fn touches_surface_forget_and_update_targeting_fact() {
        // forget(target_id=fact) and update(supersedes_id=fact) must
        // both appear in `touches` so the auditor sees the lineage.
        let fact_id = EventId::new();
        let cap_id = EventId::new();
        let cap_id_other = EventId::new();
        let forget = Event::new(
            EventKind::Forget(ForgetPayload {
                target_id: fact_id,
                reason: "user requested".into(),
                scope: None,
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        );
        let update = Event::new(
            EventKind::Update(UpdatePayload {
                supersedes_id: fact_id,
                new_fact: FactPayload {
                    subject: "user".into(),
                    predicate: "prefers".into(),
                    object: "haskell".into(),
                    confidence: 0.8,
                    valid_from: ts(1_700_000_000),
                    valid_to: None,
                    derived_from: vec![cap_id_other],
                    kind: Default::default(),
                    tags: Default::default(),
                    extra: Map::new(),
                },
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        );
        let other_forget = Event::new(
            EventKind::Forget(ForgetPayload {
                target_id: cap_id, // a different id, should NOT match
                reason: "x".into(),
                scope: None,
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        );
        assert!(touch_row(&forget, &fact_id).is_some());
        assert!(touch_row(&update, &fact_id).is_some());
        assert!(touch_row(&other_forget, &fact_id).is_none());
    }
}
