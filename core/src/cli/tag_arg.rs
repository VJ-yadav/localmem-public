//! Shared CLI helpers for parsing `--tags key=value,key=value` flags.
//!
//! The same input syntax appears on `localmem write` (T-51) and on
//! every read-side command (search, recall, profile) that accepts tag
//! filters. Centralising the parser here keeps the syntax single-sourced
//! so a future fix (e.g. quoted values containing commas) lands in one
//! place rather than at every flag site.

use anyhow::{bail, Result};
use std::collections::BTreeMap;

/// Parse a `key=value,key=value` string into an ordered tag map.
///
/// Behavior:
/// - `None` and `Some("")` both return an empty map.
/// - Whitespace around keys, values, and separators is trimmed.
/// - Empty entries (e.g. trailing comma) are skipped silently.
/// - Missing `=` or empty key are hard errors so users notice typos
///   instead of getting a silent "no filter applied".
/// - Duplicate keys keep the last-seen value (matches the natural
///   "rightmost override" convention CLI users expect).
pub fn parse_tags_arg(input: Option<&str>) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let Some(raw) = input else {
        return Ok(out);
    };
    for entry in raw.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (k, v) = match trimmed.split_once('=') {
            Some(pair) => pair,
            None => {
                bail!("tag {trimmed:?} is missing '='; expected key=value (e.g. project=localmem)")
            }
        };
        let key = k.trim();
        let value = v.trim();
        if key.is_empty() {
            bail!("tag entry {trimmed:?} has an empty key");
        }
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_and_empty_return_empty_map() {
        assert!(parse_tags_arg(None).unwrap().is_empty());
        assert!(parse_tags_arg(Some("")).unwrap().is_empty());
        assert!(parse_tags_arg(Some("   ")).unwrap().is_empty());
    }

    #[test]
    fn parses_single_pair() {
        let m = parse_tags_arg(Some("project=localmem")).unwrap();
        assert_eq!(m.get("project").map(String::as_str), Some("localmem"));
    }

    #[test]
    fn parses_multiple_pairs_with_whitespace() {
        let m = parse_tags_arg(Some("project=localmem, topic = async, client=internal")).unwrap();
        assert_eq!(m.get("project").map(String::as_str), Some("localmem"));
        assert_eq!(m.get("topic").map(String::as_str), Some("async"));
        assert_eq!(m.get("client").map(String::as_str), Some("internal"));
    }

    #[test]
    fn empty_value_is_allowed() {
        // `project=` sets the value to "" — useful for explicit "unset"
        // markers without inventing a separate syntax.
        let m = parse_tags_arg(Some("project=")).unwrap();
        assert_eq!(m.get("project").map(String::as_str), Some(""));
    }

    #[test]
    fn duplicate_key_keeps_last_value() {
        let m = parse_tags_arg(Some("project=old,project=new")).unwrap();
        assert_eq!(m.get("project").map(String::as_str), Some("new"));
    }

    #[test]
    fn missing_equals_is_an_error() {
        let err = parse_tags_arg(Some("just_a_word")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("missing '='"), "got: {msg}");
    }

    #[test]
    fn empty_key_is_an_error() {
        let err = parse_tags_arg(Some("=value")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty key"), "got: {msg}");
    }

    #[test]
    fn trailing_commas_are_skipped() {
        let m = parse_tags_arg(Some("project=lm,,topic=tags,")).unwrap();
        assert_eq!(m.len(), 2);
    }
}
