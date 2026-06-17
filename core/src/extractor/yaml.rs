//! User-defined YAML extractors (T-59).
//!
//! Each `.yaml` file in `<home>/policies/extractors/` becomes its own
//! [`Extractor`] impl in the registry. Lets a user customise extraction
//! without writing Rust or running a local LLM.
//!
//! File schema:
//!
//! ```yaml
//! id: my-extractor                          # required; surfaced as `yaml:<id>` in name()
//! patterns:
//!   - regex: '(?i)^I prefer (.+?) over (.+?)$'
//!     fact:
//!       subject: 'user'
//!       predicate: 'prefers_over'
//!       object: '{{capture[1]}} (vs {{capture[2]}})'
//!       confidence: 0.75
//! ```
//!
//! Templates: `{{capture[N]}}` substitutes regex capture group N.
//! `N=0` is the whole match; `1..` are the parenthesised groups in the
//! regex. **Templates are validated at load time** — a reference to
//! `{{capture[3]}}` against a regex with only 2 groups is a load-time
//! failure, not a silent skip. No bandaid: a typo in the YAML must
//! surface before the binary serves a single write.
//!
//! Each pattern is tried in declaration order; the FIRST match fires
//! and the rest are skipped (matches the v0.1 rule-extractor's
//! "most specific first" discipline). One file produces at most one
//! `ExtractedFact` per `extract` call. Compose multiple files in the
//! registry for richer extraction.

use super::{ExtractedFact, Extractor};
use crate::kind::Kind;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Default confidence for user-authored patterns when the YAML
/// omits the field. Lower than the shipped rules' `0.7` because
/// user-authored regex hasn't been vetted by the project's test
/// suite. Bumps the bar for T-56 contradiction resolution (which
/// requires confidence ≥ 0.7).
pub const DEFAULT_YAML_CONFIDENCE: f64 = 0.6;

/// Slug prefix in `Extractor::name()`. The full name is
/// `yaml:<file-id>` so audit traces can distinguish multiple
/// user-authored extractors.
pub const NAME_PREFIX: &str = "yaml";

/// Default glob pattern (relative to `<home>`) that the registry
/// scans for custom YAML extractors. Lives here so a future spec
/// change updates one place.
pub const DEFAULT_GLOB: &str = "policies/extractors/*.yaml";

#[derive(Debug, Deserialize)]
struct YamlExtractorFile {
    id: String,
    patterns: Vec<YamlPattern>,
}

#[derive(Debug, Deserialize)]
struct YamlPattern {
    regex: String,
    fact: YamlFactTemplate,
}

#[derive(Debug, Deserialize)]
struct YamlFactTemplate {
    subject: String,
    predicate: String,
    object: String,
    #[serde(default = "default_confidence")]
    confidence: f64,
}

fn default_confidence() -> f64 {
    DEFAULT_YAML_CONFIDENCE
}

/// Loaded + validated YAML extractor. Ready to apply against an
/// arbitrary input string.
#[derive(Debug)]
pub struct YamlExtractor {
    /// `yaml:<id>` slug returned by `Extractor::name()`.
    name: String,
    /// Compiled patterns. Loading validates each regex compiles AND
    /// that every `{{capture[N]}}` reference in the template stays
    /// within `regex.captures_len()`.
    patterns: Vec<CompiledPattern>,
    /// Source path; surfaced in error messages so a user fixing a
    /// load failure can find the offending file.
    source: PathBuf,
}

#[derive(Debug)]
struct CompiledPattern {
    regex: Regex,
    subject: String,
    predicate: String,
    object: String,
    confidence: f64,
}

impl YamlExtractor {
    /// Parse + validate a single YAML file. Errors carry the file
    /// path so a user can fix the YAML directly from the message.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read_to_string(path)
            .with_context(|| format!("read YAML extractor at {}", path.display()))?;
        Self::from_str(&bytes, path).with_context(|| format!("parse {}", path.display()))
    }

    /// Parse + validate from an in-memory string. Pulled out so unit
    /// tests can exercise the validation paths without touching disk.
    /// `source` is the path used in diagnostic messages; pass any
    /// stable identifier for tests.
    pub fn from_str(bytes: &str, source: &Path) -> Result<Self> {
        let file: YamlExtractorFile = serde_yaml::from_str(bytes).context("deserialize YAML")?;
        if file.id.trim().is_empty() {
            bail!(
                "YAML extractor at {}: `id` must not be empty",
                source.display()
            );
        }
        if file.patterns.is_empty() {
            bail!(
                "YAML extractor at {}: `patterns` list is empty; \
                 the file must define at least one regex + fact template",
                source.display()
            );
        }
        let mut compiled = Vec::with_capacity(file.patterns.len());
        for (idx, p) in file.patterns.into_iter().enumerate() {
            let regex = Regex::new(&p.regex).with_context(|| {
                format!(
                    "{}: pattern[{idx}] regex {:?} failed to compile",
                    source.display(),
                    p.regex
                )
            })?;
            // captures_len() returns 1 (the whole match) + N capture
            // groups. Validate every template reference stays within
            // that bound so a load error is loud and pre-runtime.
            let max_capture_idx = regex.captures_len().saturating_sub(1);
            validate_template_captures(&p.fact.subject, max_capture_idx)
                .with_context(|| format!("{}: pattern[{idx}].fact.subject", source.display()))?;
            validate_template_captures(&p.fact.predicate, max_capture_idx)
                .with_context(|| format!("{}: pattern[{idx}].fact.predicate", source.display()))?;
            validate_template_captures(&p.fact.object, max_capture_idx)
                .with_context(|| format!("{}: pattern[{idx}].fact.object", source.display()))?;
            if !p.fact.confidence.is_finite() || p.fact.confidence < 0.0 || p.fact.confidence > 1.0
            {
                bail!(
                    "{}: pattern[{idx}].fact.confidence ({}) must be a finite number in [0.0, 1.0]",
                    source.display(),
                    p.fact.confidence
                );
            }
            compiled.push(CompiledPattern {
                regex,
                subject: p.fact.subject,
                predicate: p.fact.predicate,
                object: p.fact.object,
                confidence: p.fact.confidence,
            });
        }
        let name = format!("{NAME_PREFIX}:{id}", id = file.id);
        Ok(Self {
            name,
            patterns: compiled,
            source: source.to_path_buf(),
        })
    }

    /// Apply the patterns in order; return the first match's
    /// templated `ExtractedFact`. Returns `None` when no pattern
    /// matches (the registry collapses that to an empty Vec, same
    /// as the rules extractor on no-match).
    fn apply(&self, text: &str) -> Option<ExtractedFact> {
        for p in &self.patterns {
            if let Some(caps) = p.regex.captures(text) {
                let subject = render_template(&p.subject, &caps);
                let predicate = render_template(&p.predicate, &caps);
                let object = render_template(&p.object, &caps);
                if subject.is_empty() || predicate.is_empty() || object.is_empty() {
                    // A match that templates to an empty triple is
                    // useless; skip rather than emit a degenerate fact.
                    continue;
                }
                return Some(ExtractedFact {
                    subject,
                    predicate,
                    object,
                    confidence: p.confidence,
                });
            }
        }
        None
    }

    /// Path of the loaded YAML file; used in `localmem doctor` and
    /// debug output.
    pub fn source(&self) -> &Path {
        &self.source
    }
}

#[async_trait]
impl Extractor for YamlExtractor {
    fn name(&self) -> &str {
        &self.name
    }

    async fn extract(&self, text: &str, _kind_hint: Option<&Kind>) -> Result<Vec<ExtractedFact>> {
        Ok(self.apply(text).into_iter().collect())
    }
}

/// Walk every `.yaml` (and `.yml`) file under `dir` and load it as a
/// [`YamlExtractor`]. Returns the loaded extractors in lexicographic
/// path order so registry composition is deterministic across runs.
/// Missing directory → `Ok(vec![])` (user simply hasn't authored any
/// custom extractors yet). Any per-file load failure aborts the scan
/// with the file path in the error chain — no silent skip.
pub fn load_dir(dir: &Path) -> Result<Vec<YamlExtractor>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read YAML extractor dir {}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("iter YAML extractor dir {}", dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
            .unwrap_or(false);
        if !ext {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        out.push(YamlExtractor::load(&path)?);
    }
    Ok(out)
}

/// Reject `{{capture[N]}}` references in `template` where `N` is
/// greater than the regex's capture count. Returns `Ok(())` when
/// every reference is in range, `Err` naming the bad index otherwise.
/// Implementation is regex-free + tolerates non-template text; we
/// only care about well-formed `{{capture[N]}}` substrings.
fn validate_template_captures(template: &str, max_capture_idx: usize) -> Result<()> {
    let mut rest = template;
    while let Some(start) = rest.find("{{capture[") {
        let after_prefix = &rest[start + "{{capture[".len()..];
        let end_bracket = after_prefix
            .find("]}}")
            .ok_or_else(|| anyhow!("unterminated capture reference in template: {template:?}"))?;
        let num_str = &after_prefix[..end_bracket];
        let n: usize = num_str
            .parse()
            .map_err(|_| anyhow!("capture index {num_str:?} is not a non-negative integer"))?;
        if n > max_capture_idx {
            bail!(
                "capture[{n}] referenced but regex only has {max_capture_idx} \
                 capture group(s) (0 = whole match, 1..N = groups)"
            );
        }
        rest = &after_prefix[end_bracket + "]}}".len()..];
    }
    Ok(())
}

/// Render every `{{capture[N]}}` in `template` against `caps`. Missing
/// groups (regex matched but the optional group didn't fire) render as
/// the empty string — the caller drops empty triples upstream.
fn render_template(template: &str, caps: &regex::Captures<'_>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{capture[") {
        out.push_str(&rest[..start]);
        let after_prefix = &rest[start + "{{capture[".len()..];
        // `validate_template_captures` already verified the shape at
        // load time; an unwrap here would panic on user input we
        // already vetted. Still, code defensively: a future caller
        // who skips validation should get a graceful empty render.
        let Some(end_bracket) = after_prefix.find("]}}") else {
            out.push_str(rest);
            return out;
        };
        let num_str = &after_prefix[..end_bracket];
        let n: usize = match num_str.parse() {
            Ok(n) => n,
            Err(_) => {
                out.push_str(rest);
                return out;
            }
        };
        if let Some(m) = caps.get(n) {
            out.push_str(m.as_str());
        }
        rest = &after_prefix[end_bracket + "]}}".len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn src() -> PathBuf {
        PathBuf::from("test.yaml")
    }

    #[test]
    fn loads_minimal_valid_yaml() {
        let yaml = r#"
id: simple
patterns:
  - regex: '(?i)^I like (.+)$'
    fact:
      subject: 'user'
      predicate: 'likes'
      object: '{{capture[1]}}'
"#;
        let ex = YamlExtractor::from_str(yaml, &src()).unwrap();
        assert_eq!(ex.name(), "yaml:simple");
        let out = ex.apply("I like Rust").unwrap();
        assert_eq!(out.subject, "user");
        assert_eq!(out.predicate, "likes");
        assert_eq!(out.object, "Rust");
        // Default confidence applied when YAML omits it.
        assert!((out.confidence - DEFAULT_YAML_CONFIDENCE).abs() < 1e-9);
    }

    #[test]
    fn loads_multi_pattern_file_and_picks_first_match() {
        let yaml = r#"
id: multi
patterns:
  - regex: '(?i)^I prefer (.+)$'
    fact:
      subject: 'user'
      predicate: 'prefers'
      object: '{{capture[1]}}'
      confidence: 0.75
  - regex: '(?i)^(\w+) is (\w+)$'
    fact:
      subject: '{{capture[1]}}'
      predicate: 'is'
      object: '{{capture[2]}}'
"#;
        let ex = YamlExtractor::from_str(yaml, &src()).unwrap();
        let pref = ex.apply("I prefer haskell").unwrap();
        assert_eq!(pref.predicate, "prefers");
        assert!((pref.confidence - 0.75).abs() < 1e-9);
        let is = ex.apply("Rust is fast").unwrap();
        assert_eq!(is.subject, "Rust");
        assert_eq!(is.object, "fast");
        // No match → None.
        assert!(ex.apply("hello world").is_none());
    }

    #[test]
    fn template_capture_zero_refers_to_whole_match() {
        let yaml = r#"
id: whole-match
patterns:
  - regex: '(?i)^I prefer (.+)$'
    fact:
      subject: 'user'
      predicate: 'prefers'
      object: '{{capture[0]}}'
"#;
        let ex = YamlExtractor::from_str(yaml, &src()).unwrap();
        let out = ex.apply("I prefer functional Rust").unwrap();
        // capture[0] is the whole match — same as the input here.
        assert_eq!(out.object, "I prefer functional Rust");
    }

    #[test]
    fn rejects_invalid_regex_at_load() {
        let yaml = r#"
id: bad-regex
patterns:
  - regex: '['
    fact:
      subject: 'x'
      predicate: 'y'
      object: 'z'
"#;
        let err = YamlExtractor::from_str(yaml, &src()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("regex"), "got: {msg}");
        assert!(msg.contains("failed to compile"), "got: {msg}");
    }

    #[test]
    fn rejects_out_of_range_capture_reference_at_load() {
        // Regex has one capture group; template references capture[2].
        let yaml = r#"
id: oor-capture
patterns:
  - regex: '(?i)^I prefer (.+)$'
    fact:
      subject: 'user'
      predicate: 'prefers'
      object: '{{capture[2]}}'
"#;
        let err = YamlExtractor::from_str(yaml, &src()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("capture[2]"), "got: {msg}");
        assert!(msg.contains("1 capture group"), "got: {msg}");
    }

    #[test]
    fn rejects_empty_patterns_list() {
        let yaml = r#"
id: empty
patterns: []
"#;
        let err = YamlExtractor::from_str(yaml, &src()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "got: {msg}");
    }

    #[test]
    fn rejects_empty_id() {
        let yaml = r#"
id: ''
patterns:
  - regex: 'x'
    fact:
      subject: 'a'
      predicate: 'b'
      object: 'c'
"#;
        let err = YamlExtractor::from_str(yaml, &src()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("`id`"), "got: {msg}");
    }

    #[test]
    fn rejects_out_of_range_confidence() {
        let yaml = r#"
id: bad-conf
patterns:
  - regex: 'x'
    fact:
      subject: 'a'
      predicate: 'b'
      object: 'c'
      confidence: 1.5
"#;
        let err = YamlExtractor::from_str(yaml, &src()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("confidence"), "got: {msg}");
        assert!(msg.contains("1.5"), "got: {msg}");
    }

    #[test]
    fn drops_match_that_templates_to_empty_triple() {
        // Regex matches but the optional capture group is empty so
        // the object renders as "". The extractor must NOT emit a
        // degenerate `(subject, predicate, "")` fact.
        let yaml = r#"
id: optional-group
patterns:
  - regex: '(?i)^I prefer (?:(\w+))?$'
    fact:
      subject: 'user'
      predicate: 'prefers'
      object: '{{capture[1]}}'
"#;
        let ex = YamlExtractor::from_str(yaml, &src()).unwrap();
        // The match fires (whole regex matches) but capture[1] is None.
        // Render -> empty object -> dropped.
        assert!(ex.apply("I prefer").is_none());
        // Sanity: the regex does fire with a value.
        let with = ex.apply("I prefer rust").unwrap();
        assert_eq!(with.object, "rust");
    }

    #[tokio::test]
    async fn trait_impl_delegates_to_apply() {
        let yaml = r#"
id: trait-test
patterns:
  - regex: '(?i)^I prefer (.+)$'
    fact:
      subject: 'user'
      predicate: 'prefers'
      object: '{{capture[1]}}'
"#;
        let ex = YamlExtractor::from_str(yaml, &src()).unwrap();
        let out = ex.extract("I prefer Vim", None).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].predicate, "prefers");
        let none = ex.extract("hello", None).await.unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn load_dir_returns_empty_when_directory_missing() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("does-not-exist");
        let out = load_dir(&dir).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn load_dir_returns_empty_when_directory_empty() {
        let tmp = tempdir().unwrap();
        let out = load_dir(tmp.path()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn load_dir_loads_multiple_files_in_deterministic_order() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        // Out-of-order create; load order should be lex-sorted.
        std::fs::write(
            dir.join("zeta.yaml"),
            "id: zeta\npatterns: [{regex: 'x', fact: {subject: a, predicate: b, object: c}}]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("alpha.yml"),
            "id: alpha\npatterns: [{regex: 'x', fact: {subject: a, predicate: b, object: c}}]\n",
        )
        .unwrap();
        // Non-yaml file should be ignored.
        std::fs::write(dir.join("notes.txt"), "not yaml").unwrap();

        let out = load_dir(dir).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name(), "yaml:alpha");
        assert_eq!(out[1].name(), "yaml:zeta");
    }

    #[test]
    fn load_dir_bails_on_first_invalid_file() {
        // A directory with one good + one broken file must fail
        // loudly. We never want a silently-skipped extractor.
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("good.yaml"),
            "id: good\npatterns: [{regex: 'x', fact: {subject: a, predicate: b, object: c}}]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("broken.yaml"),
            "id: broken\npatterns: [{regex: '[', fact: {subject: a, predicate: b, object: c}}]\n",
        )
        .unwrap();
        let err = load_dir(dir).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("broken.yaml"),
            "error must name the bad file: {msg}"
        );
    }

    #[test]
    fn render_template_handles_no_placeholders() {
        // Template with no {{capture[N]}} should pass through unchanged.
        let re = Regex::new(r"^.*$").unwrap();
        let caps = re.captures("anything").unwrap();
        assert_eq!(render_template("static", &caps), "static");
    }

    #[test]
    fn render_template_interleaves_literal_and_capture() {
        let re = Regex::new(r"^(\w+) is (\w+)$").unwrap();
        let caps = re.captures("Rust is fast").unwrap();
        assert_eq!(
            render_template("{{capture[1]}} is described as {{capture[2]}}", &caps),
            "Rust is described as fast"
        );
    }
}
