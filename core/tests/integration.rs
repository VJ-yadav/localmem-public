//! Black-box integration tests over controlled fixtures.
//!
//! These drive the REAL public flows end-to-end (replay -> facts as-of; import
//! -> dedup) through the `localmem` library API, rather than poking internals,
//! so they catch wiring bugs that unit tests miss (e.g. the kind of duplicate
//! drift T-63 fixed). Controlled fixtures assert exact behavior; the
//! adversarial fixtures (malformed / empty) assert robustness: clean error or
//! no-op, never a panic.

use std::path::Path;

fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write fixture");
}

/// Init a localmem home, drop in a controlled events.jsonl, and replay it
/// (the real rebuild path: replay re-runs valid-time resolution per fact).
async fn seed_and_replay(home: &Path, events_jsonl: &str) {
    localmem::cli::init::init_home(home).expect("init home");
    write(&home.join("events.jsonl"), events_jsonl);
    localmem::cli::replay::run(home.to_str(), /* json = */ false)
        .await
        .expect("replay");
}

fn utc(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .expect("parse rfc3339")
        .with_timezone(&chrono::Utc)
}

/// The moat, end-to-end through replay: two `(user, uses)` facts ingested OUT
/// OF ORDER (SQLite recent, older Postgres "imported" after). Replay must
/// resolve by VALID time so the as-of timeline is correct.
#[tokio::test]
async fn timeline_out_of_order_import_rebuilds_correct_as_of() {
    let tmp = tempfile::tempdir().unwrap();
    let events = concat!(
        r#"{"id":"01HXY0FACT000000000000000B","ts":"2026-06-01T00:00:00Z","kind":"fact","payload":{"subject":"user","predicate":"uses","object":"SQLite","confidence":0.9,"valid_from":"2024-05-01T00:00:00Z"},"source":{"app":"demo","host":"h"},"version":1}"#,
        "\n",
        r#"{"id":"01HXY0FACT000000000000000A","ts":"2026-06-02T00:00:00Z","kind":"fact","payload":{"subject":"user","predicate":"uses","object":"Postgres","confidence":0.9,"valid_from":"2024-03-01T00:00:00Z"},"source":{"app":"demo","host":"h"},"version":1}"#,
        "\n",
    );
    seed_and_replay(tmp.path(), events).await;

    let store = localmem::facts::FactsStore::open(tmp.path()).unwrap();

    // As-of April 2024: Postgres was current (valid Mar, superseded May).
    let april = store
        .facts_at_time("user", utc("2024-04-01T00:00:00Z"))
        .unwrap();
    assert_eq!(april.len(), 1, "exactly one belief current in April");
    assert_eq!(april[0].object, "Postgres");

    // As-of late 2024: SQLite is current; the older import did not clobber it.
    let now = store
        .facts_at_time("user", utc("2024-12-01T00:00:00Z"))
        .unwrap();
    assert_eq!(now.len(), 1, "exactly one belief current now");
    assert_eq!(now[0].object, "SQLite");
}

/// Replay is deterministic and idempotent: running it twice over the same log
/// yields the same as-of result (derived stores are recomputable).
#[tokio::test]
async fn replay_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let events = concat!(
        r#"{"id":"01HXY0FACT000000000000000A","ts":"2026-06-02T00:00:00Z","kind":"fact","payload":{"subject":"user","predicate":"uses","object":"Postgres","confidence":0.9,"valid_from":"2024-03-01T00:00:00Z"},"source":{"app":"demo","host":"h"},"version":1}"#,
        "\n",
    );
    seed_and_replay(tmp.path(), events).await;
    // Replay again over the unchanged log.
    localmem::cli::replay::run(tmp.path().to_str(), false)
        .await
        .expect("second replay");
    let store = localmem::facts::FactsStore::open(tmp.path()).unwrap();
    let now = store
        .facts_at_time("user", utc("2024-12-01T00:00:00Z"))
        .unwrap();
    assert_eq!(now.len(), 1);
    assert_eq!(now[0].object, "Postgres");
}

// --- import flow (controlled + adversarial fixtures) ---

const CHATGPT_EXPORT: &str = r#"[
  {"title":"Rust", "mapping": {
    "n1": {"message": {"author": {"role": "user"}, "create_time": 1700000000.0,
            "content": {"content_type": "text", "parts": ["I prefer functional Rust."]}}}
  }}
]"#;

#[test]
fn import_chatgpt_then_reimport_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    localmem::cli::init::init_home(tmp.path()).unwrap();
    let export = tmp.path().join("conversations.json");
    write(&export, CHATGPT_EXPORT);

    let s1 = localmem::import::chatgpt::import_chatgpt(tmp.path(), &export).unwrap();
    assert_eq!(s1.events_appended, 2, "marker + one user message");
    assert_eq!(s1.messages_deduped, 0);

    // Re-import the same export: the message dedupes, only the marker lands.
    let s2 = localmem::import::chatgpt::import_chatgpt(tmp.path(), &export).unwrap();
    assert_eq!(s2.messages_deduped, 1);
    assert_eq!(s2.events_appended, 1, "marker only, no duplicate capture");
}

#[test]
fn import_auto_detects_chatgpt() {
    let tmp = tempfile::tempdir().unwrap();
    let export = tmp.path().join("conversations.json");
    write(&export, CHATGPT_EXPORT);
    assert_eq!(
        localmem::import::detect_format(&export),
        Some("chatgpt"),
        "a ChatGPT export should auto-detect"
    );
}

#[test]
fn import_malformed_errors_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    localmem::cli::init::init_home(tmp.path()).unwrap();
    let bad = tmp.path().join("bad.json");
    write(&bad, "this is not json");
    // Errors (does not panic); the registry/CLI surfaces it.
    assert!(localmem::import::chatgpt::import_chatgpt(tmp.path(), &bad).is_err());
}

#[test]
fn import_empty_export_yields_marker_only() {
    let tmp = tempfile::tempdir().unwrap();
    localmem::cli::init::init_home(tmp.path()).unwrap();
    let empty = tmp.path().join("empty.json");
    write(&empty, "[]");
    let s = localmem::import::chatgpt::import_chatgpt(tmp.path(), &empty).unwrap();
    assert_eq!(s.events_appended, 1, "just the import marker");
    assert_eq!(s.messages_deduped, 0);
}
