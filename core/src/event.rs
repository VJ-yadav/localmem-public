//! Event log schema.
//!
//! Every entry in `events.jsonl` is one [`Event`]. The on-wire JSON shape is
//! defined in ARCHITECTURE.md ("Event log schema" section) and is the
//! versioned, append-only source of truth for all derived stores.
//!
//! Design notes:
//! - `kind` + `payload` are adjacently tagged via serde so the JSON has a
//!   flat `{"kind": "capture", "payload": {...}}` shape, matching the spec.
//! - Each payload struct has a flattened `extra` map that round-trips unknown
//!   fields. This is the forward-compat property: a v0.1 binary can read
//!   and re-emit a v0.2 event without losing future-added fields.
//! - `version` lets us evolve the envelope itself when needed. v0.1 emits
//!   `version: 1`. Migration is a read-time forward function in `replay`.

use crate::event_id::EventId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Current event schema version emitted by this binary.
pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: EventId,
    pub ts: DateTime<Utc>,

    #[serde(flatten)]
    pub kind: EventKind,

    pub source: Source,

    #[serde(default = "default_version")]
    pub version: u32,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

impl Event {
    /// Convenience: build a new event with a fresh ULID and current UTC time.
    // T-04 (event log writer) will use this. Until then it is referenced
    // only by tests, so silence the dead-code lint without dropping the API.
    #[allow(dead_code)]
    pub fn new(kind: EventKind, source: Source) -> Self {
        Self::with_id(EventId::new(), kind, source)
    }

    /// Build an event with a caller-supplied id. Needed by T-56 smart
    /// forgetting: the write pipeline allocates the new fact's
    /// id BEFORE deciding whether to emit it as a `fact` event or an
    /// `update` event, because the DuckDB row's primary key has to
    /// match whichever event ends up in the log so `replay` rebuilds
    /// it correctly.
    pub fn with_id(id: EventId, kind: EventKind, source: Source) -> Self {
        Self {
            id,
            ts: Utc::now(),
            kind,
            source,
            version: CURRENT_VERSION,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "payload", rename_all = "lowercase")]
pub enum EventKind {
    /// Raw input from a tool or user.
    Capture(CapturePayload),
    /// A normalized claim derived from captures.
    Fact(FactPayload),
    /// Supersede an earlier fact with new info.
    Update(UpdatePayload),
    /// T-52b: mutate metadata on an earlier capture without rewriting
    /// its content or its derived facts. Used for the todo `done`
    /// lifecycle: a separate event keeps `events.jsonl` append-only
    /// and lets `localmem replay` reconstruct the latest state by
    /// walking the log. Distinct from [`Update`], which supersedes a
    /// *fact* (smart forgetting, T-56).
    UpdateCapture(UpdateCapturePayload),
    /// Soft-delete: hidden from queries, still in log.
    Forget(ForgetPayload),
    /// A policy decision (commit / dedup / skip / forget).
    Policy(PolicyPayload),
    /// Bulk ingest from another system.
    Import(ImportPayload),
    /// Layer 2 understanding (the unified memory-layer design): the LLM's
    /// decomposition of a capture into a summary + intent + typed entities,
    /// derived asynchronously OFF the write path. Append-only and immutable
    /// like every event: re-running a better model emits a NEW understanding
    /// event rather than mutating this one. Facts extracted in the same pass
    /// are emitted separately as `Fact`/`Update` events.
    Understanding(UnderstandingPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Source {
    pub app: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CapturePayload {
    pub text: String,
    /// Closed-core kind taxonomy (T-52). Defaults to
    /// [`crate::kind::Kind::Note`] when the field is absent. Six
    /// canonical kinds plus a catch-all `Other(String)` for
    /// extension kinds the binary doesn't know about. The
    /// `skip_serializing_if` keeps the wire shape v0.1-compatible
    /// for the implicit-Note default — only non-default kinds
    /// appear on the wire.
    #[serde(default, skip_serializing_if = "crate::kind::Kind::is_note")]
    pub kind: crate::kind::Kind,
    /// Context-rewritten text (T-55, v0.2). Filled when the
    /// configured rewriter produced a self-contained version of
    /// `text`. The original `text` is preserved verbatim for audit;
    /// `rewritten_text` is what the lex + vec indexes consume and
    /// what `localmem search` returns in its snippet. Absent when
    /// the rewriter is in `none` mode or returned the input
    /// unchanged.
    ///
    /// Backward-compat: legacy v0.1 captures (and v0.2 captures
    /// written before rewriting was enabled) have this field unset;
    /// the read path treats unset as "use `text`". The
    /// `skip_serializing_if` keeps the wire shape byte-identical for
    /// rewriter-disabled writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewritten_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    /// Container tags (v0.2). User-defined `key=value` metadata for
    /// scoping memories to projects, topics, clients, etc. Two reserved
    /// keys carry behavioral semantics (see SPEC_V0_2 "container-tag
    /// model"); all other keys are arbitrary.
    ///
    /// BTreeMap (vs HashMap) keeps deterministic key order so serialized
    /// events are byte-identical across runs, which the golden tests in
    /// this module rely on.
    ///
    /// Backward-compat: legacy events without a `tags` field deserialize
    /// to an empty map; the `skip_serializing_if` keeps the wire shape
    /// identical for untagged captures so v0.1 fixtures remain valid.
    /// An old binary that reads a new event with tags collects them into
    /// `extra` (via the flattened catch-all below), preserving content;
    /// when a new binary then reads that re-emitted event, `tags` takes
    /// priority over `extra` so the typed field is restored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    /// Temporal envelope (timezone-correct temporal envelope). Timezone-correct,
    /// precision-aware time. Native captures fill it fully; imports fill what
    /// the source provides. Absent on legacy v0.1/v0.2 captures and on native
    /// captures written before this shipped; the read path falls back to the
    /// event envelope `ts`. `skip_serializing_if` keeps the wire shape
    /// byte-identical when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<crate::temporal::TimeEnvelope>,
    /// Forward-compat: round-trips unknown fields.
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl CapturePayload {
    /// Text every read path should index against and surface in
    /// search snippets: `rewritten_text` when present, else `text`.
    /// Centralised so the lex indexer, the vec embedder, and the
    /// hybrid-retriever snippet path share one source of truth on
    /// "what the user actually sees."
    pub fn indexable_text(&self) -> &str {
        self.rewritten_text.as_deref().unwrap_or(&self.text)
    }

    /// The capture's effective UTC instant: the temporal envelope's instant
    /// when present (timezone-correct and recomputable), else `fallback` (the
    /// event-shell `ts`). Derived facts source their `valid_from` from this so
    /// the envelope, not the raw write-time `ts`, is the canonical time. For a
    /// native capture the two coincide; for an import the envelope carries the
    /// original instant while `ts` would be import time.
    pub fn effective_capture_instant(&self, fallback: DateTime<Utc>) -> DateTime<Utc> {
        self.time
            .as_ref()
            .map(|t| t.effective_instant())
            .unwrap_or(fallback)
    }

    /// Whether this capture is short-lived working memory (a tool-use trace),
    /// recognized either by an `ephemeral:*` retention tag (the hooks' semantic
    /// "short-lived" marker) or the `trace` sub-kind. Ephemeral captures stay
    /// first-class in the event log for audit/replay, but they must NOT seed the
    /// durable intelligence layer: the understanding worker skips decomposing
    /// them, and `replay` skips re-materializing any facts derived from them, so
    /// command/file-path noise never reaches the facts store, graph, or profile.
    /// One source of truth for both code paths.
    pub fn is_ephemeral(&self) -> bool {
        self.tags
            .get(crate::reserved_tags::KEY_RETENTION)
            .is_some_and(|r| r.starts_with(crate::reserved_tags::RETENTION_EPHEMERAL_PREFIX))
            || self.kind.as_str() == "trace"
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FactPayload {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f64,
    pub valid_from: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_from: Vec<EventId>,
    /// Kind inherited from the source capture (T-52). T-56 reads
    /// this to decide whether a contradiction resolution applies:
    /// decisions are append-only, everything else can retire prior
    /// rows. Same backward-compat discipline as
    /// [`CapturePayload::kind`].
    #[serde(default, skip_serializing_if = "crate::kind::Kind::is_note")]
    pub kind: crate::kind::Kind,
    /// Container tags inherited from the source capture at extraction
    /// time (T-51b). Stored on the fact event so `localmem replay` can
    /// reconstruct the facts table's `tags` column without joining back
    /// to the capture. Same backward-compat discipline as
    /// [`CapturePayload::tags`]: absent on the wire when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdatePayload {
    pub supersedes_id: EventId,
    pub new_fact: FactPayload,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// T-52b: payload for [`EventKind::UpdateCapture`]. Currently the
/// only mutable field is `done` (carrying the todo lifecycle flag);
/// the struct is open via `extra` so future per-capture mutable
/// metadata (pinned, archived, etc.) can land additively without
/// new event kinds. Every field except `target_id` is optional so
/// an event can update one slot without zeroing the others — the
/// indexer reads the latest non-`None` value per slot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateCapturePayload {
    pub target_id: EventId,
    /// New `done` state for a todo capture. `None` leaves the
    /// current state untouched (e.g. when a future field on this
    /// payload is what's being changed). `Some(true)` marks the
    /// todo complete; `Some(false)` reopens it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
    /// Optional human-readable reason, surfaced by `localmem audit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForgetPayload {
    pub target_id: EventId,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyPayload {
    pub rule: String,
    pub input_id: EventId,
    pub action: PolicyAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum PolicyAction {
    Commit,
    Update,
    Dedup,
    Skip,
    Forget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportPayload {
    pub source_format: String,
    pub count: u64,
    pub batch_id: String,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// A typed entity surfaced by the understanding pass. `kind` is an OPEN label
/// (person, project, tool, org, concept, ...): the set is meant to grow, so it
/// is never a closed enum. Defined here (not reused from the `understanding`
/// module) so the event schema stays self-contained on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnderstoodEntity {
    pub name: String,
    pub kind: String,
}

/// Payload for [`EventKind::Understanding`]: the derived meaning of one capture.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnderstandingPayload {
    /// The capture this understanding was derived from.
    pub source_id: EventId,
    /// One or two sentences capturing the gist. May be empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// What the user was trying to do (imperative phrase). May be empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub intent: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<UnderstoodEntity>,
    /// Concrete anchors the capture mentions (file paths, IDs, URLs), so memory
    /// "knows where things live" without re-reading the raw text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    /// Open salience label (`decision`, `rule`, `preference`, `question`,
    /// `note`, ...) so retrieval/briefing can rank signal over chatter. Absent
    /// on the wire when it's the default `note`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub salience: String,
    /// The model that produced this understanding (provenance + recomputability:
    /// a future better model emits a newer understanding event).
    pub model: String,
    /// Valid-time inherited from the source capture's temporal envelope, so the
    /// understanding sorts on the same instant as the capture and its facts.
    pub valid_from: DateTime<Utc>,
    /// Container tags inherited from the source capture, so understandings can
    /// be scoped to a project without joining back to the capture.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn fixed_id() -> EventId {
        // A known ULID for deterministic golden tests.
        "01HXY00000000000000000000Z".parse().unwrap()
    }

    fn fixed_ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 14, 12, 34, 56).unwrap()
    }

    fn fixed_source() -> Source {
        Source {
            app: "claude-code".into(),
            host: "studio.local".into(),
            user: Some("alice".into()),
        }
    }

    #[test]
    fn capture_event_roundtrips() {
        let ev = Event {
            id: fixed_id(),
            ts: fixed_ts(),
            kind: EventKind::Capture(CapturePayload {
                time: None,
                text: "I prefer functional Rust.".into(),
                rewritten_text: None,
                mime: Some("text/plain".into()),
                kind: Default::default(),
                attachments: vec![],
                tags: Default::default(),
                extra: Map::new(),
            }),
            source: fixed_source(),
            version: 1,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            json,
            json!({
                "id": "01HXY00000000000000000000Z",
                "ts": "2026-05-14T12:34:56Z",
                "kind": "capture",
                "payload": {
                    "text": "I prefer functional Rust.",
                    "mime": "text/plain"
                },
                "source": {
                    "app": "claude-code",
                    "host": "studio.local",
                    "user": "alice"
                },
                "version": 1
            })
        );
        let parsed: Event = serde_json::from_value(json).unwrap();
        assert_eq!(ev, parsed);
    }

    #[test]
    fn fact_event_roundtrips() {
        let ev = Event {
            id: fixed_id(),
            ts: fixed_ts(),
            kind: EventKind::Fact(FactPayload {
                subject: "user".into(),
                predicate: "prefers".into(),
                object: "functional Rust".into(),
                confidence: 0.8,
                valid_from: fixed_ts(),
                valid_to: None,
                derived_from: vec![fixed_id()],
                kind: Default::default(),
                tags: Default::default(),
                extra: Map::new(),
            }),
            source: fixed_source(),
            version: 1,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, parsed);
    }

    #[test]
    fn forget_event_roundtrips() {
        let ev = Event::new(
            EventKind::Forget(ForgetPayload {
                target_id: fixed_id(),
                reason: "contradicted by newer fact".into(),
                scope: None,
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let s = serde_json::to_string(&ev).unwrap();
        let parsed: Event = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, parsed);
    }

    #[test]
    fn policy_event_roundtrips() {
        let ev = Event::new(
            EventKind::Policy(PolicyPayload {
                rule: "high_signal".into(),
                input_id: fixed_id(),
                action: PolicyAction::Commit,
                reasoning: Some("single declarative preference".into()),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let s = serde_json::to_string(&ev).unwrap();
        let parsed: Event = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, parsed);

        // Each PolicyAction value round-trips.
        for action in [
            PolicyAction::Commit,
            PolicyAction::Update,
            PolicyAction::Dedup,
            PolicyAction::Skip,
            PolicyAction::Forget,
        ] {
            let s = serde_json::to_string(&action).unwrap();
            let back: PolicyAction = serde_json::from_str(&s).unwrap();
            assert_eq!(action, back);
        }
    }

    #[test]
    fn import_event_roundtrips() {
        let ev = Event::new(
            EventKind::Import(ImportPayload {
                source_format: "chatgpt".into(),
                count: 1234,
                batch_id: "import-2026-05-14".into(),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let s = serde_json::to_string(&ev).unwrap();
        let parsed: Event = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, parsed);
    }

    #[test]
    fn update_event_roundtrips() {
        let fact = FactPayload {
            subject: "user".into(),
            predicate: "lives_in".into(),
            object: "Tokyo".into(),
            confidence: 0.9,
            valid_from: fixed_ts(),
            valid_to: None,
            derived_from: vec![],
            kind: Default::default(),
            tags: Default::default(),
            extra: Map::new(),
        };
        let ev = Event::new(
            EventKind::Update(UpdatePayload {
                supersedes_id: fixed_id(),
                new_fact: fact,
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let s = serde_json::to_string(&ev).unwrap();
        let parsed: Event = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, parsed);
    }

    #[test]
    fn understanding_event_roundtrips_and_omits_empty_optionals() {
        let ev = Event {
            id: fixed_id(),
            ts: fixed_ts(),
            kind: EventKind::Understanding(UnderstandingPayload {
                source_id: fixed_id(),
                summary: "Vijay picked LanceDB for localmem's vectors.".into(),
                intent: "record a decision".into(),
                entities: vec![UnderstoodEntity {
                    name: "LanceDB".into(),
                    kind: "tool".into(),
                }],
                references: vec!["core/src/vectors.rs".into()],
                salience: "decision".into(),
                model: "llama3.2:latest".into(),
                valid_from: fixed_ts(),
                tags: Default::default(),
                extra: Map::new(),
            }),
            source: fixed_source(),
            version: 1,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["kind"], "understanding");
        // Empty tags must not appear on the wire (byte-compat discipline).
        assert!(json["payload"].get("tags").is_none());
        let parsed: Event = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(ev, parsed);

        // A minimal understanding (no summary/intent/entities) round-trips with
        // those fields absent on the wire.
        let minimal = Event::new(
            EventKind::Understanding(UnderstandingPayload {
                source_id: fixed_id(),
                summary: String::new(),
                intent: String::new(),
                entities: vec![],
                references: vec![],
                salience: String::new(),
                model: "llama3.2:latest".into(),
                valid_from: fixed_ts(),
                tags: Default::default(),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let mjson = serde_json::to_value(&minimal).unwrap();
        assert!(mjson["payload"].get("summary").is_none());
        assert!(mjson["payload"].get("entities").is_none());
        let mparsed: Event =
            serde_json::from_str(&serde_json::to_string(&minimal).unwrap()).unwrap();
        assert_eq!(minimal, mparsed);
    }

    #[test]
    fn capture_kind_roundtrips_and_omits_when_note() {
        // Default (Kind::Note) must NOT appear on the wire so v0.1
        // fixtures round-trip byte-identically. Non-Note kinds do
        // appear.
        let default_kind = Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
                text: "hi".into(),
                kind: crate::kind::Kind::Note,
                rewritten_text: None,
                mime: None,
                attachments: vec![],
                tags: BTreeMap::new(),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let json = serde_json::to_value(&default_kind).unwrap();
        assert!(
            json["payload"].get("kind").is_none(),
            "default Note kind must serialize as absent, got: {json}",
        );

        // Each canonical non-Note kind survives the round trip.
        for variant in [
            crate::kind::Kind::Fact,
            crate::kind::Kind::Preference,
            crate::kind::Kind::Decision,
            crate::kind::Kind::Constraint,
            crate::kind::Kind::Todo,
            crate::kind::Kind::Other("recipe".into()),
        ] {
            let ev = Event::new(
                EventKind::Capture(CapturePayload {
                    time: None,
                    text: "hi".into(),
                    kind: variant.clone(),
                    rewritten_text: None,
                    mime: None,
                    attachments: vec![],
                    tags: BTreeMap::new(),
                    extra: Map::new(),
                }),
                fixed_source(),
            );
            let s = serde_json::to_string(&ev).unwrap();
            let parsed: Event = serde_json::from_str(&s).unwrap();
            let EventKind::Capture(p) = &parsed.kind else {
                panic!("expected capture")
            };
            assert_eq!(
                p.kind, variant,
                "kind {variant:?} did not round-trip via {s}",
            );
        }
    }

    #[test]
    fn capture_rewritten_text_roundtrips_and_omits_when_none() {
        // Backward-compat: a capture without rewritten_text must
        // serialize with NO `rewritten_text` key at all, so v0.1
        // fixtures stay byte-identical.
        let none_rewrite = Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
                text: "I prefer Rust.".into(),
                rewritten_text: None,
                mime: None,
                kind: Default::default(),
                attachments: vec![],
                tags: BTreeMap::new(),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let json = serde_json::to_value(&none_rewrite).unwrap();
        assert!(
            json["payload"].get("rewritten_text").is_none(),
            "absent rewrite must not appear on the wire, got: {json}",
        );

        // Populated rewrite must round-trip through serde unchanged.
        let with_rewrite = Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
                text: "I prefer Rust.".into(),
                rewritten_text: Some("Vijay prefer Rust.".into()),
                mime: None,
                kind: Default::default(),
                attachments: vec![],
                tags: BTreeMap::new(),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let s = serde_json::to_string(&with_rewrite).unwrap();
        let parsed: Event = serde_json::from_str(&s).unwrap();
        let EventKind::Capture(p) = &parsed.kind else {
            panic!("expected capture")
        };
        assert_eq!(p.text, "I prefer Rust.");
        assert_eq!(p.rewritten_text.as_deref(), Some("Vijay prefer Rust."));
    }

    #[test]
    fn indexable_text_returns_rewritten_when_present_else_text() {
        let only_text = CapturePayload {
            text: "I prefer Rust.".into(),
            rewritten_text: None,
            ..Default::default()
        };
        assert_eq!(only_text.indexable_text(), "I prefer Rust.");

        let both = CapturePayload {
            text: "I prefer Rust.".into(),
            rewritten_text: Some("Vijay prefer Rust.".into()),
            ..Default::default()
        };
        assert_eq!(both.indexable_text(), "Vijay prefer Rust.");
    }

    #[test]
    fn old_binary_emitted_capture_deserializes_with_no_rewrite() {
        // A capture written by v0.1 (no rewritten_text field at all)
        // must deserialize cleanly with the field as None.
        let v1 = json!({
            "id": "01HXY00000000000000000000Z",
            "ts": "2026-05-14T12:34:56Z",
            "kind": "capture",
            "payload": {"text": "hi"},
            "source": {"app": "x", "host": "y"},
            "version": 1
        });
        let parsed: Event = serde_json::from_value(v1).unwrap();
        let EventKind::Capture(p) = &parsed.kind else {
            panic!("expected capture")
        };
        assert!(p.rewritten_text.is_none());
        assert_eq!(p.text, "hi");
    }

    #[test]
    fn capture_with_tags_roundtrips_and_omits_when_empty() {
        // Empty tags must NOT appear on the wire (backward-compat: v0.1
        // fixtures and any tooling that compares event JSON byte-for-byte
        // expects exactly the v0.1 shape when no tags are set).
        let untagged = Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
                text: "hi".into(),
                rewritten_text: None,
                mime: None,
                kind: Default::default(),
                attachments: vec![],
                tags: BTreeMap::new(),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let json = serde_json::to_value(&untagged).unwrap();
        assert!(
            json["payload"].get("tags").is_none(),
            "empty tags must serialize as absent, got: {json}"
        );

        // Populated tags must serialize as a flat object and round-trip
        // with BTreeMap order (so byte-identical serialization across
        // runs holds for golden tests).
        let mut tags = BTreeMap::new();
        tags.insert("project".into(), "localmem".into());
        tags.insert("topic".into(), "retrieval".into());
        let tagged = Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
                text: "hi".into(),
                rewritten_text: None,
                mime: None,
                kind: Default::default(),
                attachments: vec![],
                tags: tags.clone(),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let s = serde_json::to_string(&tagged).unwrap();
        // Project comes before topic in the BTreeMap, so the wire order
        // must reflect that. This pins the determinism guarantee.
        let project_pos = s.find("project").unwrap();
        let topic_pos = s.find("topic").unwrap();
        assert!(project_pos < topic_pos);
        let parsed: Event = serde_json::from_str(&s).unwrap();
        let EventKind::Capture(p) = &parsed.kind else {
            panic!("expected capture")
        };
        assert_eq!(p.tags, tags);
    }

    #[test]
    fn fact_with_tags_roundtrips_and_omits_when_empty() {
        // Same backward-compat discipline as captures: empty tags must
        // not appear on the wire, so v0.1 fact-event fixtures still
        // serialize byte-identically. Populated tags round-trip through
        // the typed field.
        let untagged = Event::new(
            EventKind::Fact(FactPayload {
                subject: "user".into(),
                predicate: "prefers".into(),
                object: "rust".into(),
                confidence: 0.7,
                valid_from: fixed_ts(),
                valid_to: None,
                derived_from: vec![fixed_id()],
                kind: Default::default(),
                tags: BTreeMap::new(),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let json = serde_json::to_value(&untagged).unwrap();
        assert!(
            json["payload"].get("tags").is_none(),
            "empty tags must serialize as absent on facts: {json}"
        );

        let mut tags = BTreeMap::new();
        tags.insert("project".into(), "localmem".into());
        let tagged = Event::new(
            EventKind::Fact(FactPayload {
                subject: "user".into(),
                predicate: "prefers".into(),
                object: "rust".into(),
                confidence: 0.7,
                valid_from: fixed_ts(),
                valid_to: None,
                derived_from: vec![fixed_id()],
                kind: Default::default(),
                tags: tags.clone(),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let s = serde_json::to_string(&tagged).unwrap();
        let parsed: Event = serde_json::from_str(&s).unwrap();
        let EventKind::Fact(p) = &parsed.kind else {
            panic!("expected fact")
        };
        assert_eq!(p.tags, tags);
    }

    #[test]
    fn old_binary_emitted_capture_deserializes_with_empty_tags() {
        // Simulate v0.1 output: a capture event with no `tags` field at
        // all. The new typed field must default to an empty map without
        // pulling unknown payload fields into it. The `extra` map stays
        // empty too because there are no unknowns.
        let v1 = json!({
            "id": "01HXY00000000000000000000Z",
            "ts": "2026-05-14T12:34:56Z",
            "kind": "capture",
            "payload": {
                "text": "hello"
            },
            "source": {"app": "x", "host": "y"},
            "version": 1
        });
        let parsed: Event = serde_json::from_value(v1).unwrap();
        let EventKind::Capture(p) = &parsed.kind else {
            panic!("expected capture")
        };
        assert!(p.tags.is_empty(), "expected empty tags by default");
        assert!(p.extra.is_empty(), "extra must not capture missing fields");
    }

    #[test]
    fn tags_take_priority_over_extra_when_both_present_in_wire_form() {
        // Defensive: a malformed source that puts `tags` at the payload
        // level (typed field) must NOT also leak into `extra` after the
        // round-trip. Otherwise a new binary reading an old binary's
        // re-serialization of a new event would double-store the tags.
        let v = json!({
            "id": "01HXY00000000000000000000Z",
            "ts": "2026-05-14T12:34:56Z",
            "kind": "capture",
            "payload": {
                "text": "hi",
                "tags": {"a": "1"}
            },
            "source": {"app": "x", "host": "y"},
            "version": 1
        });
        let parsed: Event = serde_json::from_value(v).unwrap();
        let EventKind::Capture(p) = &parsed.kind else {
            panic!("expected capture")
        };
        assert_eq!(p.tags.get("a").map(String::as_str), Some("1"));
        assert!(
            !p.extra.contains_key("tags"),
            "tags must not also appear in extra (would round-trip twice)"
        );
    }

    #[test]
    fn forward_compat_unknown_payload_fields_roundtrip() {
        // Simulate a v0.next event with extra fields. The current binary
        // must preserve them when re-serializing so we never silently lose
        // data. `tags` is no longer "unknown" (it's a typed field as of
        // v0.2), so the test uses other future fields here.
        let v_next = json!({
            "id": "01HXY00000000000000000000Z",
            "ts": "2026-05-14T12:34:56Z",
            "kind": "capture",
            "payload": {
                "text": "hello",
                "future_field": {"new": true},
                "future_modes": ["a", "b"]
            },
            "source": {"app": "claude-code", "host": "studio.local"},
            "version": 3
        });
        let parsed: Event = serde_json::from_value(v_next.clone()).unwrap();
        let re_emitted = serde_json::to_value(&parsed).unwrap();
        // Future fields preserved in the captured payload's `extra` map.
        let payload = re_emitted.get("payload").unwrap();
        assert_eq!(payload.get("future_field"), Some(&json!({"new": true})));
        assert_eq!(payload.get("future_modes"), Some(&json!(["a", "b"])));
        // Envelope version preserved.
        assert_eq!(re_emitted.get("version"), Some(&json!(3)));
    }

    #[test]
    fn unknown_kind_fails_to_deserialize() {
        // We do NOT silently accept unknown event kinds. A future kind must
        // be explicitly added to EventKind. The alternative (ignore unknown
        // kinds) would let downstream stores silently miss events.
        let unknown = json!({
            "id": "01HXY00000000000000000000Z",
            "ts": "2026-05-14T12:34:56Z",
            "kind": "telepathic_thought",
            "payload": {},
            "source": {"app": "x", "host": "y"},
            "version": 1
        });
        let result: Result<Event, _> = serde_json::from_value(unknown);
        assert!(result.is_err());
    }

    #[test]
    fn jsonl_line_round_trip() {
        // events.jsonl is one JSON object per line. The serializer must
        // produce output that has no internal newlines.
        let ev = Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
                text: "multi\nline\ntext".into(),
                rewritten_text: None,
                mime: None,
                kind: Default::default(),
                attachments: vec![],
                tags: Default::default(),
                extra: Map::new(),
            }),
            fixed_source(),
        );
        let line = serde_json::to_string(&ev).unwrap();
        assert!(
            !line.contains('\n'),
            "serialized line must not contain newlines"
        );
        // And the embedded text round-trips correctly.
        let parsed: Event = serde_json::from_str(&line).unwrap();
        if let EventKind::Capture(c) = parsed.kind {
            assert_eq!(c.text, "multi\nline\ntext");
        } else {
            panic!("expected capture");
        }
    }
}
