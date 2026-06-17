//! Text chunking for retrieval (SPEC-intelligence-v2 §2.6).
//!
//! North Star: the right context in the fewest tokens, without missing anything.
//! A large raw capture (a pasted conversation, a long note) must NOT be a single
//! retrieval unit, or one essay dominates a result set and a hit costs thousands
//! of tokens (measured: a 14K-token blob was a single, wrong hit). We split it
//! into overlapping windows so each is a sharp, cheap-to-return chunk. This
//! preserves the recall floor (everything stays findable, the embed-both reason)
//! while making any single hit small. We chunk the raw, we do NOT drop it.
//!
//! Tokens are approximated by WHITESPACE WORDS so the hot path needs no
//! tokenizer dependency; ~0.75 words per token means our word targets sit a bit
//! under the equivalent token counts, which is the safe direction (smaller
//! chunks). Splitting is on whitespace; chunks overlap so a fact spanning a
//! boundary is not lost.

/// Target words per chunk (~300 tokens).
pub const CHUNK_TARGET_WORDS: usize = 220;
/// Overlap between consecutive chunks (~55 tokens) so boundary-spanning facts
/// survive in at least one chunk.
pub const CHUNK_OVERLAP_WORDS: usize = 40;
/// Captures at or below this many words are returned whole (never chunked): they
/// are already a sharp retrieval unit, and chunking them adds rows for no gain.
pub const CHUNK_MIN_WORDS: usize = 320;

/// Split `text` into overlapping word-windows for indexing + embedding. A short
/// text returns a single chunk (the whole, trimmed text). Empty input returns an
/// empty vec. The union of chunks covers every word (recall floor); consecutive
/// chunks share `CHUNK_OVERLAP_WORDS`.
pub fn chunk_text(text: &str) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    if words.len() <= CHUNK_MIN_WORDS {
        return vec![words.join(" ")];
    }
    let step = CHUNK_TARGET_WORDS
        .saturating_sub(CHUNK_OVERLAP_WORDS)
        .max(1);
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let end = (i + CHUNK_TARGET_WORDS).min(words.len());
        out.push(words[i..end].join(" "));
        if end == words.len() {
            break;
        }
        i += step;
    }
    out
}

/// True when `text` is large enough to be chunked into more than one piece. Cheap
/// pre-check so callers can skip chunk plumbing for the common small case.
pub fn is_chunkable(text: &str) -> bool {
    text.split_whitespace().count() > CHUNK_MIN_WORDS
}

/// Max words in a RETURNED retrieval snippet. A hair above `CHUNK_TARGET_WORDS`
/// so a full chunk passes through uncut, while a lexical full-text hit can never
/// dump a 14K-token blob into an agent's context.
pub const SNIPPET_CAP_WORDS: usize = 260;

/// Truncate `text` to at most `max_words` words, appending an ellipsis when cut.
/// Bounds the token cost of a returned snippet; short content passes unchanged.
pub fn cap_words(text: &str, max_words: usize) -> String {
    let mut it = text.split_whitespace();
    let head: Vec<&str> = it.by_ref().take(max_words).collect();
    if it.next().is_some() {
        format!("{} …", head.join(" "))
    } else {
        head.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: usize) -> String {
        (0..n)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(chunk_text("").is_empty());
        assert!(chunk_text("   ").is_empty());
    }

    #[test]
    fn small_text_is_one_whole_chunk() {
        let t = "Vijay prefers local-first storage.";
        assert_eq!(chunk_text(t), vec![t.to_string()]);
        assert!(!is_chunkable(t));
    }

    #[test]
    fn at_threshold_not_chunked() {
        let t = words(CHUNK_MIN_WORDS);
        assert_eq!(
            chunk_text(&t).len(),
            1,
            "exactly MIN words is still one chunk"
        );
        assert!(!is_chunkable(&t));
    }

    #[test]
    fn large_text_splits_into_overlapping_windows() {
        let t = words(1000);
        let chunks = chunk_text(&t);
        assert!(
            chunks.len() > 1,
            "1000 words must split, got {}",
            chunks.len()
        );
        assert!(is_chunkable(&t));
        // Each chunk bounded by the target.
        for c in &chunks {
            assert!(c.split_whitespace().count() <= CHUNK_TARGET_WORDS);
        }
        // Consecutive chunks overlap (share the boundary words).
        let first_last: Vec<&str> = chunks[0]
            .split_whitespace()
            .rev()
            .take(CHUNK_OVERLAP_WORDS)
            .collect();
        let second: Vec<&str> = chunks[1]
            .split_whitespace()
            .take(CHUNK_OVERLAP_WORDS)
            .collect();
        assert!(
            first_last.iter().any(|w| second.contains(w)),
            "consecutive chunks should overlap"
        );
    }

    #[test]
    fn chunks_cover_every_word() {
        let t = words(1000);
        let chunks = chunk_text(&t);
        // The recall floor: every original word appears in at least one chunk.
        let joined = chunks.join(" ");
        for i in 0..1000 {
            assert!(joined.contains(&format!("w{i} ")) || joined.ends_with(&format!("w{i}")));
        }
    }
}
