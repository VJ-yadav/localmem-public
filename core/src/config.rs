//! `<home>/config.toml` loader.
//!
//! See SPEC.md "Configuration" for the on-disk schema. T-46.
//!
//! Resolution order (highest precedence first):
//! 1. CLI flag (handled by clap in main.rs)
//! 2. Environment variable `LOCALMEM_<SECTION>_<KEY>`
//! 3. `<home>/config.toml`
//! 4. Compiled-in defaults
//!
//! v0.1 reads a small subset: `[server].addr` and `[embedder].model` are
//! the only fields currently consumed by code paths. The rest of the
//! struct mirrors SPEC.md so the file we ship via `localmem init` stays
//! a complete reference for what later versions will honor.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Filename at the home root.
pub const CONFIG_FILE: &str = "config.toml";

/// Default loopback address for the HTTP server (SPEC.md `[server].addr`).
pub const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:7788";

/// Top-level config. Every field has a sensible default so missing
/// config.toml is not a failure mode; v0.1 returns `Config::default()` in
/// that case and lets env-var + CLI overrides do their work.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub home: HomeSection,
    pub embedder: EmbedderSection,
    pub extractor: ExtractorSection,
    pub policy: PolicySection,
    pub rewriter: RewriterSection,
    pub retriever: RetrieverSection,
    pub server: ServerSection,
    pub sync: SyncSection,
    pub telemetry: TelemetrySection,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct HomeSection {
    pub version: u32,
    /// User name substituted into rewritten captures (T-55). Empty
    /// string means "fall back to `$USER` env, then to
    /// [`crate::rewriter::FALLBACK_USER_NAME`]". Setting an explicit
    /// value here lets the rewrite output match the name the user
    /// goes by, regardless of their shell login.
    pub user_name: String,
}
impl Default for HomeSection {
    fn default() -> Self {
        Self {
            version: 1,
            user_name: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RewriterSection {
    /// One of `none`, `regex`, `local-llm`, `hosted` (T-55). `none`
    /// is the v0.2 default so a fresh install behaves identically
    /// to v0.1; users opt in to rewriting by flipping this.
    pub mode: String,
}
impl Default for RewriterSection {
    fn default() -> Self {
        Self {
            mode: crate::rewriter::MODE_NONE.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ExtractorSection {
    /// T-58 ordered list of extractor plugin slugs. Each entry must
    /// match a known [`crate::extractor`] impl name: `"rules"`,
    /// `"local-llm"`, `"hosted"`. Unknown slugs are loud failures at
    /// `ExtractorRegistry::from_config`, not silent no-ops.
    ///
    /// Default is `["rules"]` — preserves v0.1 behavior exactly. Adding
    /// `"local-llm"` opts in to Ollama-backed extraction (real once
    /// T-62 lands). Adding `"hosted"` opts in to the hosted endpoint
    /// (real once T-68 lands). Today both stub-error; the registry
    /// degrades gracefully so the rules path still produces facts.
    pub plugins: Vec<String>,
    /// Ollama model tag used by the `local-llm` extractor. Defaults
    /// match the rewriter section's default model so a user who set
    /// both points at the same Ollama instance.
    pub llm_model: String,
    /// HTTPS endpoint for the `hosted` extractor. Empty until T-68
    /// publishes the production URL; the stub echoes it back in its
    /// bail message either way.
    pub hosted_endpoint: String,
    /// T-59: directory holding user-authored YAML extractors. Path
    /// is `<home>`-relative; the registry scans it for `*.yaml` /
    /// `*.yml` files at startup and registers each as a separate
    /// extractor (`yaml:<id>` in `Extractor::name()`). Default
    /// matches SPEC_V0_2. An empty string disables YAML loading
    /// entirely. A missing dir is fine (returns no extractors); a
    /// broken file is a LOUD failure at server/CLI startup, never
    /// silently skipped.
    pub custom_extractors_dir: String,
}
impl Default for ExtractorSection {
    fn default() -> Self {
        Self {
            plugins: vec!["rules".to_string()],
            llm_model: "llama3.2:3b".to_string(),
            hosted_endpoint: String::new(),
            custom_extractors_dir: "policies/extractors".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct RetrieverSection {
    /// T-57 recency bias weight. The hybrid retriever adds
    /// `recency_weight * exp(-age_days / RECENCY_TAU_DAYS)` to each
    /// merged hit's RRF score before the final sort, biasing recent
    /// captures up. `0.0` disables the term; the v0.2 default
    /// (`0.01`) is small enough that lexical/vector signal still
    /// wins on strong matches but breaks ties toward fresh memories.
    /// Not `Eq` because the field is an `f32`; downstream code does
    /// not require `Eq` on `Config`.
    pub recency_weight: f32,
    /// T-60: ordered list of retriever plugin slugs. Each entry must
    /// match a known retriever impl: `"hybrid"`, `"entity-graph"`.
    /// Unknown slugs are loud failures at
    /// `RetrieverRegistry::from_config`, not silent no-ops.
    ///
    /// Default is `["hybrid"]` — preserves v0.1 behaviour exactly.
    /// Adding `"entity-graph"` opts in to fact-graph traversal
    /// alongside hybrid; the registry merges both via cross-
    /// retriever RRF.
    pub plugins: Vec<String>,
    /// T-73: per-kind recency-decay half-lives. Maps a kind name
    /// (`fact`, `preference`, `decision`, `constraint`, `todo`,
    /// `note`) to a duration string parsed via
    /// [`crate::journal_duration::parse_duration`]. The hybrid
    /// retriever uses `weight * 0.5^(age_days / half_life_days)` for
    /// captures whose kind is in this map; unknown kinds or empty
    /// strings fall back to the legacy uniform exp-decay
    /// (`exp(-age_days / 30)`) so backwards compat is preserved.
    /// Defaults follow Memento's `docs/architecture/decay-and-supersession.md`:
    /// fact=90d, preference=180d, decision=365d, constraint=180d,
    /// todo=14d, note=30d.
    pub decay_half_life: std::collections::BTreeMap<String, String>,
}
impl Default for RetrieverSection {
    fn default() -> Self {
        let mut decay = std::collections::BTreeMap::new();
        decay.insert("fact".to_string(), "90d".to_string());
        decay.insert("preference".to_string(), "180d".to_string());
        decay.insert("decision".to_string(), "365d".to_string());
        decay.insert("constraint".to_string(), "180d".to_string());
        decay.insert("todo".to_string(), "14d".to_string());
        decay.insert("note".to_string(), "30d".to_string());
        Self {
            recency_weight: crate::retriever::DEFAULT_RECENCY_WEIGHT,
            plugins: vec!["hybrid".to_string()],
            decay_half_life: decay,
        }
    }
}

impl RetrieverSection {
    /// T-73: produce the per-kind half-life map as `(kind → days)`
    /// ready for the retriever to consume. Each value string is
    /// parsed via the journal duration grammar (`45s`, `30m`, `1h`,
    /// `1d`, `2w`). Unparseable entries are dropped silently — a
    /// retriever that doesn't find a kind falls back to the uniform
    /// tau path, so a single bad entry doesn't break unrelated kinds.
    pub fn decay_half_lives_in_days(&self) -> std::collections::HashMap<String, f64> {
        let mut out = std::collections::HashMap::with_capacity(self.decay_half_life.len());
        for (k, v) in &self.decay_half_life {
            if let Some(days) = parse_duration_days(v) {
                out.insert(k.clone(), days);
            }
        }
        out
    }
}

/// Parse a short duration (`45s`, `30m`, `1h`, `1d`, `2w`) and
/// return its length in days as `f64`. Returns `None` on any parse
/// error so a bad entry in `[retriever].decay_half_life` falls back
/// to the uniform-tau path rather than crashing the retriever.
fn parse_duration_days(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let split_idx = s.find(|c: char| !c.is_ascii_digit())?;
    let (num_str, suffix) = s.split_at(split_idx);
    let n: f64 = num_str.parse().ok()?;
    if n < 0.0 {
        return None;
    }
    Some(match suffix {
        "s" => n / 86_400.0,
        "m" => n / 1_440.0,
        "h" => n / 24.0,
        "d" => n,
        "w" => n * 7.0,
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct EmbedderSection {
    pub model: String,
    pub backend: String,
}
impl Default for EmbedderSection {
    fn default() -> Self {
        Self {
            model: "bge-small-en-v1.5".into(),
            backend: "onnx".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PolicySection {
    pub default: String,
    pub user: String,
    pub llm_assist: bool,
}
impl Default for PolicySection {
    fn default() -> Self {
        Self {
            default: "policies/default.yaml".into(),
            user: "policies/user.yaml".into(),
            llm_assist: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ServerSection {
    pub addr: String,
    pub unix_socket: String,
}
impl Default for ServerSection {
    fn default() -> Self {
        Self {
            addr: DEFAULT_SERVER_ADDR.into(),
            unix_socket: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SyncSection {
    pub enabled: bool,
    pub endpoint: String,
    pub key_file: String,
}
impl Default for SyncSection {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            key_file: "keys/master.key".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct TelemetrySection {
    pub enabled: bool,
}

impl Config {
    /// Load `<home>/config.toml`. Missing file → returns defaults (not an
    /// error: an uninitialized home is still usable for read-only
    /// commands like `--help`). Parse failures DO error so a malformed
    /// file is loud, not silent.
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join(CONFIG_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read config at {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&text).with_context(|| format!("parse config at {}", path.display()))?;
        Ok(cfg.with_env_overrides())
    }

    /// Apply `LOCALMEM_<SECTION>_<KEY>` overrides. Only the keys SPEC.md
    /// calls out are wired in v0.1; future versions can extend this.
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(s) = std::env::var("LOCALMEM_SERVER_ADDR") {
            if !s.is_empty() {
                self.server.addr = s;
            }
        }
        if let Ok(s) = std::env::var("LOCALMEM_EMBEDDER_MODEL") {
            if !s.is_empty() {
                self.embedder.model = s;
            }
        }
        if let Ok(s) = std::env::var("LOCALMEM_REWRITER_MODE") {
            if !s.is_empty() {
                self.rewriter.mode = s;
            }
        }
        if let Ok(s) = std::env::var("LOCALMEM_HOME_USER_NAME") {
            if !s.is_empty() {
                self.home.user_name = s;
            }
        }
        if let Ok(s) = std::env::var("LOCALMEM_RETRIEVER_RECENCY_WEIGHT") {
            if !s.is_empty() {
                if let Ok(parsed) = s.parse::<f32>() {
                    self.retriever.recency_weight = parsed;
                }
            }
        }
        // T-58: comma-separated list of plugin slugs. We strip whitespace
        // around each entry so `"rules, local-llm"` works the same as
        // `"rules,local-llm"`. An entirely empty value is treated as
        // "unset" to match the rest of this function's discipline.
        if let Ok(s) = std::env::var("LOCALMEM_EXTRACTOR_PLUGINS") {
            let parsed: Vec<String> = s
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            if !parsed.is_empty() {
                self.extractor.plugins = parsed;
            }
        }
        if let Ok(s) = std::env::var("LOCALMEM_EXTRACTOR_LLM_MODEL") {
            if !s.is_empty() {
                self.extractor.llm_model = s;
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_config_returns_defaults() {
        let tmp = tempdir().unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(cfg.server.addr, DEFAULT_SERVER_ADDR);
        assert_eq!(cfg.embedder.model, "bge-small-en-v1.5");
        assert!(!cfg.sync.enabled);
        assert!(!cfg.telemetry.enabled);
    }

    #[test]
    fn loads_user_overrides_from_disk() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            r#"
[server]
addr = "127.0.0.1:9999"

[embedder]
model = "custom-model"
"#,
        )
        .unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(cfg.server.addr, "127.0.0.1:9999");
        assert_eq!(cfg.embedder.model, "custom-model");
        // Untouched sections keep their defaults.
        assert!(!cfg.sync.enabled);
    }

    #[test]
    fn env_var_overrides_disk() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[server]\naddr = \"disk:1234\"\n",
        )
        .unwrap();
        std::env::set_var("LOCALMEM_SERVER_ADDR", "env:5678");
        let cfg = Config::load(tmp.path()).unwrap();
        std::env::remove_var("LOCALMEM_SERVER_ADDR");
        assert_eq!(cfg.server.addr, "env:5678");
    }

    #[test]
    fn malformed_config_returns_error() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join(CONFIG_FILE), "not = valid = toml").unwrap();
        let err = Config::load(tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse config"), "got: {msg}");
    }

    #[test]
    fn rewriter_section_defaults_to_none_mode() {
        let tmp = tempdir().unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(cfg.rewriter.mode, "none");
        assert_eq!(cfg.home.user_name, "");
    }

    #[test]
    fn loads_rewriter_mode_and_user_name_from_disk() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            r#"
[home]
user_name = "Vijay"

[rewriter]
mode = "regex"
"#,
        )
        .unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(cfg.home.user_name, "Vijay");
        assert_eq!(cfg.rewriter.mode, "regex");
    }

    #[test]
    fn rewriter_mode_env_var_overrides_disk() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[rewriter]\nmode = \"regex\"\n",
        )
        .unwrap();
        std::env::set_var("LOCALMEM_REWRITER_MODE", "none");
        let cfg = Config::load(tmp.path()).unwrap();
        std::env::remove_var("LOCALMEM_REWRITER_MODE");
        assert_eq!(cfg.rewriter.mode, "none");
    }

    #[test]
    fn empty_env_var_does_not_override() {
        // An empty env var (LOCALMEM_SERVER_ADDR="") shouldn't blank out
        // the disk value; treat empty as "unset" to match the spirit of
        // shell-style optional vars.
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[server]\naddr = \"disk:9876\"\n",
        )
        .unwrap();
        std::env::set_var("LOCALMEM_SERVER_ADDR", "");
        let cfg = Config::load(tmp.path()).unwrap();
        std::env::remove_var("LOCALMEM_SERVER_ADDR");
        assert_eq!(cfg.server.addr, "disk:9876");
    }

    // ---- T-57: [retriever].recency_weight --------------------------

    #[test]
    fn retriever_section_defaults_to_v0_2_recency_weight() {
        let tmp = tempdir().unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(
            cfg.retriever.recency_weight,
            crate::retriever::DEFAULT_RECENCY_WEIGHT,
        );
    }

    #[test]
    fn loads_recency_weight_from_disk() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[retriever]\nrecency_weight = 0.05\n",
        )
        .unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(cfg.retriever.recency_weight, 0.05);
    }

    #[test]
    fn recency_weight_env_var_overrides_disk() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[retriever]\nrecency_weight = 0.05\n",
        )
        .unwrap();
        std::env::set_var("LOCALMEM_RETRIEVER_RECENCY_WEIGHT", "0.20");
        let cfg = Config::load(tmp.path()).unwrap();
        std::env::remove_var("LOCALMEM_RETRIEVER_RECENCY_WEIGHT");
        assert_eq!(cfg.retriever.recency_weight, 0.20);
    }

    // ---- T-58: [extractor] plugins list ----------------------------

    #[test]
    fn extractor_section_defaults_to_rules_only() {
        let tmp = tempdir().unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(cfg.extractor.plugins, vec!["rules".to_string()]);
        assert_eq!(cfg.extractor.llm_model, "llama3.2:3b");
        assert!(cfg.extractor.hosted_endpoint.is_empty());
        assert_eq!(
            cfg.extractor.custom_extractors_dir,
            "policies/extractors".to_string(),
        );
    }

    #[test]
    fn loads_custom_extractors_dir_from_disk() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            r#"
[extractor]
custom_extractors_dir = "alt/path"
"#,
        )
        .unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(cfg.extractor.custom_extractors_dir, "alt/path");
    }

    #[test]
    fn empty_custom_extractors_dir_disables_yaml_scan() {
        // An explicit empty string in config means "do not scan",
        // per the [`ExtractorRegistry::from_config_with_home`] contract.
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            r#"
[extractor]
custom_extractors_dir = ""
"#,
        )
        .unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert!(cfg.extractor.custom_extractors_dir.is_empty());
    }

    #[test]
    fn loads_extractor_plugins_from_disk() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            r#"
[extractor]
plugins = ["rules", "local-llm"]
llm_model = "qwen2.5:7b"
"#,
        )
        .unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(
            cfg.extractor.plugins,
            vec!["rules".to_string(), "local-llm".to_string()]
        );
        assert_eq!(cfg.extractor.llm_model, "qwen2.5:7b");
    }

    #[test]
    fn env_var_overrides_extractor_plugins_with_csv_grammar() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[extractor]\nplugins = [\"rules\"]\n",
        )
        .unwrap();
        // CSV with whitespace around each token; we tolerate.
        std::env::set_var(
            "LOCALMEM_EXTRACTOR_PLUGINS",
            "rules, local-llm , hosted",
        );
        let cfg = Config::load(tmp.path()).unwrap();
        std::env::remove_var("LOCALMEM_EXTRACTOR_PLUGINS");
        assert_eq!(
            cfg.extractor.plugins,
            vec![
                "rules".to_string(),
                "local-llm".to_string(),
                "hosted".to_string()
            ]
        );
    }

    #[test]
    fn empty_extractor_plugins_env_var_does_not_override() {
        // An empty CSV must not blank out the disk value (same
        // discipline as every other env-override in this module).
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[extractor]\nplugins = [\"rules\"]\n",
        )
        .unwrap();
        std::env::set_var("LOCALMEM_EXTRACTOR_PLUGINS", "  ,  ,  ");
        let cfg = Config::load(tmp.path()).unwrap();
        std::env::remove_var("LOCALMEM_EXTRACTOR_PLUGINS");
        assert_eq!(cfg.extractor.plugins, vec!["rules".to_string()]);
    }

    // ---- T-73: per-kind half-life parsing ---------------------------------

    #[test]
    fn retriever_section_defaults_have_spec_v0_2_half_lives() {
        let r = RetrieverSection::default();
        let m = r.decay_half_lives_in_days();
        assert_eq!(m.get("fact").copied(), Some(90.0));
        assert_eq!(m.get("preference").copied(), Some(180.0));
        assert_eq!(m.get("decision").copied(), Some(365.0));
        assert_eq!(m.get("constraint").copied(), Some(180.0));
        assert_eq!(m.get("todo").copied(), Some(14.0));
        assert_eq!(m.get("note").copied(), Some(30.0));
    }

    #[test]
    fn decay_half_lives_parses_supported_duration_units() {
        let mut r = RetrieverSection::default();
        r.decay_half_life.clear();
        r.decay_half_life.insert("fact".into(), "2w".into());
        r.decay_half_life.insert("preference".into(), "48h".into());
        r.decay_half_life.insert("note".into(), "1440m".into());
        r.decay_half_life.insert("todo".into(), "86400s".into());
        let m = r.decay_half_lives_in_days();
        assert_eq!(m.get("fact").copied(), Some(14.0));
        assert_eq!(m.get("preference").copied(), Some(2.0));
        assert_eq!(m.get("note").copied(), Some(1.0));
        assert_eq!(m.get("todo").copied(), Some(1.0));
    }

    #[test]
    fn decay_half_lives_silently_drops_bad_entries() {
        // A typo in one entry must not break unrelated kinds: the
        // retriever falls back to uniform tau for the broken kind
        // and keeps the others intact. Loud config errors require an
        // explicit `Config::load` failure path that we don't have
        // for this map, by design.
        let mut r = RetrieverSection::default();
        r.decay_half_life.clear();
        r.decay_half_life.insert("fact".into(), "90d".into());
        r.decay_half_life.insert("preference".into(), "not-a-duration".into());
        r.decay_half_life.insert("decision".into(), "".into());
        r.decay_half_life.insert("todo".into(), "-7d".into());
        let m = r.decay_half_lives_in_days();
        assert_eq!(m.get("fact").copied(), Some(90.0));
        assert!(!m.contains_key("preference"));
        assert!(!m.contains_key("decision"));
        assert!(!m.contains_key("todo"));
    }

    #[test]
    fn loads_decay_half_life_from_disk() {
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[retriever.decay_half_life]\nfact = \"45d\"\ntodo = \"7d\"\n",
        )
        .unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(
            cfg.retriever.decay_half_life.get("fact").map(String::as_str),
            Some("45d")
        );
        assert_eq!(
            cfg.retriever.decay_half_life.get("todo").map(String::as_str),
            Some("7d")
        );
    }

    #[test]
    fn malformed_recency_weight_env_var_is_ignored() {
        // A garbage env value must not panic and must not silently
        // zero the disk default. We tolerate the parse failure and
        // keep the disk value.
        let tmp = tempdir().unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            "[retriever]\nrecency_weight = 0.07\n",
        )
        .unwrap();
        std::env::set_var("LOCALMEM_RETRIEVER_RECENCY_WEIGHT", "not-a-number");
        let cfg = Config::load(tmp.path()).unwrap();
        std::env::remove_var("LOCALMEM_RETRIEVER_RECENCY_WEIGHT");
        assert_eq!(cfg.retriever.recency_weight, 0.07);
    }
}
