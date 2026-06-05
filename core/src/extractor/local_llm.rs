//! Local LLM-backed fact extractor stub (T-58 + T-62).
//!
//! Real implementation lives behind `localmem fetch-model llama3.2:3b`
//! (T-62) plus the Ollama HTTP client (queued for the same task slice).
//! Until then, this stub bails loudly when called so a user who set
//! `[extractor].plugins = ["rules", "local-llm"]` sees a clear error
//! in their logs rather than silent "no LLM extraction happens."
//!
//! The discipline mirrors `core/src/rewriter.rs::LocalLlmRewriter`:
//! recognise the config entry at load time, fail at use time. The
//! registry surfaces the failure as a WARN-level log and continues
//! with whatever the other extractors found, so the user's writes
//! still produce facts.

use super::{ExtractedFact, Extractor};
use crate::kind::Kind;
use anyhow::{bail, Result};
use async_trait::async_trait;

/// Slug used in `[extractor].plugins` config and in `Extractor::name()`.
pub const NAME: &str = "local-llm";

pub struct LocalLlmExtractor {
    /// Ollama tag we WOULD load when T-62 lands. Stored so the bail
    /// message can name it back to the user.
    model: String,
}

impl LocalLlmExtractor {
    /// Pull config knobs (model tag, endpoint) so the stub can echo
    /// them in its error message. Doesn't actually contact Ollama.
    pub fn from_config(cfg: &crate::config::ExtractorSection) -> Self {
        Self {
            model: cfg.llm_model.clone(),
        }
    }
}

#[async_trait]
impl Extractor for LocalLlmExtractor {
    fn name(&self) -> &str {
        NAME
    }

    async fn extract(
        &self,
        _text: &str,
        _kind_hint: Option<&Kind>,
    ) -> Result<Vec<ExtractedFact>> {
        bail!(
            "local-llm extractor is not yet implemented (model={:?}). \
             Track T-58 + T-62 in TASKS.md; \
             today the registry skips this extractor and degrades to \
             the others. Remove \"local-llm\" from [extractor].plugins \
             to silence this warning, or wait for the implementation.",
            self.model
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_bails_loudly_with_actionable_message() {
        let ex = LocalLlmExtractor {
            model: "llama3.2:3b".into(),
        };
        let err = ex.extract("anything", None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not yet implemented"),
            "error must mark stub status: {msg}"
        );
        assert!(
            msg.contains("llama3.2:3b"),
            "error must echo the configured model: {msg}"
        );
        assert!(
            msg.contains("T-62"),
            "error must point at the tracking task: {msg}"
        );
    }

    #[test]
    fn from_config_carries_llm_model_into_stub() {
        let cfg = crate::config::ExtractorSection {
            llm_model: "qwen2.5:7b".into(),
            ..Default::default()
        };
        let ex = LocalLlmExtractor::from_config(&cfg);
        assert_eq!(ex.model, "qwen2.5:7b");
    }
}
