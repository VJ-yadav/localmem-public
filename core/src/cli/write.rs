//! `localmem write` handler.
//!
//! Ingests one capture event. See SPEC.md "localmem write" for the
//! contract: emit a capture, run policy, journal the decision, and on
//! COMMIT update every available derived store.
//!
//! Embedder is optional at write time. If the BGE assets aren't on disk
//! we still update lexical + facts so future writes and replays produce
//! consistent state; vectors come back on the next replay once the
//! model is installed.

use crate::embed::{Embedder, EMBEDDING_DIM};
use crate::event::{CapturePayload, Event, EventKind, FactPayload, PolicyAction, Source};
use crate::event_log::EventLog;
use crate::extractor::ExtractorRegistry;
use crate::facts::Fact;
use crate::journal::{Journal, JournalEntry};
use crate::lexical::{LexicalIndex, LexicalResultExt};
use crate::policy::{EvalContext, Policy};
use crate::vectors::VectorStore;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use tracing::warn;

/// Build the configured rewriter, apply it to `text`, and return
/// the rewritten output only when it differs from the input. A
/// no-op rewrite stays `None` so the wire shape and storage cost
/// match v0.1 for non-rewriting captures.
///
/// Rewriter failures are logged at WARN and degrade to "no rewrite"
/// rather than failing the entire write. The capture still commits;
/// the user can re-run after fixing config without losing data.
/// Misconfiguration (unknown mode) is the one case we surface
/// loudly — `Config::load` already errors there, so this fn is
/// reached only with a valid mode string.
fn build_and_apply_rewriter(cfg: &crate::config::Config, text: &str) -> Option<String> {
    let rewriter = match crate::rewriter::build(&cfg.rewriter.mode) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, mode = %cfg.rewriter.mode,
                  "rewriter config invalid; falling back to no-rewrite");
            return None;
        }
    };
    let user_name = crate::rewriter::resolve_user_name(&cfg.home.user_name);
    match rewriter.rewrite(text, &user_name) {
        Ok(out) if out != text => Some(out),
        Ok(_) => None,
        Err(e) => {
            warn!(error = %e, mode = %cfg.rewriter.mode,
                  "rewriter call failed; falling back to no-rewrite");
            None
        }
    }
}

/// Mirror of [`crate::cli::search::resolve_model_dir`]. Inlined here so
/// the search module is not pulled into the write module's surface.
fn resolve_model_dir(home: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var("LOCALMEM_MODEL_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home.join("models").join("bge-small-en-v1.5")
}

/// Stable maximum recent-event window used by both write and replay so
/// policy dedup behaves the same in both code paths.
const POLICY_RECENT_WINDOW: usize = 100;

/// Emit the "embedder unavailable" WARN at most once per process.
///
/// Field-feedback fix (P1, 2026-06-04): without this, a batch of
/// `localmem write` calls on a model-less install previously printed
/// the same 4-line warning N times. Once-per-process keeps the
/// signal: the user still sees it on the first miss, every later
/// miss only logs at TRACE so `--json` callers and batch agents
/// don't drown in noise.
fn warn_embedder_missing_once_in_write(model_dir: &Path, err: &anyhow::Error) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        tracing::trace!(
            model_dir = %model_dir.display(),
            error = %err,
            "embedder unavailable (already warned this process); capture indexed lex+facts only"
        );
        return;
    }
    warn!(
        model_dir = %model_dir.display(),
        error = %err,
        "embedder unavailable; capture indexed in lex+facts only. \
         Run `localmem replay` after installing the model to backfill vectors."
    );
}

/// CLI-side write result. Shape matches SPEC.md memory_write output
/// (`facts_extracted` is a count, not an array) so the JSON the CLI
/// emits is interchangeable with the JSON the MCP tool returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriteOutput {
    pub event_id: String,
    pub action: String,
    pub facts_extracted: u32,
    /// T-55: filled when the rewriter produced a different version
    /// of the input. Absent otherwise (consistent with the on-disk
    /// `rewritten_text` field), so the JSON shape stays
    /// indistinguishable from v0.1 for non-rewriting writes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewritten_text: Option<String>,
}

/// Entry point for the `write` subcommand.
///
/// Tries to route through the local HTTP server first (T-45) to avoid
/// the Tantivy writer-lock collision when `localmem serve` is running.
/// Falls back to the in-process pipeline when no server responds.
pub async fn run(
    home: Option<&str>,
    content: Option<&str>,
    source: Option<&str>,
    tags: BTreeMap<String, String>,
    kind: crate::kind::Kind,
    as_of: Option<&str>,
    as_json: bool,
) -> Result<()> {
    let home = resolve_home(home)?;
    let text = read_content(content)?;
    if text.is_empty() {
        bail!("write requires non-empty content via --content or stdin");
    }
    let source_app = source.unwrap_or("cli");

    // Parse --as-of once here so a malformed value fails fast and identically
    // on both the remote and the in-process path.
    let as_of_instant = as_of
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Utc))
                .with_context(|| format!("--as-of must be RFC3339, got {s:?}"))
        })
        .transpose()?;

    // Resolve server addr: config -> env override -> default. This mirrors
    // the resolution order `localmem serve` uses, so the CLI and server
    // agree on where to connect by default.
    let cfg = crate::config::Config::load(&home).context("load config")?;
    let server_addr = cfg.server.addr.clone();
    let out = if let Some(remote) =
        try_post_write(&server_addr, &text, source_app, &tags, &kind, as_of_instant).await
    {
        remote
    } else {
        write_capture_at(&home, &text, source_app, tags, kind, as_of_instant).await?
    };
    emit(&out, as_json)
}

/// Probe `GET http://<addr>/health` with a 200ms budget; if reachable,
/// POST the write to `/write` and return its response. Failure of any
/// step yields `None` so the CLI falls back to the in-process pipeline.
async fn try_post_write(
    addr: &str,
    content: &str,
    source: &str,
    tags: &BTreeMap<String, String>,
    kind: &crate::kind::Kind,
    as_of: Option<chrono::DateTime<Utc>>,
) -> Option<WriteOutput> {
    let base = format!("http://{addr}");
    let client = std::sync::Arc::new(
        ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_millis(200))
            .build(),
    );

    let client_probe = std::sync::Arc::clone(&client);
    let base_probe = base.clone();
    let probe_ok = tokio::task::spawn_blocking(move || {
        match client_probe.get(&format!("{base_probe}/health")).call() {
            Ok(r) => r.status() == 200,
            Err(_) => false,
        }
    })
    .await
    .ok()?;
    if !probe_ok {
        return None;
    }

    // Only include `tags` and `kind` in the request body when
    // they're non-default. The server accepts both shapes; omitting
    // the keys keeps the wire format identical for v0.1-style
    // writes.
    let mut body = serde_json::json!({
        "content": content,
        "source": source,
    });
    if !tags.is_empty() {
        body.as_object_mut()
            .expect("body was just built as object")
            .insert(
                "tags".into(),
                serde_json::to_value(tags).expect("BTreeMap<String,String> is JSON-serializable"),
            );
    }
    if !kind.is_note() {
        body.as_object_mut()
            .expect("body is object")
            .insert("kind".into(), Value::String(kind.as_str().to_string()));
    }
    if let Some(instant) = as_of {
        body.as_object_mut().expect("body is object").insert(
            "as_of".into(),
            Value::String(instant.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
    }
    let client_post = std::sync::Arc::clone(&client);
    let base_post = base;
    let resp_text = tokio::task::spawn_blocking(move || {
        let resp = client_post
            .post(&format!("{base_post}/write"))
            .send_json(body)
            .ok()?;
        if resp.status() != 200 {
            return None;
        }
        resp.into_string().ok()
    })
    .await
    .ok()??;
    let v: serde_json::Value = serde_json::from_str(&resp_text).ok()?;
    Some(WriteOutput {
        event_id: v.get("event_id")?.as_str()?.to_string(),
        action: v.get("action")?.as_str()?.to_string(),
        facts_extracted: v.get("facts_extracted").and_then(Value::as_u64)? as u32,
        // Server includes rewritten_text only when the rewriter
        // produced a different version; absence = no rewrite.
        rewritten_text: v
            .get("rewritten_text")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Core write pipeline. Public for tests and (eventually) the HTTP
/// `/write` handler so the CLI and server share one implementation.
/// Stamps the capture's valid-time to now; use [`write_capture_at`] to
/// record a memory that happened at a different instant.
pub async fn write_capture(
    home: &Path,
    text: &str,
    source_app: &str,
    tags: BTreeMap<String, String>,
    kind: crate::kind::Kind,
) -> Result<WriteOutput> {
    write_capture_at(home, text, source_app, tags, kind, None).await
}

/// Like [`write_capture`] but pins the capture's valid-time (the temporal
/// envelope) to `as_of` when given. `None` stamps now. This is the in-process
/// twin of the server `/write` `as_of` field, so the CLI and HTTP paths produce
/// an identical event for the same input.
pub async fn write_capture_at(
    home: &Path,
    text: &str,
    source_app: &str,
    tags: BTreeMap<String, String>,
    kind: crate::kind::Kind,
    as_of: Option<chrono::DateTime<Utc>>,
) -> Result<WriteOutput> {
    let event_log = EventLog::open(home).context("open event log")?;

    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let user = std::env::var("USER").ok().filter(|s| !s.is_empty());

    // T-55: run the configured rewriter on the original text. We
    // record both the rewriter mode and the resolved user_name in
    // the trace so a confused user can grep the log to see what
    // happened to their capture text.
    let cfg = crate::config::Config::load(home).context("load config for rewriter")?;
    let rewritten_text = build_and_apply_rewriter(&cfg, text);

    let capture = Event::new(
        EventKind::Capture(CapturePayload {
            text: text.to_string(),
            kind,
            rewritten_text,
            mime: None,
            attachments: vec![],
            tags,
            time: Some(match as_of {
                Some(instant) => crate::temporal::TimeEnvelope::from_instant(instant),
                None => crate::temporal::TimeEnvelope::capture_now(),
            }),
            extra: Map::new(),
        }),
        Source {
            app: source_app.into(),
            host,
            user,
        },
    );
    event_log
        .append(&capture)
        .context("append capture to event log")?;

    let policy = Policy::load(home).context("load policy")?;
    let recent = load_recent(&event_log, POLICY_RECENT_WINDOW, capture.id)?;
    let decision = policy
        .evaluate(&capture, &EvalContext { recent: &recent })
        .context("evaluate policy")?;
    let journal = Journal::open(home).context("open journal")?;
    journal
        .append(&JournalEntry::from_decision(
            &decision, capture.id, capture.ts,
        ))
        .context("append journal entry")?;

    let action_label = match decision.action {
        PolicyAction::Commit => "COMMIT",
        PolicyAction::Update => "UPDATE",
        PolicyAction::Dedup => "DEDUP",
        PolicyAction::Skip => "SKIP",
        PolicyAction::Forget => "FORGET",
    };

    let mut facts_count: u32 = 0;
    if decision.action == PolicyAction::Commit {
        facts_count = commit_capture(home, &capture, &event_log, &decision.rule_id).await?;
    }

    // Surface the rewritten text on the response so CLI callers can
    // see what their capture got indexed as without needing to read
    // the event log.
    let rewritten_text = match &capture.kind {
        EventKind::Capture(p) => p.rewritten_text.clone(),
        _ => None,
    };
    Ok(WriteOutput {
        event_id: capture.id.to_string(),
        action: action_label.to_string(),
        facts_extracted: facts_count,
        rewritten_text,
    })
}

/// On COMMIT: update lexical, vectors (if embedder available), and facts.
/// Returns the ids of any fact events appended to the log.
async fn commit_capture(
    home: &Path,
    capture: &Event,
    event_log: &EventLog,
    rule_id: &str,
) -> Result<u32> {
    let payload = match &capture.kind {
        EventKind::Capture(p) => p,
        _ => bail!("commit_capture invoked on non-capture event"),
    };

    // Lexical always indexes; BM25 is cheap and works without the model.
    let mut lexical = LexicalIndex::open(home).lex_context("open lexical index")?;
    lexical
        .index_event(capture)
        .context("index capture in lexical")?;
    lexical.commit().context("commit lexical writer")?;

    // Vector store is gated on the embedder. A missing model is logged
    // and the write still succeeds; the next `localmem replay` after
    // installing the model rebuilds the vector store from the log.
    //
    // T-55: embed the indexable text (rewritten when present, else
    // original). The vec store also stores this string as the
    // result-side `content`, so search snippets match what lex
    // returns.
    let model_dir = resolve_model_dir(home);
    let to_index = payload.indexable_text();
    match Embedder::load(&model_dir) {
        Ok(mut emb) => {
            let v = emb.embed(to_index).context("embed capture content")?;
            let vectors = VectorStore::open(home, EMBEDDING_DIM)
                .await
                .context("open vector store")?;
            let tags_json = match &capture.kind {
                crate::event::EventKind::Capture(p) => {
                    serde_json::to_string(&p.tags).unwrap_or_else(|_| "{}".into())
                }
                _ => "{}".to_string(),
            };
            vectors
                .add(
                    &capture.id.to_string(),
                    &v,
                    to_index,
                    &tags_json,
                    capture.ts,
                )
                .await
                .context("write embedding to vectors.lance")?;
        }
        Err(err) => {
            warn_embedder_missing_once_in_write(&model_dir, &err);
        }
    }

    // Facts: run the extractor; for each extracted fact, check
    // T-56 contradiction resolution against prior live facts. If a
    // contradiction fires, emit an `Update` event (with the new
    // fact's payload) and journal each retirement. Otherwise emit
    // a plain `fact` event. Either event's id becomes the new
    // fact's primary key in DuckDB so `replay` can reconstruct the
    // row from `events.jsonl` alone.
    // T-58 + T-59: build the registry from the home's `[extractor]`
    // config AND the user's YAML extractors dir. Same loud-failure
    // discipline as the server: a config typo or broken YAML aborts
    // the write rather than silently dropping extraction. The
    // registry build is sync (regex + YAML parsing); only the actual
    // extract call awaits. `Config::load` returns defaults if config
    // is absent, so a fresh home still gets the rules path.
    let cfg = crate::config::Config::load(home).context("load config for extractor")?;
    let extractor = ExtractorRegistry::from_config_with_home(&cfg.extractor, home)
        .context("build extractor registry")?;
    let extracted = extractor
        .extract(&payload.text, Some(&payload.kind))
        .await
        .context("registry extract")?;
    let facts_store = crate::cli::open_facts(home)?;
    let journal = Journal::open(home).context("open journal for contradictions")?;
    let mut count: u32 = 0;
    for ef in &extracted {
        let new_payload = build_fact_payload(capture, ef);
        // Reserve the event id upfront so the in-memory `Fact`
        // (passed to resolve_contradiction) shares it with whichever
        // event ends up appended below.
        let new_event_id = crate::event_id::EventId::new();
        let mut new_fact = Fact::from_event(
            new_event_id,
            &new_payload,
            Utc::now(),
            Some(rule_id.to_string()),
        );

        // P1: valid-time-ordered resolution. May retire older priors AND/OR
        // set new_fact.retired_at when an existing newer fact bounds this one.
        let retired_ids = facts_store
            .resolve_contradiction(&mut new_fact)
            .context("smart_forgetting: resolve_contradiction")?;

        let source = fact_event_source(capture);
        let event = if let Some(supersedes_id) = retired_ids.first().copied() {
            // T-56: contradiction path. The `Update` event carries
            // both the retirement (via `supersedes_id`) AND the new
            // fact payload, so `replay` rebuilds the table state
            // without seeing a separate `fact` event for this row.
            // If `retired_ids` has more than one element (rare
            // multi-retire case), we only point at the first one
            // from the event log; the SQL update in
            // resolve_contradiction has already retired all of
            // them, so query-time state is correct, but replay only
            // re-retires the first. Tracked as a stretch case in
            // TASKS.md.
            Event::with_id(
                new_event_id,
                EventKind::Update(crate::event::UpdatePayload {
                    supersedes_id,
                    new_fact: new_payload.clone(),
                    extra: Map::new(),
                }),
                source,
            )
        } else {
            Event::with_id(new_event_id, EventKind::Fact(new_payload.clone()), source)
        };
        event_log
            .append(&event)
            .context("append derived fact / update event")?;
        facts_store.insert(&new_fact).context("insert fact row")?;

        // Journal each retirement so `localmem journal` shows the
        // contradiction history. Action = Update mirrors what the
        // policy enum already allows; rule = "smart_forgetting" so
        // operators can filter the journal for contradiction
        // events specifically.
        for retired_id in &retired_ids {
            let entry = JournalEntry {
                ts: Utc::now(),
                action: PolicyAction::Update,
                rule: "smart_forgetting".into(),
                input_id: new_event_id,
                reasoning: Some(format!(
                    "retired {retired_id}: subject={} predicate={} new_object={:?}",
                    new_payload.subject, new_payload.predicate, new_payload.object,
                )),
            };
            journal
                .append(&entry)
                .context("journal smart_forgetting retirement")?;
        }

        count += 1;
    }
    Ok(count)
}

/// Build the `FactPayload` for a single extractor hit. Pure data, no
/// event-shell wrapping — the contradiction path needs the payload
/// to embed inside an `Update`, and the no-contradiction path wraps
/// it in a `fact` event. Extracted so both paths share the
/// kind/tag inheritance logic.
fn build_fact_payload(capture: &Event, ef: &crate::extractor::ExtractedFact) -> FactPayload {
    // T-52: inherit the source capture's kind so smart forgetting
    // (T-56) can apply the decision-never-retires rule, and so
    // profile rendering groups facts by their semantic category.
    let (inherited_tags, inherited_kind, valid_from) = match &capture.kind {
        EventKind::Capture(p) => (
            p.tags.clone(),
            p.kind.clone(),
            // P1/P0.6: source valid_from from the temporal envelope so an
            // imported fact carries its ORIGINAL instant, not import time.
            p.effective_capture_instant(capture.ts),
        ),
        _ => (BTreeMap::new(), crate::kind::Kind::default(), capture.ts),
    };
    FactPayload {
        subject: ef.subject.clone(),
        predicate: ef.predicate.clone(),
        object: ef.object.clone(),
        confidence: ef.confidence,
        valid_from,
        valid_to: None,
        derived_from: vec![capture.id],
        kind: inherited_kind,
        tags: inherited_tags,
        extra: Map::new(),
    }
}

/// Source for a derived fact event: inherits the originating
/// capture's app/host/user so the audit trail shows which tool led
/// to the fact.
fn fact_event_source(capture: &Event) -> Source {
    Source {
        app: capture.source.app.clone(),
        host: capture.source.host.clone(),
        user: capture.source.user.clone(),
    }
}

/// Pull the last `n` events from the log, excluding the candidate's own
/// id (it has just been appended). Used to feed the policy's recent
/// context window. Synchronous because the log is local and small.
fn load_recent(
    log: &EventLog,
    n: usize,
    candidate: crate::event_id::EventId,
) -> Result<Vec<Event>> {
    let mut buf: Vec<Event> = Vec::with_capacity(n.saturating_add(1));
    for ev in log.iter()? {
        let ev = ev?;
        if ev.id == candidate {
            continue;
        }
        if buf.len() == n {
            buf.remove(0);
        }
        buf.push(ev);
    }
    Ok(buf)
}

fn read_content(arg: Option<&str>) -> Result<String> {
    if let Some(s) = arg {
        return Ok(s.to_string());
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("read content from stdin")?;
    Ok(buf.trim_end_matches(['\n', '\r']).to_string())
}

fn emit(out: &WriteOutput, as_json: bool) -> Result<()> {
    if as_json {
        let mut body = serde_json::json!({
            "ok": true,
            "event_id": out.event_id,
            "action": out.action,
            "facts_extracted": out.facts_extracted,
        });
        if let Some(rewritten) = &out.rewritten_text {
            body.as_object_mut()
                .expect("json! produced an object")
                .insert(
                    "rewritten_text".into(),
                    serde_json::Value::String(rewritten.clone()),
                );
        }
        println!("{body}");
    } else {
        println!(
            "event_id={} action={} facts_extracted={}",
            out.event_id, out.action, out.facts_extracted
        );
        if let Some(rewritten) = &out.rewritten_text {
            println!("rewritten_text={rewritten:?}");
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
    use crate::event_log::EVENTS_FILE;
    use crate::facts::FactsStore;
    use tempfile::tempdir;

    /// Disable embedder lookup so tests run fast and offline.
    fn force_no_embedder() {
        std::env::set_var("LOCALMEM_MODEL_DIR", "/this/path/does/not/exist");
    }
    fn restore_embedder_env() {
        std::env::remove_var("LOCALMEM_MODEL_DIR");
    }

    #[test]
    fn build_fact_payload_sources_valid_from_from_temporal_envelope() {
        use crate::temporal::TimeEnvelope;
        // An imported capture whose original instant differs from the
        // event-shell ts (which is write/import time).
        let original = chrono::DateTime::<chrono::Utc>::from_timestamp(1_600_000_000, 0).unwrap();
        let mut cp = CapturePayload {
            text: "user uses SQLite".into(),
            ..Default::default()
        };
        cp.time = Some(TimeEnvelope::from_instant(original));
        let ev = Event::new(
            EventKind::Capture(cp),
            Source {
                app: "test".into(),
                host: "h".into(),
                user: None,
            },
        );
        // ev.ts is ~now, distinct from `original`.
        assert_ne!(ev.ts, original);
        let ef = crate::extractor::ExtractedFact {
            subject: "user".into(),
            predicate: "uses".into(),
            object: "SQLite".into(),
            confidence: 0.8,
        };
        let payload = build_fact_payload(&ev, &ef);
        assert_eq!(
            payload.valid_from, original,
            "fact valid_from must come from the capture's temporal envelope, not write-time ts"
        );
    }

    #[tokio::test]
    async fn write_appends_capture_and_journals_decision() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        force_no_embedder();
        let out = write_capture(
            tmp.path(),
            "I prefer functional Rust and avoid macros where possible.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::default(),
        )
        .await
        .unwrap();
        restore_embedder_env();
        assert!(!out.event_id.is_empty());
        assert_eq!(out.action, "COMMIT");

        // events.jsonl now has at least the capture event.
        let raw = std::fs::read_to_string(tmp.path().join(EVENTS_FILE)).unwrap();
        assert!(raw.contains(&out.event_id));

        // Journal has one entry.
        let journal = Journal::open(tmp.path()).unwrap();
        let entries: Vec<_> = journal.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn write_skips_short_capture_per_default_policy() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        force_no_embedder();
        let out = write_capture(
            tmp.path(),
            "ok",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::default(),
        )
        .await
        .unwrap();
        restore_embedder_env();
        // Default policy's high_signal rule requires min_content_length=20,
        // so a 2-char capture skips. No facts get extracted on skip.
        assert_eq!(out.action, "SKIP");
        assert_eq!(out.facts_extracted, 0);
    }

    #[tokio::test]
    async fn write_extracts_facts_on_commit() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        force_no_embedder();
        // Phrase that triggers the rule extractor.
        let out = write_capture(
            tmp.path(),
            "I prefer functional Rust over OO ceremony any day.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::default(),
        )
        .await
        .unwrap();
        restore_embedder_env();
        assert_eq!(out.action, "COMMIT");
        assert!(
            out.facts_extracted >= 1,
            "extractor should find at least one fact, got {}",
            out.facts_extracted
        );
        // Both the capture event and the derived fact event(s) land in the log.
        let log = EventLog::open(tmp.path()).unwrap();
        let events: Vec<_> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        assert!(events.len() >= 2);
    }

    #[tokio::test]
    async fn write_then_search_lex_returns_the_capture() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        force_no_embedder();
        let out = write_capture(
            tmp.path(),
            "stripe webhook signature verification fails on re-encoded body",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::default(),
        )
        .await
        .unwrap();
        // Drop any in-flight writer locks before re-opening for search.
        restore_embedder_env();

        let idx = LexicalIndex::open(tmp.path()).unwrap();
        let hits = idx.search("stripe webhook", 5, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event_id, out.event_id);
    }

    #[tokio::test]
    async fn empty_content_writes_a_skip_event_without_panicking() {
        // The CLI-layer `run()` rejects empty content before calling
        // `write_capture`, but the inner function still runs without
        // panicking when fed an empty string: the default policy SKIPs it
        // and we get a journal entry. This test pins that behavior so we
        // don't accidentally regress to a panic on the inner code path.
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        force_no_embedder();
        let out = write_capture(
            tmp.path(),
            "",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::default(),
        )
        .await
        .unwrap();
        restore_embedder_env();
        assert_eq!(out.action, "SKIP");
        assert_eq!(out.facts_extracted, 0);
    }

    #[tokio::test]
    async fn try_post_write_returns_none_when_no_server() {
        // Probing a port that nothing is bound to must return None within
        // the short timeout budget (~200ms) so the CLI falls back fast.
        // We use port 1 because it's privileged and reliably refuses
        // connections from a non-root test process.
        let out = try_post_write(
            "127.0.0.1:1",
            "anything",
            "test",
            &BTreeMap::new(),
            &crate::kind::Kind::default(),
            None,
        )
        .await;
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn write_capture_at_pins_valid_time_to_as_of() {
        // T-113: `localmem write --as-of <past>` must stamp the capture's
        // valid-time to that instant, not write-time, so bitemporal recall
        // resolves it correctly. This is the in-process twin of the server
        // /write `as_of` integration test.
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        force_no_embedder();
        let as_of = chrono::DateTime::parse_from_rfc3339("2021-03-04T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        write_capture_at(
            tmp.path(),
            "We migrated the billing service to Postgres.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::default(),
            Some(as_of),
        )
        .await
        .unwrap();
        restore_embedder_env();

        let log = EventLog::open(tmp.path()).unwrap();
        let events: Vec<Event> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        let (payload, ev_ts) = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::Capture(p) => Some((p, e.ts)),
                _ => None,
            })
            .expect("a capture event was written");
        assert_eq!(
            payload.effective_capture_instant(ev_ts),
            as_of,
            "capture valid-time must be the --as-of instant, not write-time"
        );
    }

    #[tokio::test]
    async fn write_persists_tags_on_capture_event() {
        // T-51 acceptance: tags supplied to write_capture round-trip into
        // the capture event's payload and become queryable via the lex
        // tag filter we built into LexicalIndex::search.
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        force_no_embedder();
        let mut tags = BTreeMap::new();
        tags.insert("project".into(), "localmem".into());
        tags.insert("topic".into(), "tags".into());
        let out = write_capture(
            tmp.path(),
            "rust async runtime notes for the localmem project",
            "test",
            tags.clone(),
            crate::kind::Kind::default(),
        )
        .await
        .unwrap();
        restore_embedder_env();
        assert_eq!(out.action, "COMMIT");

        // Filtered lex search finds the capture.
        let idx = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let hits = idx.search("rust async", 10, Some(&tags)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event_id, out.event_id);

        // Filter for a tag the capture doesn't have returns nothing.
        let mut wrong = BTreeMap::new();
        wrong.insert("project".into(), "other".into());
        let no_hits = idx.search("rust async", 10, Some(&wrong)).unwrap();
        assert!(no_hits.is_empty());
    }

    // ---- T-55: rewriter wired into the write pipeline ----

    fn write_config(home: &std::path::Path, body: &str) {
        std::fs::write(home.join("config.toml"), body).unwrap();
    }

    #[tokio::test]
    async fn rewriter_none_leaves_text_untouched_and_field_absent() {
        // Default config = none mode. The capture event must NOT
        // carry a rewritten_text field (asserts the wire shape stays
        // v0.1-compatible for unrewritten captures).
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        force_no_embedder();
        let out = write_capture(
            tmp.path(),
            "I prefer functional Rust over OO ceremony any day.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::default(),
        )
        .await
        .unwrap();
        restore_embedder_env();
        assert!(
            out.rewritten_text.is_none(),
            "none mode must not set rewritten_text"
        );
        // Lex stores the original text since there's no rewrite.
        let idx = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let hits = idx.search("functional Rust", 5, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event_id, out.event_id);
        assert!(
            hits[0].snippet.to_lowercase().contains("i prefer"),
            "lex snippet should retain the original first-person text"
        );
    }

    #[tokio::test]
    async fn rewriter_regex_substitutes_pronouns_and_lex_indexes_rewrite() {
        // With [rewriter].mode = "regex" and an explicit user_name,
        // "I prefer X" becomes "Vijay prefer X" both on the capture
        // event AND in the lex snippet (proving lex indexes the
        // rewritten text).
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        write_config(
            tmp.path(),
            r#"
[home]
user_name = "Vijay"

[rewriter]
mode = "regex"
"#,
        );
        force_no_embedder();
        let out = write_capture(
            tmp.path(),
            "I prefer functional Rust over OO ceremony any day.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::default(),
        )
        .await
        .unwrap();
        restore_embedder_env();
        let rewritten = out
            .rewritten_text
            .as_ref()
            .expect("regex mode must produce a rewrite for first-person text");
        assert!(rewritten.contains("Vijay prefer"), "got: {rewritten}");
        assert!(!rewritten.contains("I prefer"), "got: {rewritten}");

        // Lex now stores the rewritten text. A query against the
        // original wording also matches (because the rewriter only
        // changes pronouns; "functional Rust" survives intact), but
        // the SNIPPET returned must come from the rewritten string.
        let idx = LexicalIndex::open_reader_only(tmp.path()).unwrap();
        let hits = idx.search("functional Rust", 5, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("Vijay"),
            "lex snippet should reflect the rewritten text, got: {}",
            hits[0].snippet
        );
    }

    #[tokio::test]
    async fn rewriter_regex_with_no_pronouns_leaves_field_absent() {
        // Even with mode=regex, a text without pronouns produces no
        // change. The capture event must NOT carry rewritten_text in
        // that case (avoids duplicating the data on the wire).
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        write_config(
            tmp.path(),
            r#"
[home]
user_name = "Vijay"

[rewriter]
mode = "regex"
"#,
        );
        force_no_embedder();
        let out = write_capture(
            tmp.path(),
            "Stripe webhook signature verification failed on the re-encoded body.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::default(),
        )
        .await
        .unwrap();
        restore_embedder_env();
        assert!(
            out.rewritten_text.is_none(),
            "no-pronoun text should not produce a rewrite, got: {:?}",
            out.rewritten_text,
        );
    }

    #[tokio::test]
    async fn rewriter_local_llm_mode_falls_back_to_no_rewrite() {
        // local-llm mode is unsupported in v0.2 v1; the rewriter
        // bails. The write pipeline catches that and falls back to
        // text-only rather than failing the user's write.
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        write_config(
            tmp.path(),
            r#"
[home]
user_name = "Vijay"

[rewriter]
mode = "local-llm"
"#,
        );
        force_no_embedder();
        let out = write_capture(
            tmp.path(),
            "I prefer functional Rust over OO ceremony any day.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::default(),
        )
        .await
        .unwrap();
        restore_embedder_env();
        // Capture commits despite the rewriter being unsupported.
        assert_eq!(out.action, "COMMIT");
        // Field stays absent — graceful degradation, not silent
        // substitution of stale data.
        assert!(out.rewritten_text.is_none());
    }

    // ---- T-56: smart-forgetting end-to-end via the write pipeline ----

    #[tokio::test]
    async fn second_high_confidence_capture_retires_first_via_update_event() {
        // Two captures producing facts with the same
        // (subject, predicate); the second should emit an `Update`
        // event (not a `fact` event) and retire the first.
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        force_no_embedder();

        // First write: kind=preference so the extracted fact
        // inherits Kind::Preference (which allows contradiction
        // resolution per T-52). "I prefer functional Rust" emits
        // (user, prefers, functional Rust).
        let first = write_capture(
            tmp.path(),
            "I prefer functional Rust over OO ceremony any day.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::Preference,
        )
        .await
        .unwrap();
        assert_eq!(first.action, "COMMIT");

        // Second write: contradicts the first on (user, prefers).
        let second = write_capture(
            tmp.path(),
            "I prefer object-oriented Go for distributed systems work.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::Preference,
        )
        .await
        .unwrap();
        restore_embedder_env();
        assert_eq!(second.action, "COMMIT");

        // The event log now has: capture#1, fact#1, capture#2,
        // update#2. The update event carries the new fact payload
        // AND supersedes_id pointing at fact#1.
        let log = EventLog::open(tmp.path()).unwrap();
        let events: Vec<_> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        let update_count = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Update(_)))
            .count();
        let fact_count = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Fact(_)))
            .count();
        assert_eq!(
            update_count, 1,
            "second contradicting capture should emit one Update event, got {update_count}"
        );
        assert_eq!(
            fact_count, 1,
            "only the first capture should emit a Fact event (second becomes Update), got {fact_count}"
        );

        // Live facts at "now + epsilon": exactly the second fact.
        let store = FactsStore::open(tmp.path()).unwrap();
        let live = store.facts_at_time("user", Utc::now()).unwrap();
        assert_eq!(live.len(), 1, "only the new fact should be live");
        assert_eq!(
            live[0].object,
            "object-oriented Go for distributed systems work"
        );

        // Journal records the contradiction with rule="smart_forgetting".
        let journal = Journal::open(tmp.path()).unwrap();
        let entries: Vec<_> = journal.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        let smart_forgetting = entries
            .iter()
            .filter(|e| e.rule == "smart_forgetting")
            .count();
        assert_eq!(
            smart_forgetting, 1,
            "smart_forgetting retirement should produce exactly one journal entry"
        );
    }

    #[tokio::test]
    async fn decision_capture_does_not_retire_prior_preference_on_same_predicate() {
        // Spec: "Decision kind is append-only" — even when a new
        // capture with the same (subject, predicate) arrives, if
        // the new fact's kind is Decision, the prior fact stays
        // live.
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        force_no_embedder();
        write_capture(
            tmp.path(),
            "I prefer Postgres for transactional workloads in most cases.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::Preference,
        )
        .await
        .unwrap();
        write_capture(
            tmp.path(),
            "I prefer DuckDB for analytics workloads on local files.",
            "test",
            BTreeMap::new(),
            crate::kind::Kind::Decision,
        )
        .await
        .unwrap();
        restore_embedder_env();

        // Both facts should be live: the Decision did not retire
        // the prior Preference.
        let store = FactsStore::open(tmp.path()).unwrap();
        let live = store.facts_at_time("user", Utc::now()).unwrap();
        assert_eq!(
            live.len(),
            2,
            "Decision-kind new fact must NOT retire the prior Preference, got: {live:?}"
        );

        // The event log should contain TWO `fact` events (no Update).
        let log = EventLog::open(tmp.path()).unwrap();
        let events: Vec<_> = log.iter().unwrap().collect::<Result<Vec<_>>>().unwrap();
        let updates = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Update(_)))
            .count();
        assert_eq!(updates, 0, "Decision should not produce an Update event");
    }
}
