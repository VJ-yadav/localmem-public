//! Hosted-endpoint fact extractor stub (T-58 + T-68).
//!
//! Real implementation lives in the v0.2.1 monetization task slice
//! (T-68 Hosted Intelligence endpoint). Until then, this stub bails
//! loudly when called so a user who set `[extractor].plugins =
//! ["rules", "hosted"]` sees a clear error in their logs rather than
//! silent "no hosted extraction happens."
//!
//! The discipline mirrors `core/src/rewriter.rs::HostedRewriter` and
//! [`super::local_llm::LocalLlmExtractor`]: recognise the config
//! entry at load time, fail at use time, never fail the user's write
//! because a sidecar extractor isn't wired yet.

use super::{ExtractedFact, Extractor};
use crate::kind::Kind;
use anyhow::{bail, Result};
use async_trait::async_trait;

/// Slug used in `[extractor].plugins` config and in `Extractor::name()`.
pub const NAME: &str = "hosted";

pub struct HostedExtractor {
    /// Endpoint URL we WOULD POST to when T-68 lands. Stored so the
    /// bail message can name it; empty by default to avoid asserting
    /// any operational URL until the service is real.
    endpoint: String,
}

impl HostedExtractor {
    /// Pull the endpoint URL from config so the stub can echo it in
    /// its bail message. Doesn't actually contact anything.
    pub fn from_config(cfg: &crate::config::ExtractorSection) -> Self {
        Self {
            endpoint: cfg.hosted_endpoint.clone(),
        }
    }
}

#[async_trait]
impl Extractor for HostedExtractor {
    fn name(&self) -> &str {
        NAME
    }

    async fn extract(
        &self,
        _text: &str,
        _kind_hint: Option<&Kind>,
    ) -> Result<Vec<ExtractedFact>> {
        let endpoint_hint = if self.endpoint.is_empty() {
            "(none configured)".to_string()
        } else {
            self.endpoint.clone()
        };
        bail!(
            "hosted extractor is not yet implemented (endpoint={endpoint_hint}). \
             Track T-58 + T-68 in TASKS.md (Hosted Intelligence is v0.2.1). \
             Today the registry skips this extractor and degrades to the \
             others. Remove \"hosted\" from [extractor].plugins to silence \
             this warning, or wait for the implementation."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_bails_loudly_with_actionable_message() {
        let ex = HostedExtractor {
            endpoint: "https://intel.localmem.io/v1/extract".into(),
        };
        let err = ex.extract("anything", None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not yet implemented"),
            "error must mark stub status: {msg}"
        );
        assert!(
            msg.contains("intel.localmem.io"),
            "error must echo the configured endpoint: {msg}"
        );
        assert!(
            msg.contains("T-68"),
            "error must point at the tracking task: {msg}"
        );
    }

    #[tokio::test]
    async fn stub_bail_describes_unconfigured_endpoint_clearly() {
        let ex = HostedExtractor {
            endpoint: String::new(),
        };
        let err = ex.extract("anything", None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("none configured"),
            "error must call out unset endpoint: {msg}"
        );
    }
}
