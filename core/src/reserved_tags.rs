//! Reserved-tag semantics (T-51c).
//!
//! SPEC_V0_2 "container-tag model" defines two reserved keys whose
//! presence changes query-time behavior. Every other tag key is
//! user-defined and inert. Both keys are still ordinary entries in
//! the capture's tag map; this module just encodes how to interpret
//! them.
//!
//! - `retention = ephemeral:<TTL>`: the capture (and its derived
//!   facts) ages out at `now - capture_ts > TTL` and disappears from
//!   every query path. Older `retention=permanent` (or absent) is
//!   the default; ephemeral memories are NOT deleted — they remain
//!   in `events.jsonl` for audit and replay — they just stop
//!   surfacing in `search`, `recall`, and `profile`.
//!
//! - `visibility = private`: the capture is hidden from the default
//!   read paths. Only an entity-only `recall(entity=X)` — i.e. one
//!   that has no free-text query and is acting as a deliberate
//!   audit pull — surfaces it. `visibility=surfaced` is the
//!   default; treat any other value as `private` would mislead
//!   users, so we match the spec verbatim.
//!
//! Why a separate module: the predicate is identical at every read
//! site (lex search, hybrid retriever vec pass, facts queries), and
//! collapsing it here keeps the spec-text-to-behavior mapping in one
//! place. The `[[tag-match]]`-style centralization mirrors what T-51b
//! did for the subset-match helper.

use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

/// Tag key reserved for retention behavior. Value form: `ephemeral:<duration>`.
pub const KEY_RETENTION: &str = "retention";
/// Tag key reserved for visibility behavior. Value form: `private` (or `surfaced` / absent).
pub const KEY_VISIBILITY: &str = "visibility";
/// Value of `visibility` that hides the capture from default read paths.
pub const VISIBILITY_PRIVATE: &str = "private";
/// Prefix of the `retention` value that gates ephemeral expiry.
pub const RETENTION_EPHEMERAL_PREFIX: &str = "ephemeral:";

/// Caller's visibility policy. `Default` is what `search`, `profile`,
/// and any query-driven `recall` use: `visibility=private` hits are
/// filtered out. `IncludePrivate` is the audit-grade exception for
/// `recall(entity=X)` with no free-text query — the user is
/// deliberately asking "show me everything you know about X,
/// including the private stuff."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Default,
    IncludePrivate,
}

/// Reserved-tag predicate: returns `true` when the capture (and its
/// derived facts) should surface at `now` under the given visibility
/// policy. Returns `false` when the capture is hidden due to a
/// reserved-tag rule.
///
/// - Ordering: visibility is checked first so a private capture that
///   is *also* ephemeral short-circuits to `false` without parsing
///   the TTL. The two rules are AND-of-pass: both must allow the hit.
/// - `now` is the caller's reference time, not the wall clock. Tests
///   inject a deterministic instant; production code passes
///   [`chrono::Utc::now`]. Passing it explicitly means a query
///   running at the day boundary is consistent across the lex and
///   facts paths even if they fire microseconds apart.
pub fn is_visible(
    tags: &BTreeMap<String, String>,
    capture_ts: DateTime<Utc>,
    now: DateTime<Utc>,
    visibility: Visibility,
) -> bool {
    if visibility == Visibility::Default
        && tags.get(KEY_VISIBILITY).map(String::as_str) == Some(VISIBILITY_PRIVATE)
    {
        return false;
    }
    if let Some(ttl) = tags.get(KEY_RETENTION).and_then(|s| parse_retention_ttl(s)) {
        if now.signed_duration_since(capture_ts) > ttl {
            return false;
        }
    }
    true
}

/// Parse `ephemeral:<duration>` where `<duration>` is `<n><unit>` and
/// `<unit>` is one of `s`, `m`, `h`, `d`, `w`. Returns `None` for
/// inputs that don't match the `ephemeral:` prefix; returns `None`
/// (not an error) for malformed durations so a typo in the tag value
/// degrades to "retention not set" rather than failing the query.
///
/// The unit set matches the journal CLI's `--since` parser (see
/// `core/src/server/routes.rs::parse_duration`) so users can re-use
/// the duration syntax they already know.
pub fn parse_retention_ttl(value: &str) -> Option<Duration> {
    let rest = value.strip_prefix(RETENTION_EPHEMERAL_PREFIX)?;
    parse_duration_str(rest)
}

fn parse_duration_str(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: i64 = num.parse().ok()?;
    match unit {
        "s" => Some(Duration::seconds(n)),
        "m" => Some(Duration::minutes(n)),
        "h" => Some(Duration::hours(n)),
        "d" => Some(Duration::days(n)),
        "w" => Some(Duration::weeks(n)),
        _ => None,
    }
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

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    // ---- parse_retention_ttl ------------------------------------------------

    #[test]
    fn parses_each_unit() {
        assert_eq!(
            parse_retention_ttl("ephemeral:30s"),
            Some(Duration::seconds(30))
        );
        assert_eq!(
            parse_retention_ttl("ephemeral:15m"),
            Some(Duration::minutes(15))
        );
        assert_eq!(
            parse_retention_ttl("ephemeral:24h"),
            Some(Duration::hours(24))
        );
        assert_eq!(parse_retention_ttl("ephemeral:7d"), Some(Duration::days(7)));
        assert_eq!(
            parse_retention_ttl("ephemeral:2w"),
            Some(Duration::weeks(2))
        );
    }

    #[test]
    fn non_ephemeral_value_returns_none() {
        // "permanent" is the spec default; not parsed as a TTL.
        assert!(parse_retention_ttl("permanent").is_none());
        // Free-form anything else: also not a TTL.
        assert!(parse_retention_ttl("forever").is_none());
    }

    #[test]
    fn malformed_duration_returns_none_not_error() {
        // Typos must not crash the query path; absence-of-TTL is the
        // graceful fallback.
        assert!(parse_retention_ttl("ephemeral:").is_none());
        assert!(parse_retention_ttl("ephemeral:abc").is_none());
        assert!(parse_retention_ttl("ephemeral:24x").is_none());
        assert!(parse_retention_ttl("ephemeral:-5h").is_some()); // negative ok; will appear expired immediately
    }

    // ---- is_visible: visibility ---------------------------------------------

    #[test]
    fn private_is_hidden_by_default() {
        let tags = map(&[("visibility", "private")]);
        assert!(!is_visible(&tags, ts(0), ts(1), Visibility::Default));
    }

    #[test]
    fn private_is_surfaced_when_include_private() {
        let tags = map(&[("visibility", "private")]);
        assert!(is_visible(&tags, ts(0), ts(1), Visibility::IncludePrivate));
    }

    #[test]
    fn missing_or_surfaced_visibility_passes_default() {
        assert!(is_visible(
            &BTreeMap::new(),
            ts(0),
            ts(1),
            Visibility::Default
        ));
        let tags = map(&[("visibility", "surfaced")]);
        assert!(is_visible(&tags, ts(0), ts(1), Visibility::Default));
    }

    // ---- is_visible: retention TTL -----------------------------------------

    #[test]
    fn ephemeral_within_ttl_passes() {
        let tags = map(&[("retention", "ephemeral:1h")]);
        let capture = ts(1_700_000_000);
        let now = capture + Duration::minutes(30);
        assert!(is_visible(&tags, capture, now, Visibility::Default));
    }

    #[test]
    fn ephemeral_past_ttl_is_hidden() {
        let tags = map(&[("retention", "ephemeral:1h")]);
        let capture = ts(1_700_000_000);
        let now = capture + Duration::hours(2);
        assert!(!is_visible(&tags, capture, now, Visibility::Default));
    }

    #[test]
    fn ephemeral_at_exact_ttl_passes() {
        // Half-open semantics: hits drop only when age strictly
        // exceeds TTL. Lets a `--since=now` query catch the boundary
        // exactly without flapping based on microsecond skew.
        let tags = map(&[("retention", "ephemeral:1h")]);
        let capture = ts(1_700_000_000);
        let now = capture + Duration::hours(1);
        assert!(is_visible(&tags, capture, now, Visibility::Default));
    }

    #[test]
    fn permanent_retention_never_expires() {
        let tags = map(&[("retention", "permanent")]);
        let capture = ts(1_700_000_000);
        // 100 years later, still visible.
        let now = capture + Duration::days(100 * 365);
        assert!(is_visible(&tags, capture, now, Visibility::Default));
    }

    #[test]
    fn malformed_retention_falls_back_to_no_ttl() {
        // Typos must not silently expire user data. The capture stays
        // visible until something else hides it.
        let tags = map(&[("retention", "ephemeral:not_a_duration")]);
        let capture = ts(1_700_000_000);
        let now = capture + Duration::days(365);
        assert!(is_visible(&tags, capture, now, Visibility::Default));
    }

    // ---- is_visible: both rules compose ------------------------------------

    #[test]
    fn private_short_circuits_before_ttl_parse() {
        // A private+ephemeral capture is hidden by default; the TTL
        // doesn't get a chance to override visibility. The behavior is
        // the same regardless of whether the capture is expired.
        let tags = map(&[("visibility", "private"), ("retention", "ephemeral:1h")]);
        let capture = ts(1_700_000_000);
        let now = capture + Duration::minutes(30); // within TTL
        assert!(!is_visible(&tags, capture, now, Visibility::Default));
    }

    #[test]
    fn include_private_still_respects_ttl() {
        // The audit path doesn't override TTL: expired memories stay
        // expired even on the audit recall.
        let tags = map(&[("visibility", "private"), ("retention", "ephemeral:1h")]);
        let capture = ts(1_700_000_000);
        let now = capture + Duration::hours(2); // past TTL
        assert!(!is_visible(&tags, capture, now, Visibility::IncludePrivate));
    }
}
