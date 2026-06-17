//! Local LLM-backed fact extractor (Phase 3 / T-58 + T-62).
//!
//! Calls a locally-running Ollama server to extract structured
//! `(subject, predicate, object, confidence)` facts from a capture, lifting the
//! quality ceiling of the regex `rules` extractor. It is OPT-IN (only active
//! when `"local-llm"` is in `[extractor].plugins`) and LOCAL (loopback Ollama),
//! so it does not violate the no-network guarantee. When Ollama is unreachable
//! or returns unusable output, `extract` errors and the registry degrades to
//! whatever the other extractors (e.g. `rules`) found, so writes still produce
//! facts.
//!
//! The cross-encoder/SLM specialization noted in the master spec slots in later
//! as a separate extractor; this Ollama path validates the pipeline + the
//! structured-output contract first.

use super::{ExtractedFact, Extractor};
use crate::kind::Kind;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

/// Slug used in `[extractor].plugins` config and in `Extractor::name()`.
pub const NAME: &str = "local-llm";

/// Overall timeout for the Ollama call. Local generation on a small model is
/// usually a few seconds; the generous cap covers cold model loads.
const OLLAMA_TIMEOUT_SECS: u64 = 120;

/// Instruction to the model. Forces strict JSON so the response is parseable.
const SYSTEM_PROMPT: &str = "You extract structured facts from a short note. \
Respond with ONLY JSON of the exact form \
{\"facts\":[{\"subject\":\"...\",\"predicate\":\"...\",\"object\":\"...\",\"confidence\":0.0}]}. \
`subject` is the entity the fact is about, `predicate` the relation, `object` the value. \
`confidence` is your certainty in [0,1]. Extract only facts clearly stated in the note; \
do not infer. If there are no clear facts, return {\"facts\":[]}.";

pub struct LocalLlmExtractor {
    model: String,
    endpoint: String,
}

impl LocalLlmExtractor {
    pub fn from_config(cfg: &crate::config::ExtractorSection) -> Self {
        Self {
            model: cfg.llm_model.clone(),
            endpoint: cfg.ollama_endpoint.clone(),
        }
    }
}

#[async_trait]
impl Extractor for LocalLlmExtractor {
    fn name(&self) -> &str {
        NAME
    }

    async fn extract(&self, text: &str, _kind_hint: Option<&Kind>) -> Result<Vec<ExtractedFact>> {
        let url = format!("{}/api/chat", self.endpoint.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "stream": false,
            "format": "json",
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": text},
            ],
        });

        // ureq is synchronous; run it on a blocking thread so we don't stall
        // the async runtime. The error names the likely fix (Ollama not running
        // / model not pulled) because the registry surfaces it as a WARN.
        let url_for_err = url.clone();
        let resp: Value = tokio::task::spawn_blocking(move || -> Result<Value> {
            let agent = ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(OLLAMA_TIMEOUT_SECS))
                .build();
            let response = agent.post(&url).send_json(body).map_err(|e| {
                anyhow!(
                    "ollama request to {url_for_err} failed \
                     (is `ollama serve` running and the model pulled?): {e}"
                )
            })?;
            response
                .into_json::<Value>()
                .context("parse ollama response JSON")
        })
        .await
        .context("join ollama extraction task")??;

        // Ollama /api/chat returns {"message": {"content": "<json string>"}}.
        let content = resp
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow!("ollama response missing message.content"))?;
        parse_facts(content)
    }
}

/// Parse the model's JSON content string into [`ExtractedFact`]s. Pure and
/// model-free: the testable core of the contract. Accepts either
/// `{"facts":[...]}` or a bare `[...]`, skips entries missing any of
/// subject/predicate/object, and clamps confidence to `[0,1]` (default 0.7
/// when absent).
pub(crate) fn parse_facts(content: &str) -> Result<Vec<ExtractedFact>> {
    let v: Value = serde_json::from_str(content.trim())
        .with_context(|| format!("local-llm returned non-JSON content: {content:?}"))?;
    let arr = v
        .get("facts")
        .and_then(|f| f.as_array())
        .or_else(|| v.as_array())
        .ok_or_else(|| anyhow!("local-llm JSON missing a `facts` array"))?;

    Ok(arr.iter().filter_map(fact_from_value).collect())
}

/// Parse one fact object into an [`ExtractedFact`], or `None` if it is partial.
/// Shared by the local-llm facts contract and the understanding-layer
/// decomposition (which carries facts as one of its fields), so the tolerance
/// rules (skip-partial, default-0.7, clamp-to-[0,1]) live in exactly one place.
pub(crate) fn fact_from_value(item: &Value) -> Option<ExtractedFact> {
    let str_field = |key: &str| -> String {
        item.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let subject = str_field("subject");
    let predicate = str_field("predicate");
    let object = str_field("object");
    if subject.is_empty() || predicate.is_empty() || object.is_empty() {
        return None; // skip partial/malformed facts rather than poison the store
    }
    let confidence = item
        .get("confidence")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.7)
        .clamp(0.0, 1.0);
    Some(ExtractedFact {
        subject,
        predicate,
        object,
        confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_facts_reads_facts_object() {
        let content = r#"{"facts":[
            {"subject":"user","predicate":"prefers","object":"functional Rust","confidence":0.9},
            {"subject":"team","predicate":"chose","object":"DuckDB","confidence":0.8}
        ]}"#;
        let facts = parse_facts(content).unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].subject, "user");
        assert_eq!(facts[0].object, "functional Rust");
        assert!((facts[1].confidence - 0.8).abs() < 1e-9);
    }

    #[test]
    fn parse_facts_accepts_bare_array_and_defaults_confidence() {
        let content = r#"[{"subject":"user","predicate":"uses","object":"SQLite"}]"#;
        let facts = parse_facts(content).unwrap();
        assert_eq!(facts.len(), 1);
        assert!((facts[0].confidence - 0.7).abs() < 1e-9); // default when absent
    }

    #[test]
    fn parse_facts_skips_partial_and_clamps_confidence() {
        let content = r#"{"facts":[
            {"subject":"","predicate":"x","object":"y"},
            {"subject":"a","predicate":"b","object":"c","confidence":5.0}
        ]}"#;
        let facts = parse_facts(content).unwrap();
        assert_eq!(facts.len(), 1, "the empty-subject fact is skipped");
        assert!(
            (facts[0].confidence - 1.0).abs() < 1e-9,
            "confidence clamped to 1.0"
        );
    }

    #[test]
    fn parse_facts_empty_list_is_ok() {
        assert!(parse_facts(r#"{"facts":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn parse_facts_non_json_errors() {
        assert!(parse_facts("sorry, I cannot help with that").is_err());
    }

    #[test]
    fn from_config_carries_model_and_endpoint() {
        let cfg = crate::config::ExtractorSection {
            llm_model: "qwen2.5:7b".into(),
            ollama_endpoint: "http://localhost:9999".into(),
            ..Default::default()
        };
        let ex = LocalLlmExtractor::from_config(&cfg);
        assert_eq!(ex.model, "qwen2.5:7b");
        assert_eq!(ex.endpoint, "http://localhost:9999");
    }
}
