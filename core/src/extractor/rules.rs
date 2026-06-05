//! Rule-based fact extractor (T-13, lifted behind the T-58 [`Extractor`]
//! trait at [`super`]).
//!
//! Pattern-matches capture content into `(subject, predicate, object)`
//! tuples. See ARCHITECTURE.md (Write policy) and TASKS.md task T-13.
//!
//! v0.1 shipped rule-based only behind a concrete `Extractor` struct;
//! v0.2's T-58 lifts the surface to a trait so local-LLM (Ollama) and
//! hosted impls can compose in parallel via the registry. The rule
//! logic itself is unchanged from v0.1.
//!
//! Rule ordering matters: the most specific pattern fires first so a
//! sentence like "I prefer functional Rust" produces `(user, prefers,
//! functional Rust)` rather than the catch-all `(I, is, prefer ...)`.
//! Each rule emits at most one fact for v0; multi-fact extraction belongs
//! to the LLM extractor.

use super::{ExtractedFact, Extractor};
use crate::kind::Kind;
use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;

/// Confidence we assign to anything the rule-based extractor matches.
/// Conservative on purpose: rule output is treated by the policy engine
/// (Group D) as evidence to commit, not a final truth claim. The cloud
/// LLM extractor will produce confidences in a different range.
pub const RULE_CONFIDENCE: f64 = 0.7;

/// Slug used in `[extractor].plugins` config and in `Extractor::name()`.
/// Single source so a future rename surfaces a compile error rather than
/// silently breaking config lookups.
pub const NAME: &str = "rules";

/// Compiled rule set. Constructing a [`RulesExtractor`] is cheap once but
/// non-trivial (regex compilation), so callers should hold one instance
/// for the lifetime of the server.
pub struct RulesExtractor {
    prefer: Regex,
    email: Regex,
    is: Regex,
}

impl RulesExtractor {
    /// Build the default rule set. Patterns are case-insensitive and anchored
    /// to the whole sentence so partial matches in the middle of a paragraph
    /// do not fire spurious facts.
    pub fn new() -> Self {
        // Anchored on both ends. `(?i)` enables case-insensitive matching.
        let prefer = Regex::new(r"(?i)^\s*I\s+prefer\s+(.+?)\s*$").expect("prefer regex");
        let email = Regex::new(r"(?i)^\s*My\s+email\s+is\s+(\S+?)\s*$").expect("email regex");
        // "X is Y": deliberately loose. Subject is one or more identifier
        // characters, then a single " is " (lower-case only to avoid eating
        // "Is" at the start of questions like "Is rust fast"), then object.
        let is = Regex::new(r"^\s*([A-Za-z][\w\- ]*?)\s+is\s+(.+?)\s*$").expect("is regex");
        Self { prefer, email, is }
    }

    /// Synchronous extraction. The trait method
    /// [`Extractor::extract`] wraps this so async callers see a
    /// uniform surface; the sync entry point stays available for tests
    /// and for any future caller that wants to skip the async hop.
    pub fn extract_sync(&self, text: &str) -> Vec<ExtractedFact> {
        let trimmed = text.trim();
        // Questions are noise for v0 rules — "What time is it?" looks like an
        // `X is Y` shape but contains no fact. We reject anything ending in
        // `?` before stripping punctuation so the catch-all rule cannot fire.
        if trimmed.ends_with('?') {
            return Vec::new();
        }
        let trimmed = trimmed.trim_end_matches('.').trim();
        if trimmed.is_empty() {
            return Vec::new();
        }

        // Most specific first. Email before prefer to handle phrasings like
        // "I prefer that my email is x@y.z" (unlikely, but cheap to be safe).
        if let Some(caps) = self.email.captures(trimmed) {
            let object = caps[1].trim().to_string();
            if !object.is_empty() {
                return vec![ExtractedFact {
                    subject: "user".into(),
                    predicate: "has_email".into(),
                    object,
                    confidence: RULE_CONFIDENCE,
                }];
            }
        }
        if let Some(caps) = self.prefer.captures(trimmed) {
            let object = caps[1].trim().to_string();
            if !object.is_empty() {
                return vec![ExtractedFact {
                    subject: "user".into(),
                    predicate: "prefers".into(),
                    object,
                    confidence: RULE_CONFIDENCE,
                }];
            }
        }
        if let Some(caps) = self.is.captures(trimmed) {
            let subject = caps[1].trim().to_string();
            let object = caps[2].trim().to_string();
            // Drop empties and first-person subjects: "I is busy" is more
            // likely garbage than a real fact, and a real "I prefer ..."
            // should have already gone through the prefer branch.
            let bad_subject = subject.is_empty()
                || subject.eq_ignore_ascii_case("i")
                || subject.eq_ignore_ascii_case("my");
            if !bad_subject && !object.is_empty() {
                return vec![ExtractedFact {
                    subject,
                    predicate: "is".into(),
                    object,
                    confidence: RULE_CONFIDENCE,
                }];
            }
        }
        Vec::new()
    }
}

impl Default for RulesExtractor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Extractor for RulesExtractor {
    fn name(&self) -> &str {
        NAME
    }

    async fn extract(&self, text: &str, _kind_hint: Option<&Kind>) -> Result<Vec<ExtractedFact>> {
        // Rules are sync and CPU-bound; just delegate. No `spawn_blocking`
        // because the regex execution is microseconds — pushing it to a
        // blocking thread would add more overhead than it saves.
        Ok(self.extract_sync(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> ExtractedFact {
        let facts = RulesExtractor::new().extract_sync(text);
        assert_eq!(
            facts.len(),
            1,
            "expected one fact for {text:?}, got {facts:?}"
        );
        facts.into_iter().next().unwrap()
    }

    fn none(text: &str) {
        let facts = RulesExtractor::new().extract_sync(text);
        assert!(
            facts.is_empty(),
            "expected no facts for {text:?}, got {facts:?}"
        );
    }

    #[test]
    fn prefer_rule_fires() {
        let f = one("I prefer functional Rust");
        assert_eq!(f.subject, "user");
        assert_eq!(f.predicate, "prefers");
        assert_eq!(f.object, "functional Rust");
        assert_eq!(f.confidence, RULE_CONFIDENCE);
    }

    #[test]
    fn prefer_rule_strips_trailing_period() {
        let f = one("I prefer functional Rust.");
        assert_eq!(f.object, "functional Rust");
    }

    #[test]
    fn prefer_rule_is_case_insensitive() {
        let f = one("i PREFER vim over emacs");
        assert_eq!(f.predicate, "prefers");
        assert_eq!(f.object, "vim over emacs");
    }

    #[test]
    fn email_rule_fires() {
        let f = one("My email is alice@example.com");
        assert_eq!(f.subject, "user");
        assert_eq!(f.predicate, "has_email");
        assert_eq!(f.object, "alice@example.com");
    }

    #[test]
    fn is_rule_fires_with_named_subject() {
        let f = one("Rust is a systems programming language");
        assert_eq!(f.subject, "Rust");
        assert_eq!(f.predicate, "is");
        assert_eq!(f.object, "a systems programming language");
    }

    #[test]
    fn is_rule_skips_first_person_subjects() {
        // "I is busy" matches the regex shape but is rejected; nothing
        // extracts because no other rule fits either.
        none("I is busy");
    }

    #[test]
    fn empty_input_extracts_nothing() {
        none("");
        none("   ");
        none(".");
    }

    #[test]
    fn unmatched_input_extracts_nothing() {
        none("What time is it?");
        none("Hello world");
    }

    #[test]
    fn prefer_wins_over_is_when_both_could_match() {
        let f = one("I prefer Rust");
        assert_eq!(f.predicate, "prefers");
    }

    #[test]
    fn extractor_is_reusable_across_calls() {
        // Regression: holding a single Extractor and calling extract()
        // many times must not deplete or panic.
        let ex = RulesExtractor::new();
        let inputs = [
            "I prefer Rust",
            "My email is a@b.c",
            "DuckDB is a column store",
            "hello",
        ];
        for t in inputs {
            let _ = ex.extract_sync(t);
        }
    }

    #[test]
    fn ten_sample_sentences_match_expected_extractions() {
        // T-13 acceptance: 10 sample sentences, expected extractions.
        type Case = (
            &'static str,
            Option<(&'static str, &'static str, &'static str)>,
        );
        let cases: &[Case] = &[
            (
                "I prefer functional Rust.",
                Some(("user", "prefers", "functional Rust")),
            ),
            ("I prefer vim", Some(("user", "prefers", "vim"))),
            (
                "My email is alice@example.com",
                Some(("user", "has_email", "alice@example.com")),
            ),
            (
                "My email is bob@corp.co.uk.",
                Some(("user", "has_email", "bob@corp.co.uk")),
            ),
            ("Rust is fast", Some(("Rust", "is", "fast"))),
            (
                "DuckDB is a column store",
                Some(("DuckDB", "is", "a column store")),
            ),
            (
                "Tantivy is a Lucene-style search engine",
                Some(("Tantivy", "is", "a Lucene-style search engine")),
            ),
            ("What time is it?", None),
            ("Hello world", None),
            ("", None),
        ];
        let ex = RulesExtractor::new();
        for (input, expected) in cases {
            let got = ex.extract_sync(input);
            match expected {
                None => assert!(got.is_empty(), "{input:?} should not extract, got {got:?}"),
                Some((s, p, o)) => {
                    assert_eq!(got.len(), 1, "{input:?} should extract one, got {got:?}");
                    assert_eq!(got[0].subject, *s, "subject mismatch for {input:?}");
                    assert_eq!(got[0].predicate, *p, "predicate mismatch for {input:?}");
                    assert_eq!(got[0].object, *o, "object mismatch for {input:?}");
                }
            }
        }
    }

    #[tokio::test]
    async fn trait_extract_matches_sync_extract() {
        // The async trait method is a thin shim over extract_sync.
        // Pin that contract so a future refactor that diverges them
        // (e.g. accidentally chunking text in extract) gets caught.
        let ex = RulesExtractor::new();
        let sync = ex.extract_sync("I prefer functional Rust");
        let async_out = ex.extract("I prefer functional Rust", None).await.unwrap();
        assert_eq!(sync, async_out);
    }
}
