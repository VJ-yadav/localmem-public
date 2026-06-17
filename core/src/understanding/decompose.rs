//! Per-capture decomposition: ONE raw capture -> a structured `Decomposition`
//! (a one-line summary, the user's intent, typed entities, and facts).
//!
//! This is Output A of the understanding layer. It is deliberately the
//! per-capture PRIMITIVE and nothing more: it does not build a user persona or
//! a session briefing. Those are corpus-level syntheses that aggregate many of
//! these decompositions, and live elsewhere. One prompt does not define a
//! person, so decompose never tries to.
//!
//! The model round-trip itself lives in the async worker. This module owns the
//! two pure, unit-testable halves of the contract:
//!
//!   * [`decompose_system_prompt`] - builds the instruction that forces strict
//!     JSON. Testable without a model.
//!   * [`parse_decomposition`] - parses the model's JSON content. The testable
//!     core, mirroring `local_llm::parse_facts`: tolerant, skips partial
//!     fields, never poisons the store on a malformed entry.
//!
//! Platform-neutral by construction: the only provenance it knows is an opaque
//! `source` string, surfaced to the model as context and never branched on.

use crate::extractor::local_llm::fact_from_value;
use crate::extractor::ExtractedFact;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;

/// A typed entity mentioned in a capture (a person, project, tool, concept,
/// org, ...). `kind` is an OPEN label, never a closed enum: the set of entity
/// kinds is meant to expand (e.g. each new AI tool is just another `tool`),
/// and CLAUDE.md forbids hardcoded enums. The model proposes the label; we
/// store it verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecomposedEntity {
    pub name: String,
    pub kind: String,
}

/// The structured understanding of a single capture. Every field is optional
/// in practice: a capture may yield a summary but no facts, or entities but no
/// clear intent. Empty fields are valid, not errors.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Decomposition {
    /// One or two sentences: the gist of the capture.
    pub summary: String,
    /// What the user is trying to DO, as an imperative phrase
    /// ("debug the embed worker", "decide between DuckDB and SQLite",
    /// "record a voice preference"). Empty when the capture has no clear intent.
    pub intent: String,
    pub entities: Vec<DecomposedEntity>,
    pub facts: Vec<ExtractedFact>,
    /// Concrete references the note mentions: file paths, IDs, URLs, ticket
    /// numbers. This is how the memory "knows every file location" — the
    /// navigable anchors an agent would jump to. Verbatim, deduped by the model.
    pub references: Vec<String>,
    /// Open salience label classifying the capture's importance/type
    /// (`decision`, `rule`, `preference`, `question`, `note`, ...). Lets the
    /// briefing/recall rank signal over chatter. Never a closed enum; defaults
    /// to `"note"` when the model gives nothing.
    pub salience: String,
}

/// Knobs for prompt construction. Kept tiny and config-derivable so nothing
/// platform-specific is baked into the code.
#[derive(Debug, Clone)]
pub struct DecomposeOptions {
    /// Canonical subject that facts ABOUT THE USER are attributed to, so the
    /// later persona synthesis can select `subject == user_subject`. Defaults
    /// to `"user"`; the persona dimensions themselves live in config
    /// (`policies/profile.yaml`), not here.
    pub user_subject: String,
    /// Opaque provenance (e.g. `"claude-code"`, `"cursor"`). Given to the model
    /// only as a hint; never used to branch logic. Open by design.
    pub source: Option<String>,
}

impl Default for DecomposeOptions {
    fn default() -> Self {
        Self {
            user_subject: "user".to_string(),
            source: None,
        }
    }
}

/// Build the system instruction that forces a strict-JSON decomposition.
///
/// Pure and model-free so it is assertable in tests. The `{user_subject}` and
/// optional `{source}` are interpolated; the JSON shape is fixed so
/// [`parse_decomposition`] can rely on it.
pub fn decompose_system_prompt(opts: &DecomposeOptions) -> String {
    let provenance = match opts.source.as_deref() {
        Some(s) if !s.trim().is_empty() => {
            format!(" The note was captured from `{}`.", s.trim())
        }
        _ => String::new(),
    };
    format!(
        "You decompose a short note from a user's work into structured \
         understanding.{provenance} Respond with ONLY JSON of the exact form \
         {{\"summary\":\"...\",\"intent\":\"...\",\
         \"entities\":[{{\"name\":\"...\",\"kind\":\"...\"}}],\
         \"facts\":[{{\"subject\":\"...\",\"predicate\":\"...\",\"object\":\"...\",\"confidence\":0.0}}],\
         \"references\":[\"...\"],\"salience\":\"...\"}}. \
         `summary` is one or two sentences capturing the gist. \
         `intent` is what the user is trying to DO, as a short imperative phrase; \
         use \"\" if there is no clear intent. \
         `entities` are the people, projects, tools, organizations, or concepts \
         named in the note; `kind` is a free-form lowercase label you choose \
         (for example person, project, tool, org, concept). \
         `facts` are only facts CLEARLY stated in the note; do not infer. \
         `subject` is the entity a fact is about, `predicate` the relation, \
         `object` the value, `confidence` your certainty in [0,1]. \
         For any fact about the user themselves, use the subject \"{subject}\". \
         `references` are concrete anchors the note mentions VERBATIM: file paths, \
         IDs, URLs, ticket numbers; empty array if none. \
         `salience` is one lowercase word for what this note IS \
         (decision, rule, preference, question, or note); use \"note\" if unsure. \
         If a field has nothing to fill it, use an empty string or empty array; \
         never invent content.",
        provenance = provenance,
        subject = opts.user_subject,
    )
}

/// Parse the model's JSON content string into a [`Decomposition`].
///
/// Pure and model-free: the testable core of the contract. Tolerant by design,
/// matching `local_llm::parse_facts`:
///   * a missing `summary`/`intent` defaults to `""`,
///   * an entity missing a `name` is skipped; a missing `kind` defaults to
///     `"thing"`,
///   * facts reuse the shared `fact_from_value` rules (skip-partial,
///     default-0.7, clamp-to-[0,1]),
///   * only wholly non-JSON content is a hard error.
pub fn parse_decomposition(content: &str) -> Result<Decomposition> {
    let v: Value = serde_json::from_str(content.trim())
        .with_context(|| format!("decompose returned non-JSON content: {content:?}"))?;
    if !v.is_object() {
        return Err(anyhow!("decompose JSON is not an object: {content:?}"));
    }

    let str_field = |key: &str| -> String {
        v.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };

    let entities = v
        .get("entities")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(entity_from_value).collect())
        .unwrap_or_default();

    let facts = v
        .get("facts")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(fact_from_value).collect())
        .unwrap_or_default();

    let references = v
        .get("references")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // Salience defaults to "note" (not "") so the briefing always has a class to
    // rank by; lowercased so labels collapse.
    let salience = {
        let s = str_field("salience").to_lowercase();
        if s.is_empty() {
            "note".to_string()
        } else {
            s
        }
    };

    Ok(Decomposition {
        summary: str_field("summary"),
        intent: str_field("intent"),
        entities,
        facts,
        references,
        salience,
    })
}

/// Parse one entity object, or `None` when it has no usable `name`. A missing
/// `kind` falls back to `"thing"` so the entity is still linkable.
fn entity_from_value(item: &Value) -> Option<DecomposedEntity> {
    let name = item.get("name").and_then(Value::as_str)?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let kind = item
        .get("kind")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .unwrap_or("thing")
        .to_lowercase();
    Some(DecomposedEntity { name, kind })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_decomposition() {
        let content = r#"{
            "summary": "Vijay is deciding the embedding store for localmem.",
            "intent": "decide between DuckDB and LanceDB for vectors",
            "entities": [
                {"name": "Vijay", "kind": "Person"},
                {"name": "localmem", "kind": "project"},
                {"name": "LanceDB", "kind": "tool"}
            ],
            "facts": [
                {"subject": "user", "predicate": "prefers", "object": "local-first storage", "confidence": 0.9},
                {"subject": "localmem", "predicate": "uses", "object": "LanceDB", "confidence": 0.8}
            ],
            "references": ["core/src/vectors.rs", "  ", "https://lancedb.com"],
            "salience": "Decision"
        }"#;
        let d = parse_decomposition(content).unwrap();
        assert!(d.summary.starts_with("Vijay is deciding"));
        assert_eq!(d.intent, "decide between DuckDB and LanceDB for vectors");
        assert_eq!(d.entities.len(), 3);
        // kind is lowercased so "Person" and "person" collapse.
        assert_eq!(d.entities[0].kind, "person");
        assert_eq!(d.facts.len(), 2);
        assert_eq!(d.facts[0].subject, "user");
        // references keep order, drop blanks; salience lowercased.
        assert_eq!(
            d.references,
            vec!["core/src/vectors.rs", "https://lancedb.com"]
        );
        assert_eq!(d.salience, "decision");
    }

    #[test]
    fn entities_skip_unnamed_and_default_kind() {
        let content = r#"{
            "summary": "",
            "entities": [
                {"name": "", "kind": "tool"},
                {"name": "Ollama"}
            ]
        }"#;
        let d = parse_decomposition(content).unwrap();
        assert_eq!(d.entities.len(), 1, "the unnamed entity is dropped");
        assert_eq!(d.entities[0].name, "Ollama");
        assert_eq!(d.entities[0].kind, "thing", "missing kind defaults");
    }

    #[test]
    fn facts_reuse_shared_tolerance_rules() {
        // Partial fact skipped, out-of-range confidence clamped, absent
        // confidence defaulted - identical to the local-llm facts contract.
        let content = r#"{
            "facts": [
                {"subject": "", "predicate": "x", "object": "y"},
                {"subject": "a", "predicate": "b", "object": "c", "confidence": 5.0},
                {"subject": "d", "predicate": "e", "object": "f"}
            ]
        }"#;
        let d = parse_decomposition(content).unwrap();
        assert_eq!(d.facts.len(), 2, "the empty-subject fact is skipped");
        assert!((d.facts[0].confidence - 1.0).abs() < 1e-9, "clamped to 1.0");
        assert!(
            (d.facts[1].confidence - 0.7).abs() < 1e-9,
            "defaulted to 0.7"
        );
    }

    #[test]
    fn missing_optional_fields_default_empty() {
        let d = parse_decomposition(r#"{"summary": "just a gist"}"#).unwrap();
        assert_eq!(d.summary, "just a gist");
        assert_eq!(d.intent, "");
        assert!(d.entities.is_empty());
        assert!(d.facts.is_empty());
        assert!(d.references.is_empty());
        assert_eq!(d.salience, "note", "salience defaults to note, never empty");
    }

    #[test]
    fn non_json_is_an_error() {
        assert!(parse_decomposition("sorry, I cannot help with that").is_err());
    }

    #[test]
    fn non_object_json_is_an_error() {
        // A bare array is valid JSON but not a decomposition object.
        assert!(parse_decomposition(r#"["summary"]"#).is_err());
    }

    #[test]
    fn system_prompt_carries_user_subject_and_json_shape() {
        let opts = DecomposeOptions {
            user_subject: "vijay".to_string(),
            source: Some("cursor".to_string()),
        };
        let p = decompose_system_prompt(&opts);
        assert!(p.contains("\"vijay\""), "canonical user subject is named");
        assert!(
            p.contains("`cursor`"),
            "opaque source is surfaced as context"
        );
        assert!(p.contains("\"summary\""));
        assert!(p.contains("\"entities\""));
        assert!(p.contains("\"facts\""));
    }

    #[test]
    fn system_prompt_omits_provenance_when_source_absent() {
        let p = decompose_system_prompt(&DecomposeOptions::default());
        assert!(!p.contains("captured from"));
        assert!(p.contains("\"user\""), "default canonical subject");
    }
}
