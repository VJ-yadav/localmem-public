//! The Ollama-backed decomposer: the live LLM call behind the understanding
//! worker. Mirrors `extractor::local_llm`'s `spawn_blocking` + `ureq` pattern
//! (synchronous HTTP off the async runtime), but produces the richer
//! [`Decomposition`] instead of facts alone.
//!
//! The worker holds a `Arc<dyn Decomposer>` rather than this concrete type so
//! it is unit-testable with a stub that returns a canned [`Decomposition`]
//! without a running model. Loopback by default, so it honors the no-network
//! guarantee: understanding is opt-in and degrades to "capture stays raw" when
//! Ollama is unreachable.

use super::decompose::{
    decompose_system_prompt, parse_decomposition, DecomposeOptions, Decomposition,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

/// Overall timeout for one decompose call. Generous cap covers a cold model
/// load; steady-state generation on a small local model is a few seconds.
const OLLAMA_TIMEOUT_SECS: u64 = 120;

/// Turns one capture's text into a structured [`Decomposition`]. Abstracted so
/// the worker can be driven by a stub in tests.
#[async_trait]
pub trait Decomposer: Send + Sync {
    async fn decompose(&self, text: &str, opts: &DecomposeOptions) -> Result<Decomposition>;
}

/// Live decomposer calling a local Ollama server's `/api/chat`.
pub struct OllamaDecomposer {
    model: String,
    endpoint: String,
}

impl OllamaDecomposer {
    pub fn new(model: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            endpoint: endpoint.into(),
        }
    }
}

#[async_trait]
impl Decomposer for OllamaDecomposer {
    async fn decompose(&self, text: &str, opts: &DecomposeOptions) -> Result<Decomposition> {
        let system = decompose_system_prompt(opts);
        let content = chat_json(&self.endpoint, &self.model, &system, text).await?;
        parse_decomposition(&content)
    }
}

/// One JSON-mode `/api/chat` round-trip. Shared by the decomposer and the
/// briefing synthesizer so both speak to Ollama identically (`format=json`,
/// blocking `ureq` off the async runtime). Returns the assistant message
/// content, which callers parse into their own schema.
pub(crate) async fn chat_json(
    endpoint: &str,
    model: &str,
    system: &str,
    user: &str,
) -> Result<String> {
    let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "stream": false,
        "format": "json",
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });

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
    .context("join ollama chat task")??;

    resp.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("ollama response missing message.content"))
}

/// Probe budget for `/api/tags`. Loopback + local, so a healthy server answers
/// well under this; a missing server fails fast so startup is not held up.
const TAGS_PROBE_TIMEOUT_MS: u64 = 1500;

/// Query Ollama for the model tags it actually has installed (`/api/tags`).
/// `None` means the server is unreachable (we cannot know what is installed).
/// This is the "verify against reality" primitive: never assume a tag exists.
pub fn installed_models(endpoint: &str) -> Option<Vec<String>> {
    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(TAGS_PROBE_TIMEOUT_MS))
        .build();
    let resp = agent.get(&url).call().ok()?;
    let v: Value = resp.into_json().ok()?;
    Some(
        v.get("models")?
            .as_array()?
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .collect(),
    )
}

/// The outcome of resolving the configured model against what is really
/// installed. Nothing here is hardcoded: the decision is driven entirely by the
/// live `/api/tags` list and the user's configured tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelResolution {
    /// The exact configured tag is installed. Use it.
    Exact(String),
    /// The configured tag is absent, but a same-family tag is installed; use it
    /// instead of failing. Carries both so the caller can explain the swap.
    Substituted { used: String, configured: String },
    /// Ollama is reachable but has no exact or same-family match. The worker
    /// should stay idle (running it would 404 every capture); the caller guides
    /// the user with the real installed list.
    NoMatch { installed: Vec<String> },
    /// Ollama was unreachable, so we could not verify. Fall back to the
    /// configured tag so the worker can retry once the server returns.
    Unprobed(String),
}

impl ModelResolution {
    /// The tag the worker should actually call with, or `None` when it should
    /// not run at all (`NoMatch`).
    pub fn model_to_use(&self) -> Option<&str> {
        match self {
            ModelResolution::Exact(m) | ModelResolution::Unprobed(m) => Some(m),
            ModelResolution::Substituted { used, .. } => Some(used),
            ModelResolution::NoMatch { .. } => None,
        }
    }
}

/// Resolve the model to use from the live installed list (or `None` if Ollama
/// was unreachable) and the configured tag. Pure + testable; the I/O lives in
/// [`installed_models`]. Order: exact match, then same-family substitute, then
/// give up with the real list so the caller can guide a pull.
pub fn resolve_model(installed: Option<&[String]>, configured: &str) -> ModelResolution {
    let Some(installed) = installed else {
        return ModelResolution::Unprobed(configured.to_string());
    };
    if installed.iter().any(|m| m == configured) {
        return ModelResolution::Exact(configured.to_string());
    }
    let family = configured.split(':').next().unwrap_or(configured);
    if let Some(alt) = installed
        .iter()
        .find(|m| m.split(':').next().map(|f| f == family).unwrap_or(false))
    {
        return ModelResolution::Substituted {
            used: alt.clone(),
            configured: configured.to_string(),
        };
    }
    ModelResolution::NoMatch {
        installed: installed.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_exact_match() {
        let installed = vec!["llama3.2:latest".to_string(), "qwen2.5:7b".to_string()];
        assert_eq!(
            resolve_model(Some(&installed), "qwen2.5:7b"),
            ModelResolution::Exact("qwen2.5:7b".to_string())
        );
    }

    #[test]
    fn resolve_substitutes_same_family_instead_of_false_ready() {
        // The exact bug we hit: want llama3.2:3b, only llama3.2:latest present.
        let installed = vec!["llama3.2:latest".to_string()];
        let r = resolve_model(Some(&installed), "llama3.2:3b");
        assert_eq!(
            r,
            ModelResolution::Substituted {
                used: "llama3.2:latest".to_string(),
                configured: "llama3.2:3b".to_string(),
            }
        );
        assert_eq!(r.model_to_use(), Some("llama3.2:latest"));
    }

    #[test]
    fn resolve_no_match_returns_installed_and_no_model() {
        let installed = vec!["mistral:7b".to_string()];
        let r = resolve_model(Some(&installed), "llama3.2:3b");
        assert_eq!(
            r,
            ModelResolution::NoMatch {
                installed: vec!["mistral:7b".to_string()]
            }
        );
        assert_eq!(r.model_to_use(), None, "no model -> worker must not run");
    }

    #[test]
    fn resolve_unprobed_falls_back_to_configured() {
        let r = resolve_model(None, "llama3.2:3b");
        assert_eq!(r, ModelResolution::Unprobed("llama3.2:3b".to_string()));
        assert_eq!(
            r.model_to_use(),
            Some("llama3.2:3b"),
            "Ollama down -> keep configured, retry later"
        );
    }

    /// Live round-trip against a running Ollama. Ignored by default (CI has no
    /// model); run locally with:
    ///   cargo test --lib live_decompose_against_ollama -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires a running Ollama; set LM_TEST_MODEL to an installed tag"]
    async fn live_decompose_against_ollama() {
        let model =
            std::env::var("LM_TEST_MODEL").unwrap_or_else(|_| "llama3.2:latest".to_string());
        let d = OllamaDecomposer::new(model, "http://localhost:11434");
        let opts = DecomposeOptions {
            user_subject: "user".to_string(),
            source: Some("claude-code".to_string()),
        };
        let text = "I'm Vijay, I build localmem in Rust and I strongly prefer \
                    local-first tools over cloud services. I decided localmem should \
                    use LanceDB for vectors because it supports native concurrent reads.";
        let out = d.decompose(text, &opts).await.expect("decompose call");
        eprintln!("--- SUMMARY: {}", out.summary);
        eprintln!("--- INTENT:  {}", out.intent);
        eprintln!("--- ENTITIES: {:?}", out.entities);
        eprintln!("--- FACTS:    {:?}", out.facts);
        assert!(
            !out.summary.is_empty() || !out.facts.is_empty(),
            "expected the model to return at least a summary or some facts"
        );
    }
}
