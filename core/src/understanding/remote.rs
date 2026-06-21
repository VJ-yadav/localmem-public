//! Remote decomposition backends (intelligence v2, P1): OpenAI + Anthropic.
//!
//! Same [`Decomposer`] interface as the local Ollama path, so the worker is
//! backend-agnostic. The user supplies their OWN key (named by
//! `[understanding].api_key_env`), so capture text leaves the machine ONLY on
//! this explicit per-user opt-in (the no-plaintext-leaves-the-machine promise) — the Community default stays local.
//!
//! Failure policy (deliberate): a remote error does NOT silently fall back to
//! the local model. A user who opted into a frontier key did so because the
//! local model is weak; degrading to it would reintroduce exactly the junk they
//! opted out of. Instead the worker's existing error path keeps the capture raw
//! (still searchable via lexical + vector) and re-decomposes on the next
//! backfill once the provider is reachable again. No code path REQUIRES the
//! network — the default provider is local — so invariant #4 holds.

use super::decompose::{
    decompose_system_prompt, parse_decomposition, DecomposeOptions, Decomposition,
};
use super::ollama::{Decomposer, OllamaDecomposer};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// One remote decompose call budget. Frontier models can take a few seconds;
/// this caps a hung connection without tripping on normal generation.
const REMOTE_TIMEOUT_SECS: u64 = 90;

/// Resolve the API key from the env var NAMED in config. We store the name, not
/// the secret, so the config is safe to commit and the key lives only in the
/// environment / a secrets manager.
fn read_key(api_key_env: &str) -> Result<String> {
    if api_key_env.trim().is_empty() {
        bail!(
            "[understanding].api_key_env is empty; set it to the env var holding \
             your provider key (e.g. \"OPENAI_API_KEY\")"
        );
    }
    let key = std::env::var(api_key_env).map_err(|_| {
        anyhow!("env var `{api_key_env}` is not set; export your provider API key there")
    })?;
    if key.trim().is_empty() {
        bail!("env var `{api_key_env}` is set but empty");
    }
    Ok(key)
}

/// Extract a clean JSON object from an LLM response that may wrap it in a
/// markdown fence or prose. OpenAI's `json_object` mode returns clean JSON
/// (passes through); Anthropic has no strict JSON mode, so we slice the
/// outermost `{ ... }`.
fn json_object_slice(content: &str) -> &str {
    let mut s = content.trim();
    if let Some(rest) = s.strip_prefix("```json") {
        s = rest;
    } else if let Some(rest) = s.strip_prefix("```") {
        s = rest;
    }
    s = s.strip_suffix("```").unwrap_or(s).trim();
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b >= a => &s[a..=b],
        _ => s,
    }
}

/// OpenAI Chat Completions decomposer (`/v1/chat/completions`, json_object).
pub struct OpenAiDecomposer {
    model: String,
    key: String,
}

impl OpenAiDecomposer {
    pub fn new(model: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            key: key.into(),
        }
    }
}

#[async_trait]
impl Decomposer for OpenAiDecomposer {
    async fn decompose(&self, text: &str, opts: &DecomposeOptions) -> Result<Decomposition> {
        let system = decompose_system_prompt(opts);
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "response_format": {"type": "json_object"},
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": text},
            ],
        });
        let key = self.key.clone();
        let resp: Value = tokio::task::spawn_blocking(move || -> Result<Value> {
            let agent = ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(REMOTE_TIMEOUT_SECS))
                .build();
            agent
                .post("https://api.openai.com/v1/chat/completions")
                .set("Authorization", &format!("Bearer {key}"))
                .set("Content-Type", "application/json")
                .send_json(body)
                .map_err(|e| anyhow!("openai request failed: {e}"))?
                .into_json::<Value>()
                .context("parse openai response JSON")
        })
        .await
        .context("join openai task")??;

        let content = resp
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow!("openai response missing choices[0].message.content"))?;
        parse_decomposition(json_object_slice(content))
    }
}

/// Anthropic Messages decomposer (`/v1/messages`). `temperature` is omitted on
/// purpose: it is rejected (400) on the latest models and optional elsewhere.
pub struct AnthropicDecomposer {
    model: String,
    key: String,
}

impl AnthropicDecomposer {
    pub fn new(model: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            key: key.into(),
        }
    }
}

#[async_trait]
impl Decomposer for AnthropicDecomposer {
    async fn decompose(&self, text: &str, opts: &DecomposeOptions) -> Result<Decomposition> {
        let system = decompose_system_prompt(opts);
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 1500,
            "system": system,
            "messages": [{"role": "user", "content": text}],
        });
        let key = self.key.clone();
        let resp: Value = tokio::task::spawn_blocking(move || -> Result<Value> {
            let agent = ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(REMOTE_TIMEOUT_SECS))
                .build();
            agent
                .post("https://api.anthropic.com/v1/messages")
                .set("x-api-key", &key)
                .set("anthropic-version", "2023-06-01")
                .set("content-type", "application/json")
                .send_json(body)
                .map_err(|e| anyhow!("anthropic request failed: {e}"))?
                .into_json::<Value>()
                .context("parse anthropic response JSON")
        })
        .await
        .context("join anthropic task")??;

        // `content` is an array of blocks; take the first text block.
        let content = resp
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        b.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
            })
            .ok_or_else(|| anyhow!("anthropic response missing a text content block"))?;
        parse_decomposition(json_object_slice(content))
    }
}

/// Build the decomposition backend from config. `ollama`/`""` -> local (the
/// caller passes the already-resolved Ollama tag as `model`). `openai` /
/// `anthropic` -> remote, reading the key from `api_key_env`. Unknown provider
/// is a hard error so a typo is loud rather than silently disabling
/// understanding.
pub fn build_decomposer(
    provider: &str,
    model: &str,
    ollama_endpoint: &str,
    api_key_env: &str,
) -> Result<Arc<dyn Decomposer>> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "ollama" | "" => Ok(Arc::new(OllamaDecomposer::new(
            model.to_string(),
            ollama_endpoint.to_string(),
        ))),
        "openai" => Ok(Arc::new(OpenAiDecomposer::new(
            model.to_string(),
            read_key(api_key_env)?,
        ))),
        "anthropic" => Ok(Arc::new(AnthropicDecomposer::new(
            model.to_string(),
            read_key(api_key_env)?,
        ))),
        other => bail!(
            "unknown [understanding].provider `{other}` (expected ollama | openai | anthropic)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_slice_strips_fences_and_prose() {
        assert_eq!(json_object_slice("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(json_object_slice("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(
            json_object_slice("Here is the JSON:\n{\"a\":1}\nThanks!"),
            "{\"a\":1}"
        );
    }

    #[test]
    fn factory_selects_local_without_a_key() {
        assert!(
            build_decomposer("ollama", "llama3.2:latest", "http://localhost:11434", "").is_ok()
        );
        assert!(build_decomposer("", "llama3.2:latest", "http://localhost:11434", "").is_ok());
    }

    #[test]
    fn factory_rejects_unknown_provider() {
        let r = build_decomposer("gemini", "x", "http://localhost:11434", "K");
        assert!(r.is_err());
        let err = r.err().unwrap();
        assert!(err.to_string().contains("unknown"), "got: {err}");
    }

    #[test]
    fn factory_errors_when_remote_key_env_missing() {
        // A var name that is virtually certain not to be set in the test env.
        let miss = "LOCALMEM_TEST_NO_SUCH_KEY_4F2A";
        std::env::remove_var(miss);
        assert!(build_decomposer("openai", "gpt-4o", "http://localhost:11434", miss).is_err());
        assert!(build_decomposer(
            "anthropic",
            "claude-opus-4-8",
            "http://localhost:11434",
            miss
        )
        .is_err());
        // Empty api_key_env is also an error for remote providers.
        assert!(build_decomposer("openai", "gpt-4o", "http://localhost:11434", "").is_err());
    }
}
