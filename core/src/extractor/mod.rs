//! Pluggable fact extractor surface (T-58).
//!
//! v0.1 shipped a single concrete `Extractor` struct (rule-based, regex).
//! v0.2 lifts that surface to a trait so additional implementations can
//! compose in parallel without forking the write pipeline:
//!
//! - [`rules::RulesExtractor`] — the v0.1 regex rules, always available.
//! - [`local_llm::LocalLlmExtractor`] — Ollama-backed (T-58 + T-62).
//!   Stub today; bails loudly so a config typo surfaces.
//! - [`hosted::HostedExtractor`] — our hosted endpoint (T-68 / v0.2.1).
//!   Stub today; same discipline.
//!
//! Multiple extractors compose via [`ExtractorRegistry`]. Run order and
//! dedup are policy decisions baked into the registry, not the trait, so
//! a future caller can build a different composition strategy without
//! changing each impl.
//!
//! Why an async trait: Ollama and the hosted endpoint are I/O-bound.
//! Forcing the rules path through `.await` adds zero measurable cost
//! (the rules impl just returns a ready future) and keeps callers
//! uniform. `async-trait` is used so the registry can hold
//! `Vec<Box<dyn Extractor>>` for dynamic dispatch.

pub mod hosted;
pub mod local_llm;
pub mod rules;
pub mod yaml;

use crate::kind::Kind;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use tracing::warn;

/// A single extracted fact, awaiting promotion to a [`crate::event`]
/// `fact` event + a [`crate::facts::Fact`] row in DuckDB.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedFact {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f64,
}

/// Pluggable fact extractor.
///
/// Implementations are stateless beyond their construction-time
/// compiled state (regex, model handles). The trait is async because
/// the production impls beyond v0.1's rules (`local-llm`, `hosted`)
/// call out over the network or to a local model server.
///
/// `kind_hint` carries the source capture's [`Kind`] when the caller
/// knows it. Rule-based impls ignore it; LLM impls can use it to bias
/// their prompt ("this is a preference, extract preferences only").
#[async_trait]
pub trait Extractor: Send + Sync {
    /// Slug used in `[extractor].plugins` config and for logging.
    /// Stable per impl; rename = breaking change in config.
    fn name(&self) -> &str;

    /// Run extraction. Returns zero or more facts. An empty result is
    /// the normal "nothing matched" case; an `Err` means the extractor
    /// itself failed (network down, model unloaded). The registry
    /// degrades gracefully on per-extractor errors.
    async fn extract(&self, text: &str, kind_hint: Option<&Kind>) -> Result<Vec<ExtractedFact>>;
}

/// Composes multiple [`Extractor`] impls. Runs them in parallel via
/// `futures::future::join_all`, then dedups by *exact* `(subject,
/// predicate, object)` keeping the highest confidence on collision.
///
/// **Dedup is exact-triple only.** Same `(subject, predicate)` with a
/// *different* `object` is a contradiction, not a duplicate, and flows
/// downstream to T-56 smart forgetting which retires the prior fact.
/// Dedup-by-(s,p) would short-circuit that path.
///
/// **Per-extractor failures degrade gracefully.** If one extractor
/// errors (e.g. Ollama down), the registry logs WARN and returns the
/// successful extractors' output. The user's write does NOT fail
/// because a sidecar extractor died — same discipline as the T-55
/// rewriter.
pub struct ExtractorRegistry {
    extractors: Vec<Box<dyn Extractor>>,
}

// Custom Debug rather than `derive`: `dyn Extractor` isn't Debug-bound
// (we don't want to force every future impl to derive Debug just for
// diagnostic output). Print the names list, which is what callers
// actually want to see.
impl std::fmt::Debug for ExtractorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtractorRegistry")
            .field("extractors", &self.names())
            .finish()
    }
}

impl ExtractorRegistry {
    /// Build a registry from an owned vec of extractors. The caller
    /// is responsible for ordering (the dedup tiebreak picks highest
    /// confidence, so order only matters for ties — fall back to
    /// first-seen by stable sort).
    pub fn new(extractors: Vec<Box<dyn Extractor>>) -> Self {
        Self { extractors }
    }

    /// Convenience: a registry containing only the v0.1 rules
    /// extractor. Used as the default when no `[extractor]` config
    /// is present and by tests that don't care about composition.
    pub fn rules_only() -> Self {
        Self::new(vec![Box::new(rules::RulesExtractor::new())])
    }

    /// Build the registry from `[extractor]` config AND scan the
    /// home's `custom_extractors_dir` for user-authored YAML
    /// extractors (T-59). The home-aware path is what production
    /// callers (server, CLI write) use.
    ///
    /// Loading discipline matches the rest of T-58: a typo in
    /// `plugins`, a broken YAML file, or any I/O error during the
    /// scan is a LOUD failure — `from_config_with_home` returns an
    /// Err and the caller refuses to start. The registry never
    /// silently degrades into "no extraction happens."
    pub fn from_config_with_home(
        cfg: &crate::config::ExtractorSection,
        home: &std::path::Path,
    ) -> Result<Self> {
        let mut built = Self::from_config(cfg)?;
        if !cfg.custom_extractors_dir.is_empty() {
            let dir = home.join(&cfg.custom_extractors_dir);
            let yaml_extractors =
                yaml::load_dir(&dir).context("load user-authored YAML extractors")?;
            for ye in yaml_extractors {
                built.extractors.push(Box::new(ye));
            }
        }
        Ok(built)
    }

    /// Build the registry from `[extractor].plugins` ALONE, with no
    /// YAML scan. Used by tests that want to exercise the registry
    /// without a home directory, and as the inner half of
    /// [`Self::from_config_with_home`]. Unknown names are loud
    /// failures so a config typo surfaces immediately instead of
    /// silently degrading to "no extraction happens."
    ///
    /// `local-llm` and `hosted` plugins are accepted at config time
    /// but their `extract` calls are stubs that bail until T-62 / T-68
    /// land. The registry surfaces those failures as WARN-level logs
    /// and continues; the user's writes still produce rules-extracted
    /// facts.
    pub fn from_config(cfg: &crate::config::ExtractorSection) -> Result<Self> {
        if cfg.plugins.is_empty() {
            // Empty plugins list is a config error, not a silent
            // "extract nothing" mode. The v0.2 default config writes
            // `plugins = ["rules"]`, so reaching here means the user
            // explicitly cleared the list — almost certainly a typo.
            bail!(
                "[extractor].plugins is empty; expected at least one of: \
                 \"rules\", \"local-llm\", \"hosted\""
            );
        }
        let mut out: Vec<Box<dyn Extractor>> = Vec::with_capacity(cfg.plugins.len());
        for name in &cfg.plugins {
            let extractor: Box<dyn Extractor> = match name.as_str() {
                rules::NAME => Box::new(rules::RulesExtractor::new()),
                local_llm::NAME => Box::new(local_llm::LocalLlmExtractor::from_config(cfg)),
                hosted::NAME => Box::new(hosted::HostedExtractor::from_config(cfg)),
                other => bail!(
                    "[extractor].plugins entry {other:?} is unknown; expected one of: \
                     \"rules\", \"local-llm\", \"hosted\""
                ),
            };
            out.push(extractor);
        }
        Ok(Self::new(out))
    }

    /// Number of registered extractors. Used by tests + diagnostics.
    pub fn len(&self) -> usize {
        self.extractors.len()
    }

    /// Returns true when the registry holds no extractors. Never the
    /// case after a successful `from_config` (it bails on empty), but
    /// `new(vec![])` can produce it.
    pub fn is_empty(&self) -> bool {
        self.extractors.is_empty()
    }

    /// Slugs of the registered extractors in registration order. For
    /// `localmem doctor` and JSON output.
    pub fn names(&self) -> Vec<&str> {
        self.extractors.iter().map(|e| e.name()).collect()
    }

    /// Run every extractor in parallel and merge their output.
    ///
    /// Failures from any one extractor are logged at WARN and dropped
    /// from the merge; the others' output still surfaces. The caller
    /// always gets a `Result::Ok` unless something catastrophic
    /// happens at the registry level (which today is unreachable —
    /// the trait method is the only failure surface).
    pub async fn extract(
        &self,
        text: &str,
        kind_hint: Option<&Kind>,
    ) -> Result<Vec<ExtractedFact>> {
        if self.extractors.is_empty() {
            return Ok(Vec::new());
        }
        let futures = self
            .extractors
            .iter()
            .map(|ex| async move { (ex.name(), ex.extract(text, kind_hint).await) });
        let results = futures::future::join_all(futures).await;

        let mut merged: HashMap<(String, String, String), ExtractedFact> = HashMap::new();
        for (name, result) in results {
            match result {
                Ok(facts) => {
                    for f in facts {
                        let key = (f.subject.clone(), f.predicate.clone(), f.object.clone());
                        merged
                            .entry(key)
                            .and_modify(|existing| {
                                // Highest confidence wins. Equal
                                // confidence keeps first-seen (HashMap
                                // entry order isn't deterministic, but
                                // the tiebreak only matters for floats
                                // we authored).
                                if f.confidence > existing.confidence {
                                    *existing = f.clone();
                                }
                            })
                            .or_insert(f);
                    }
                }
                Err(e) => {
                    // Degrade gracefully. The user's write completes
                    // with whatever the surviving extractors found.
                    warn!(
                        extractor = name,
                        error = %e,
                        "extractor failed; skipping its output for this call",
                    );
                }
            }
        }
        Ok(merged.into_values().collect())
    }
}

impl Default for ExtractorRegistry {
    fn default() -> Self {
        Self::rules_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Test fixture: emits one fixed fact at the configured confidence.
    /// Used to exercise the merge tiebreak with deterministic inputs.
    struct FixedExtractor {
        name: String,
        fact: ExtractedFact,
    }

    impl FixedExtractor {
        fn new(name: &str, subject: &str, predicate: &str, object: &str, confidence: f64) -> Self {
            Self {
                name: name.into(),
                fact: ExtractedFact {
                    subject: subject.into(),
                    predicate: predicate.into(),
                    object: object.into(),
                    confidence,
                },
            }
        }
    }

    #[async_trait]
    impl Extractor for FixedExtractor {
        fn name(&self) -> &str {
            &self.name
        }
        async fn extract(
            &self,
            _text: &str,
            _kind_hint: Option<&Kind>,
        ) -> Result<Vec<ExtractedFact>> {
            Ok(vec![self.fact.clone()])
        }
    }

    /// Test fixture: always errors. Used to exercise the registry's
    /// "degrade gracefully on per-extractor failure" path.
    struct FailingExtractor {
        name: String,
    }

    #[async_trait]
    impl Extractor for FailingExtractor {
        fn name(&self) -> &str {
            &self.name
        }
        async fn extract(
            &self,
            _text: &str,
            _kind_hint: Option<&Kind>,
        ) -> Result<Vec<ExtractedFact>> {
            anyhow::bail!("simulated extractor failure")
        }
    }

    #[tokio::test]
    async fn registry_composes_multiple_extractors() {
        // Two extractors emit different facts; both must survive.
        let a = FixedExtractor::new("a", "user", "prefers", "rust", 0.7);
        let b = FixedExtractor::new("b", "user", "lives_in", "Berlin", 0.8);
        let reg = ExtractorRegistry::new(vec![Box::new(a), Box::new(b)]);
        let mut out = reg.extract("any input", None).await.unwrap();
        out.sort_by(|x, y| x.predicate.cmp(&y.predicate));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].predicate, "lives_in");
        assert_eq!(out[1].predicate, "prefers");
    }

    #[tokio::test]
    async fn registry_dedups_exact_triple_by_max_confidence() {
        // Two extractors agree on the same (s, p, o); the higher
        // confidence wins.
        let low = FixedExtractor::new("low", "user", "prefers", "rust", 0.5);
        let high = FixedExtractor::new("high", "user", "prefers", "rust", 0.9);
        let reg = ExtractorRegistry::new(vec![Box::new(low), Box::new(high)]);
        let out = reg.extract("any", None).await.unwrap();
        assert_eq!(out.len(), 1);
        assert!(
            (out[0].confidence - 0.9).abs() < 1e-9,
            "max confidence must win, got {}",
            out[0].confidence,
        );
    }

    #[tokio::test]
    async fn registry_does_not_dedup_contradictions() {
        // Same (s, p) with DIFFERENT objects is a contradiction, not
        // a duplicate. Both must surface so T-56 smart forgetting
        // gets to evaluate the conflict downstream.
        let a = FixedExtractor::new("a", "user", "prefers", "rust", 0.9);
        let b = FixedExtractor::new("b", "user", "prefers", "haskell", 0.9);
        let reg = ExtractorRegistry::new(vec![Box::new(a), Box::new(b)]);
        let out = reg.extract("any", None).await.unwrap();
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn registry_skips_failing_extractor_without_dropping_batch() {
        // One extractor errors; the other still surfaces its fact.
        // The user's write must NOT fail because a sidecar extractor
        // died.
        let ok = FixedExtractor::new("ok", "user", "prefers", "rust", 0.7);
        let bad = FailingExtractor { name: "bad".into() };
        let reg = ExtractorRegistry::new(vec![Box::new(ok), Box::new(bad)]);
        let out = reg.extract("any", None).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].object, "rust");
    }

    #[tokio::test]
    async fn empty_registry_returns_empty_vec_not_error() {
        let reg = ExtractorRegistry::new(vec![]);
        let out = reg.extract("any", None).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn rules_only_constructs_a_working_registry() {
        let reg = ExtractorRegistry::rules_only();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.names(), vec!["rules"]);
        let out = reg.extract("I prefer functional Rust", None).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].predicate, "prefers");
    }

    #[test]
    fn from_config_rejects_unknown_plugin_name() {
        // Loud failure on config typo (no bandaid). The user gets a
        // clear error naming the bad entry + the accepted set.
        let cfg = crate::config::ExtractorSection {
            plugins: vec!["nope".into()],
            ..Default::default()
        };
        let err = ExtractorRegistry::from_config(&cfg).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nope"), "error must name the bad entry: {msg}");
        assert!(
            msg.contains("rules"),
            "error must list accepted names: {msg}"
        );
    }

    #[test]
    fn from_config_rejects_empty_plugins_list() {
        let cfg = crate::config::ExtractorSection {
            plugins: vec![],
            ..Default::default()
        };
        let err = ExtractorRegistry::from_config(&cfg).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "got: {msg}");
    }

    #[test]
    fn from_config_loads_default_rules_only() {
        let cfg = crate::config::ExtractorSection::default();
        let reg = ExtractorRegistry::from_config(&cfg).expect("default config must load");
        assert_eq!(reg.names(), vec!["rules"]);
    }

    // ---- T-59: home-aware load + YAML composition ----------------

    #[test]
    fn from_config_with_home_loads_yaml_extractors_alongside_builtins() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("policies").join("extractors");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("custom.yaml"),
            r#"
id: custom
patterns:
  - regex: '(?i)^my favourite editor is (.+)$'
    fact:
      subject: 'user'
      predicate: 'uses_editor'
      object: '{{capture[1]}}'
      confidence: 0.8
"#,
        )
        .unwrap();
        let cfg = crate::config::ExtractorSection::default();
        let reg = ExtractorRegistry::from_config_with_home(&cfg, tmp.path()).unwrap();
        assert_eq!(reg.names(), vec!["rules", "yaml:custom"]);
    }

    #[tokio::test]
    async fn yaml_and_rules_compose_in_parallel_via_registry() {
        // YAML uses a phrasing the v0.1 rules don't recognise so we
        // can verify the registry actually routes to the YAML
        // extractor (not just rules echoing). Avoid `is`/`prefer`/
        // `email` in the YAML fixture: those are caught by the
        // rules extractor and would muddy the assertion.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("policies").join("extractors");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("favourite-language.yaml"),
            r#"
id: favourite-language
patterns:
  - regex: '(?i)^My favourite language: (.+)$'
    fact:
      subject: 'user'
      predicate: 'favourite_language'
      object: '{{capture[1]}}'
      confidence: 0.8
"#,
        )
        .unwrap();
        let cfg = crate::config::ExtractorSection::default();
        let reg = ExtractorRegistry::from_config_with_home(&cfg, tmp.path()).unwrap();

        // Rules-only input: only rules fires (prefer rule matches;
        // YAML doesn't because the phrasing isn't `My favourite
        // language: ...`).
        let out_rules = reg.extract("I prefer functional Rust", None).await.unwrap();
        assert_eq!(
            out_rules.len(),
            1,
            "expected rules-only fact, got {out_rules:?}"
        );
        assert_eq!(out_rules[0].predicate, "prefers");

        // YAML-only input: only yaml fires. The rules extractor's
        // `is` regex matches " is " literally; the colon variant
        // here avoids that path. Empty extraction from rules + 1
        // from YAML = 1 fact.
        let out_yaml = reg
            .extract("My favourite language: Rust", None)
            .await
            .unwrap();
        assert_eq!(out_yaml.len(), 1, "got {out_yaml:?}");
        assert_eq!(out_yaml[0].predicate, "favourite_language");
        assert_eq!(out_yaml[0].object, "Rust");

        // Neither fires.
        let out_none = reg.extract("hello world", None).await.unwrap();
        assert!(out_none.is_empty());
    }

    #[tokio::test]
    async fn yaml_and_rules_dedup_on_exact_triple_overlap() {
        // YAML fires on the SAME phrasing the rules extractor
        // covers, producing the same (subject, predicate, object).
        // Dedup must collapse them; the YAML's higher confidence
        // wins.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("policies").join("extractors");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("overlap.yaml"),
            r#"
id: overlap
patterns:
  - regex: '(?i)^I prefer (.+)$'
    fact:
      subject: 'user'
      predicate: 'prefers'
      object: '{{capture[1]}}'
      confidence: 0.95
"#,
        )
        .unwrap();
        let cfg = crate::config::ExtractorSection::default();
        let reg = ExtractorRegistry::from_config_with_home(&cfg, tmp.path()).unwrap();

        let out = reg.extract("I prefer rust", None).await.unwrap();
        // Both fire with `(user, prefers, rust)`; the registry dedups
        // to a single triple and keeps the higher confidence (YAML's
        // 0.95 over rules' 0.7).
        assert_eq!(out.len(), 1);
        assert!(
            (out[0].confidence - 0.95).abs() < 1e-9,
            "YAML's higher confidence should win, got {}",
            out[0].confidence
        );
    }

    #[test]
    fn from_config_with_home_bails_on_broken_yaml_file() {
        // Loud failure on a broken file in the dir: the server
        // refuses to start rather than silently skipping the
        // user's extractor. Test that the bail names the file.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("policies").join("extractors");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("broken.yaml"),
            // Invalid regex.
            "id: broken\npatterns: [{regex: '[', fact: {subject: a, predicate: b, object: c}}]\n",
        )
        .unwrap();
        let cfg = crate::config::ExtractorSection::default();
        let err = ExtractorRegistry::from_config_with_home(&cfg, tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("broken.yaml"),
            "error must name the file: {msg}"
        );
    }

    #[test]
    fn from_config_with_home_skips_yaml_scan_when_dir_empty_in_config() {
        // Empty `custom_extractors_dir` disables YAML loading
        // entirely. The registry still builds from `plugins`.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::ExtractorSection {
            custom_extractors_dir: String::new(),
            ..Default::default()
        };
        let reg = ExtractorRegistry::from_config_with_home(&cfg, tmp.path()).unwrap();
        assert_eq!(reg.names(), vec!["rules"]);
    }

    #[test]
    fn from_config_with_home_tolerates_missing_dir() {
        // Configured but not yet created → empty YAML set + plugin
        // extractors still load. A fresh home should not require
        // the user to `mkdir policies/extractors` before writing.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::ExtractorSection::default();
        let reg = ExtractorRegistry::from_config_with_home(&cfg, tmp.path()).unwrap();
        assert_eq!(reg.names(), vec!["rules"]);
    }

    #[test]
    fn from_config_accepts_stubs_even_though_they_bail_on_call() {
        // Per the v0.2 design: `local-llm` and `hosted` are recognised
        // plugin names — they just bail at extract() time. This is so
        // a user's config can opt in to LLM extraction before T-62/T-68
        // ship; the WARN log makes the absence visible without a
        // startup crash.
        let cfg = crate::config::ExtractorSection {
            plugins: vec!["rules".into(), "local-llm".into(), "hosted".into()],
            ..Default::default()
        };
        let reg = ExtractorRegistry::from_config(&cfg).expect("stub plugins must register");
        assert_eq!(reg.names(), vec!["rules", "local-llm", "hosted"]);
    }
}
