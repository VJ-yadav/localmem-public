//! Context rewriting at ingest (T-55).
//!
//! SPEC_V0_2 calls this "Context Rewriting": every capture is
//! rewritten to be self-contained before lex + vec indexing, so a
//! retrieved chunk reads correctly in isolation — "they prefer X"
//! becomes "Vijay prefers X", "my email" becomes "Vijay's email".
//! The original text stays in the event log for audit; the
//! rewritten text lives next to it on the capture event and is
//! what `lex.search` returns in its snippet.
//!
//! Four modes per SPEC_V0_2 "container-tag model" → "Context
//! Rewriting at ingest":
//!
//! - `none` (default): identity. Used when no LLM is available and
//!   the user opts out of the regex rewriter (or is happy with
//!   pronouns in their search results).
//! - `regex`: deterministic pronoun substitution. Fast, no
//!   dependencies, decent quality for first-person statements.
//!   This is the only mode that ships enabled out-of-the-box in
//!   v0.2 v1.
//! - `local-llm`: calls a local Ollama instance. Higher quality;
//!   requires the user to have run `ollama serve` and pulled the
//!   target model. Stubbed in v0.2 v1 (returns an error so config
//!   typos are loud); real implementation lands with T-62 fetch-
//!   model + extractor T-58.
//! - `hosted`: calls our Hosted Intelligence endpoint. Paid tier,
//!   API-key-gated. Deferred to v0.2.1 alongside T-68.
//!
//! Why a trait rather than an enum match per call site: the same
//! `Rewriter` is called from CLI write, server `/write`, and (in a
//! future task) `localmem rewrite <event-id>` for retroactive
//! rewrites. Boxed-dyn keeps the construction logic in one place.

use anyhow::{bail, Result};
use regex::Regex;
use std::sync::OnceLock;

/// Rewriter mode names recognised in `[rewriter].mode`. Centralised
/// so the parser and the error message share one source of truth.
pub const MODE_NONE: &str = "none";
pub const MODE_REGEX: &str = "regex";
pub const MODE_LOCAL_LLM: &str = "local-llm";
pub const MODE_HOSTED: &str = "hosted";

/// Default fallback name used when neither `[home].user_name` nor
/// `$USER` resolves to anything. Picked to read naturally in the
/// rewritten output ("the user prefers Rust" still parses).
pub const FALLBACK_USER_NAME: &str = "the user";

/// Trait every rewriter implementation satisfies. Takes immutable
/// `&self` because regex and LLM clients are reusable across calls;
/// callers hold a single instance per write pipeline.
pub trait Rewriter: Send + Sync {
    fn rewrite(&self, text: &str, user_name: &str) -> Result<String>;
    fn mode(&self) -> &'static str;
}

/// Construct a rewriter from a config mode string. Unknown modes
/// error rather than silently fall back to `none`, so a typo in
/// `config.toml` is loud.
pub fn build(mode: &str) -> Result<Box<dyn Rewriter>> {
    match mode {
        MODE_NONE => Ok(Box::new(NoneRewriter)),
        MODE_REGEX => Ok(Box::new(RegexRewriter::new())),
        MODE_LOCAL_LLM => Ok(Box::new(LocalLlmRewriter)),
        MODE_HOSTED => Ok(Box::new(HostedRewriter)),
        other => bail!(
            "unknown rewriter mode {other:?}; valid: {MODE_NONE}, {MODE_REGEX}, \
             {MODE_LOCAL_LLM}, {MODE_HOSTED}"
        ),
    }
}

/// Resolve the user name to substitute into rewritten captures.
/// Order: explicit config value > `USER` env > [`FALLBACK_USER_NAME`].
/// Empty strings at any layer fall through to the next.
pub fn resolve_user_name(configured: &str) -> String {
    if !configured.trim().is_empty() {
        return configured.trim().to_string();
    }
    if let Ok(v) = std::env::var("USER") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    FALLBACK_USER_NAME.to_string()
}

/// Identity rewriter. Returns the input verbatim. The default mode
/// so a fresh install behaves identically to v0.1 (no
/// silent-rewrite surprises until the user opts in).
pub struct NoneRewriter;

impl Rewriter for NoneRewriter {
    fn rewrite(&self, text: &str, _user_name: &str) -> Result<String> {
        Ok(text.to_string())
    }
    fn mode(&self) -> &'static str {
        MODE_NONE
    }
}

/// Deterministic pronoun substitution. The patterns target the
/// first-person, first-clause style that dominates capture text
/// from AI chat tools ("I prefer X", "my email is Y", "I avoid Z").
/// We deliberately don't try to handle third-person rewrites
/// ("they said X") because guessing the referent is exactly the
/// case the LLM mode exists to handle.
///
/// Compiled regexes live in a `OnceLock` because every `rewrite`
/// call uses the same patterns; rebuilding them per call would be
/// slower than the substitutions themselves.
pub struct RegexRewriter {
    patterns: &'static RegexPatterns,
}

struct RegexPatterns {
    i_word_boundary: Regex,
    my_word_boundary: Regex,
    me_word_boundary: Regex,
    mine_word_boundary: Regex,
}

fn patterns() -> &'static RegexPatterns {
    static PATTERNS: OnceLock<RegexPatterns> = OnceLock::new();
    // Word boundaries on both sides so we don't rewrite "myself"
    // into "Vijayself" or "iframe" into "Vijayframe". The case-
    // insensitive flag (?i) catches "I", "i", "My", "MY", etc., but
    // we replace with the configured user name verbatim so the
    // rewritten output capitalises the way users expect.
    PATTERNS.get_or_init(|| RegexPatterns {
        i_word_boundary: Regex::new(r"(?i)\bi\b").expect("static regex"),
        my_word_boundary: Regex::new(r"(?i)\bmy\b").expect("static regex"),
        me_word_boundary: Regex::new(r"(?i)\bme\b").expect("static regex"),
        mine_word_boundary: Regex::new(r"(?i)\bmine\b").expect("static regex"),
    })
}

impl RegexRewriter {
    pub fn new() -> Self {
        Self {
            patterns: patterns(),
        }
    }
}

impl Default for RegexRewriter {
    fn default() -> Self {
        Self::new()
    }
}

impl Rewriter for RegexRewriter {
    fn rewrite(&self, text: &str, user_name: &str) -> Result<String> {
        // Substitution order matters: "mine" (the noun) must be
        // matched before "my" (the determiner) or "my" would
        // partially-match the prefix and break the suffix. The
        // patterns are word-bounded so this ordering is defensive,
        // not strictly required, but it costs nothing.
        let user = user_name;
        let user_possessive = format!("{user}'s");
        let mut out = self
            .patterns
            .mine_word_boundary
            .replace_all(text, user_possessive.as_str())
            .into_owned();
        out = self
            .patterns
            .my_word_boundary
            .replace_all(&out, user_possessive.as_str())
            .into_owned();
        out = self
            .patterns
            .me_word_boundary
            .replace_all(&out, user)
            .into_owned();
        out = self
            .patterns
            .i_word_boundary
            .replace_all(&out, user)
            .into_owned();
        Ok(out)
    }
    fn mode(&self) -> &'static str {
        MODE_REGEX
    }
}

/// Stub for the local Ollama path. Returns an explicit error rather
/// than silently falling back to identity: the user asked for local-
/// llm mode, and if it isn't wired we want them to know. Real
/// implementation lands with T-62 (`fetch-model`) + T-58
/// (extractor plugin trait); both pieces share the same Ollama
/// HTTP client so building it here independently would be wasted
/// work.
pub struct LocalLlmRewriter;

impl Rewriter for LocalLlmRewriter {
    fn rewrite(&self, _text: &str, _user_name: &str) -> Result<String> {
        bail!(
            "rewriter mode = {MODE_LOCAL_LLM} is not yet wired in v0.2 v1; \
             set [rewriter].mode = \"{MODE_REGEX}\" (deterministic substitution) \
             or \"{MODE_NONE}\" (passthrough). Local LLM rewriting ships in a v0.2 follow-up."
        )
    }
    fn mode(&self) -> &'static str {
        MODE_LOCAL_LLM
    }
}

/// Stub for the hosted (paid) tier. Same shape as LocalLlmRewriter:
/// loud error when invoked. Hosted Intelligence is deferred to
/// v0.2.1 per the scope cut.
pub struct HostedRewriter;

impl Rewriter for HostedRewriter {
    fn rewrite(&self, _text: &str, _user_name: &str) -> Result<String> {
        bail!(
            "rewriter mode = {MODE_HOSTED} requires the Hosted Intelligence subscription \
             (deferred to v0.2.1). Switch [rewriter].mode to \"{MODE_REGEX}\" or \
             \"{MODE_NONE}\" for now."
        )
    }
    fn mode(&self) -> &'static str {
        MODE_HOSTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- mode dispatch ----

    #[test]
    fn build_returns_correct_mode_for_each_string() {
        assert_eq!(build(MODE_NONE).unwrap().mode(), MODE_NONE);
        assert_eq!(build(MODE_REGEX).unwrap().mode(), MODE_REGEX);
        assert_eq!(build(MODE_LOCAL_LLM).unwrap().mode(), MODE_LOCAL_LLM);
        assert_eq!(build(MODE_HOSTED).unwrap().mode(), MODE_HOSTED);
    }

    #[test]
    fn build_rejects_unknown_mode_with_listing() {
        // `unwrap_err()` requires `T: Debug` on the Ok branch but
        // `Box<dyn Rewriter>` doesn't impl Debug; pattern-match
        // directly to keep the test surface clean of debug requirements.
        match build("typoed") {
            Ok(_) => panic!("expected unknown-mode error"),
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("unknown rewriter mode"), "got: {msg}");
                assert!(
                    msg.contains(MODE_REGEX),
                    "should list valid modes, got: {msg}",
                );
            }
        }
    }

    // ---- user-name resolution ----

    #[test]
    fn user_name_prefers_explicit_configured_value() {
        // Set USER as well to prove the config wins over env.
        std::env::set_var("USER", "from-env");
        let n = resolve_user_name("Vijay");
        std::env::remove_var("USER");
        assert_eq!(n, "Vijay");
    }

    #[test]
    fn user_name_falls_back_to_env_when_config_empty() {
        std::env::set_var("USER", "envname");
        let n = resolve_user_name("");
        std::env::remove_var("USER");
        assert_eq!(n, "envname");
    }

    #[test]
    fn user_name_trims_whitespace_at_both_layers() {
        std::env::set_var("USER", "  spaced  ");
        let n = resolve_user_name("   ");
        std::env::remove_var("USER");
        assert_eq!(n, "spaced");
    }

    #[test]
    fn user_name_falls_back_to_constant_when_neither_set() {
        std::env::remove_var("USER");
        let n = resolve_user_name("");
        assert_eq!(n, FALLBACK_USER_NAME);
    }

    // ---- None ----

    #[test]
    fn none_rewriter_is_identity() {
        let r = NoneRewriter;
        assert_eq!(
            r.rewrite("I prefer Rust.", "Vijay").unwrap(),
            "I prefer Rust.",
        );
    }

    // ---- Regex pronoun substitution ----

    #[test]
    fn regex_rewrites_i_to_user_name_on_word_boundary() {
        let r = RegexRewriter::new();
        assert_eq!(
            r.rewrite("I prefer Rust.", "Vijay").unwrap(),
            "Vijay prefer Rust.",
        );
        // Case-insensitive: lowercase "i" also rewrites, useful for
        // chat captures that lose capitalisation.
        assert_eq!(
            r.rewrite("i prefer Rust.", "Vijay").unwrap(),
            "Vijay prefer Rust.",
        );
    }

    #[test]
    fn regex_rewrites_my_to_possessive() {
        let r = RegexRewriter::new();
        assert_eq!(
            r.rewrite("My email is x@y.com.", "Vijay").unwrap(),
            "Vijay's email is x@y.com.",
        );
    }

    #[test]
    fn regex_rewrites_me_and_mine() {
        let r = RegexRewriter::new();
        assert_eq!(
            r.rewrite("That belongs to me.", "Vijay").unwrap(),
            "That belongs to Vijay.",
        );
        assert_eq!(
            r.rewrite("That's mine.", "Vijay").unwrap(),
            "That's Vijay's.",
        );
    }

    #[test]
    fn regex_handles_multiple_substitutions_in_one_sentence() {
        let r = RegexRewriter::new();
        assert_eq!(
            r.rewrite("I love my desk and the cat loves me.", "Vijay")
                .unwrap(),
            "Vijay love Vijay's desk and the cat loves Vijay.",
        );
    }

    #[test]
    fn regex_respects_word_boundaries() {
        // "myself", "myth", "iframe", "ironical" must NOT be rewritten.
        let r = RegexRewriter::new();
        let input = "Imagine myself working on iron from the iframe.";
        let out = r.rewrite(input, "Vijay").unwrap();
        // None of the inner pronouns are word-boundary matches.
        assert!(!out.contains("Vijayself"));
        assert!(!out.contains("Vijayframe"));
        // The leading "Imagine" must stay intact (the `\bi\b` only
        // matches a standalone "i", not the "I" in "Imagine").
        assert!(out.starts_with("Imagine"));
    }

    #[test]
    fn regex_preserves_empty_string() {
        let r = RegexRewriter::new();
        assert_eq!(r.rewrite("", "Vijay").unwrap(), "");
    }

    #[test]
    fn regex_preserves_text_with_no_pronouns() {
        let r = RegexRewriter::new();
        let unchanged = "Stripe webhook signature verification failed on the re-encoded body.";
        assert_eq!(r.rewrite(unchanged, "Vijay").unwrap(), unchanged);
    }

    // ---- LocalLlm / Hosted are stubs that bail loudly ----

    #[test]
    fn local_llm_returns_clear_unsupported_error() {
        let err = LocalLlmRewriter.rewrite("anything", "Vijay").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not yet wired") && msg.contains(MODE_REGEX),
            "expected unsupported message pointing at regex mode, got: {msg}",
        );
    }

    #[test]
    fn hosted_returns_clear_subscription_error() {
        let err = HostedRewriter.rewrite("anything", "Vijay").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Hosted Intelligence") || msg.contains("v0.2.1"),
            "expected subscription / deferred message, got: {msg}",
        );
    }
}
