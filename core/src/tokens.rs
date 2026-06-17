//! North Star token accounting (SPEC-intelligence-v2 §2.9).
//!
//! The North Star is the REAL cost of use: "the right context, in the fewest
//! tokens, without missing anything." For that number to mean anything it must
//! be the model's ACTUAL token count, not a word estimate, because GPT, Claude,
//! and local Llama models all tokenize differently and bill differently.
//!
//! We count with the target model's real tokenizer where we have it embedded
//! (the GPT family, via tiktoken; the BPE vocab is compiled in, so this stays
//! offline / local-first). For models whose tokenizer is not public (Claude) or
//! not embedded here (local Llama/Qwen), we fall back to the nearest embedded
//! BPE as a PROXY and flag the count inexact, so a caller can label it honestly
//! rather than pretend precision we do not have.

use std::sync::OnceLock;
use tiktoken_rs::{cl100k_base, o200k_base, CoreBPE};

/// Default model the token cost is accounted against when a caller does not name
/// one. gpt-4o is the common agent model and the benchmark's answering model, so
/// its tokenizer (o200k) is a sensible default unit. Overridable per request.
pub const DEFAULT_ACCOUNTING_MODEL: &str = "gpt-4o";

fn o200k() -> &'static CoreBPE {
    static B: OnceLock<CoreBPE> = OnceLock::new();
    B.get_or_init(|| o200k_base().expect("embedded o200k_base vocab"))
}

fn cl100k() -> &'static CoreBPE {
    static B: OnceLock<CoreBPE> = OnceLock::new();
    B.get_or_init(|| cl100k_base().expect("embedded cl100k_base vocab"))
}

/// Resolve the accounting model name to an embedded BPE and whether that BPE is
/// the model's EXACT tokenizer (true) or a documented proxy (false).
fn resolve(model: &str) -> (&'static CoreBPE, bool) {
    let m = model.to_ascii_lowercase();
    // GPT-4o / 4.1 / 5 and the o-series reasoning models use o200k_base (exact).
    if m.contains("gpt-4o")
        || m.contains("gpt-4.1")
        || m.contains("gpt-5")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
    {
        return (o200k(), true);
    }
    // Older GPT-4 / GPT-3.5 / text-embedding use cl100k_base (exact).
    if m.contains("gpt-4") || m.contains("gpt-3.5") || m.contains("text-embedding") {
        return (cl100k(), true);
    }
    // Claude / Llama / Qwen / unknown: no embedded BPE for these. o200k is a
    // close proxy (typically within ~10-15%); the count is flagged inexact.
    (o200k(), false)
}

/// True when [`count`] uses the named model's EXACT tokenizer (vs a proxy).
pub fn is_exact(model: &str) -> bool {
    resolve(model).1
}

/// Real token count of `text` for the given accounting `model`.
pub fn count(text: &str, model: &str) -> usize {
    resolve(model).0.encode_ordinary(text).len()
}

/// Sum of token counts across many texts under one model. Cheaper than calling
/// [`count`] per item because it resolves the tokenizer once.
pub fn count_many<I, S>(texts: I, model: &str) -> usize
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let (bpe, _) = resolve(model);
    texts
        .into_iter()
        .map(|t| bpe.encode_ordinary(t.as_ref()).len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_real_token_counts_not_word_counts() {
        // "tokenization" is one word but several BPE tokens; a word-count would
        // say 1, the real tokenizer says more. This is the whole point.
        let n = count("tokenization", "gpt-4o");
        assert!(n >= 2, "expected multi-token, got {n}");
    }

    #[test]
    fn gpt_models_use_exact_tokenizers() {
        assert!(is_exact("gpt-4o"));
        assert!(is_exact("gpt-4o-mini"));
        assert!(is_exact("gpt-4.1"));
        assert!(is_exact("o3-mini"));
        assert!(is_exact("gpt-4-turbo"));
        assert!(is_exact("gpt-3.5-turbo"));
    }

    #[test]
    fn non_gpt_models_are_proxy_not_exact() {
        assert!(!is_exact("claude-sonnet-4-6"));
        assert!(!is_exact("llama3.2:3b"));
        assert!(!is_exact("qwen3.5:4b"));
    }

    #[test]
    fn empty_text_is_zero_tokens() {
        assert_eq!(count("", "gpt-4o"), 0);
    }

    #[test]
    fn count_many_sums_individual_counts() {
        let a = count("hello world", "gpt-4o");
        let b = count("functional rust", "gpt-4o");
        let summed = count_many(["hello world", "functional rust"], "gpt-4o");
        assert_eq!(summed, a + b);
    }

    #[test]
    fn o200k_and_cl100k_can_differ() {
        // Different families can tokenize the same text differently; both must
        // produce a positive count without panicking.
        let s = "The quick brown fox jumps over 12,345 lazy dogs.";
        assert!(count(s, "gpt-4o") > 0);
        assert!(count(s, "gpt-4-turbo") > 0);
    }
}
