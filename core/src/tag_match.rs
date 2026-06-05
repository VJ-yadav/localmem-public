//! Tag subset-match predicate shared by every store-level filter.
//!
//! Defined once because three call sites all need exactly the same
//! semantics ([`crate::lexical::LexicalIndex::search`] for capture-side
//! filtering, [`crate::retriever::HybridRetriever::search`] for
//! vec-hit filtering against the lex index's stored tags, and
//! [`crate::facts::FactsStore`] for facts inherited from their source
//! captures). Three independent copies would drift in subtle ways.
//!
//! The semantics: a hit passes the filter iff every `(key, value)`
//! pair in the filter is present on the hit's tags. Empty filter
//! trivially matches everything. Value comparison is exact-equality;
//! no wildcards, no case-folding. Wildcards are a future-task surface
//! tracked under the [retriever Filters] doc (T-60+).

use std::collections::BTreeMap;

/// Subset match: `tags` satisfies `filter` when every pair in `filter`
/// appears in `tags` with identical value. An empty filter always
/// matches; this keeps callers from branching on emptiness at every
/// call site.
pub fn matches(tags: &BTreeMap<String, String>, filter: &BTreeMap<String, String>) -> bool {
    filter
        .iter()
        .all(|(k, v)| tags.get(k).map(String::as_str) == Some(v.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn empty_filter_trivially_matches() {
        assert!(matches(&map(&[("project", "lm")]), &BTreeMap::new()));
        assert!(matches(&BTreeMap::new(), &BTreeMap::new()));
    }

    #[test]
    fn subset_match_passes() {
        let tags = map(&[("project", "lm"), ("topic", "tags"), ("client", "internal")]);
        assert!(matches(&tags, &map(&[("project", "lm")])));
        assert!(matches(
            &tags,
            &map(&[("project", "lm"), ("topic", "tags")])
        ));
    }

    #[test]
    fn missing_key_fails() {
        let tags = map(&[("project", "lm")]);
        assert!(!matches(&tags, &map(&[("missing", "x")])));
    }

    #[test]
    fn value_mismatch_fails() {
        let tags = map(&[("project", "lm")]);
        assert!(!matches(&tags, &map(&[("project", "other")])));
    }

    #[test]
    fn empty_tags_with_nonempty_filter_fails() {
        assert!(!matches(&BTreeMap::new(), &map(&[("project", "lm")])));
    }
}
