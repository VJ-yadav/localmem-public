//! Closed-core kind taxonomy (T-52).
//!
//! SPEC_V0_2 "Kind taxonomy" defines six canonical kinds that carry
//! semantic behavior across the read paths. Anything else
//! round-trips verbatim under [`Kind::Other`] and is treated as a
//! note (the catch-all default) by every behavioral rule.
//!
//! Why an enum-with-catch-all instead of a strict enum: tools that
//! send localmem an unknown kind value (an experiment, a typo, a
//! v0.3 kind we haven't defined yet) must not crash the write
//! pipeline. The catch-all lets us preserve the kind string in the
//! event log for future replay while still applying note-equivalent
//! semantics today.
//!
//! Each canonical kind's semantics:
//!
//! - `fact`: bitemporal claim, subject-predicate-object triple.
//!   Smart forgetting (T-56) retires the prior `(subject, predicate)`
//!   row when a new high-confidence fact arrives.
//! - `preference`: same lifecycle as fact for contradiction
//!   resolution. The kind exists separately so profile rendering
//!   can list preferences in their own section.
//! - `decision`: append-only audit trail. Smart forgetting (T-56)
//!   never retires a decision because the history of choices is
//!   the point — "we chose X because Y" is not invalidated when
//!   the team later picks differently.
//! - `constraint`: rules that bound future work. Same smart-
//!   forgetting eligibility as preferences; profile lists them as
//!   their own section.
//! - `todo`: actionable item with a `done` state. v0.2 v1 carries
//!   the kind and a `done` flag on the capture; T-52b will wire
//!   the `localmem todo done <id>` update command.
//! - `note`: catch-all default. No extraction rules fire; profile
//!   surfaces it under "other" and never groups it with the
//!   structured kinds.

use serde::{Deserialize, Serialize};

/// Canonical kind names recognised in payloads. Centralised so the
/// `from_str` parser, the display impl, and the JSON serialization
/// share one source of truth.
pub const STR_FACT: &str = "fact";
pub const STR_PREFERENCE: &str = "preference";
pub const STR_DECISION: &str = "decision";
pub const STR_CONSTRAINT: &str = "constraint";
pub const STR_TODO: &str = "todo";
pub const STR_NOTE: &str = "note";

/// Capture / fact kind. Six canonical variants plus a catch-all for
/// extension kinds. The catch-all variant preserves the original
/// string for round-trip + future-binary-compat — when a later
/// localmem version learns about that kind, replay can re-interpret
/// it without losing the source data.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Kind {
    Fact,
    Preference,
    Decision,
    Constraint,
    Todo,
    #[default]
    Note,
    /// Any kind value that isn't one of the canonical six. Treated
    /// as note-equivalent for behavioral purposes; the original
    /// string is preserved.
    Other(String),
}

impl Kind {
    /// Canonical string form. Used on the wire and in display
    /// output. `Other(s)` returns `s` unchanged so the round-trip
    /// preserves the user's value exactly.
    pub fn as_str(&self) -> &str {
        match self {
            Kind::Fact => STR_FACT,
            Kind::Preference => STR_PREFERENCE,
            Kind::Decision => STR_DECISION,
            Kind::Constraint => STR_CONSTRAINT,
            Kind::Todo => STR_TODO,
            Kind::Note => STR_NOTE,
            Kind::Other(s) => s.as_str(),
        }
    }

    /// True when this kind is one of the six canonical variants.
    /// Extension kinds return false; behavioral code treats them as
    /// note-equivalent.
    pub fn is_canonical(&self) -> bool {
        !matches!(self, Kind::Other(_))
    }

    /// True when this kind equals [`Kind::Note`] (the default).
    /// Used as a `skip_serializing_if` predicate on payload fields
    /// so unset/default kinds stay absent on the wire and v0.1
    /// fixtures keep byte-identical serialization.
    pub fn is_note(&self) -> bool {
        matches!(self, Kind::Note)
    }

    /// True when smart forgetting (T-56) should retire prior
    /// `(subject, predicate)` rows. Decisions never retire because
    /// the audit trail of choices is the point; everything else
    /// (including `Other`) is eligible for contradiction resolution.
    ///
    /// `Note` and `Todo` aren't really fact-shaped, so in practice
    /// the contradiction check won't fire for them (they don't
    /// extract subject-predicate-object triples). The predicate is
    /// defensive: even if a future extractor produced a note-kinded
    /// fact, we still want contradiction resolution to run.
    pub fn allows_contradiction_resolution(&self) -> bool {
        !matches!(self, Kind::Decision)
    }
}

impl From<String> for Kind {
    fn from(s: String) -> Self {
        match s.as_str() {
            STR_FACT => Kind::Fact,
            STR_PREFERENCE => Kind::Preference,
            STR_DECISION => Kind::Decision,
            STR_CONSTRAINT => Kind::Constraint,
            STR_TODO => Kind::Todo,
            STR_NOTE => Kind::Note,
            _ => Kind::Other(s),
        }
    }
}

impl From<Kind> for String {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Other(s) => s,
            other => other.as_str().to_string(),
        }
    }
}

impl std::str::FromStr for Kind {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Owned via to_string because `From<String>` already does
        // the right thing for the canonical-vs-other dispatch.
        Ok(Kind::from(s.to_string()))
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_kind_is_note() {
        assert_eq!(Kind::default(), Kind::Note);
    }

    #[test]
    fn canonical_string_round_trips_through_serde() {
        for canonical in [
            Kind::Fact,
            Kind::Preference,
            Kind::Decision,
            Kind::Constraint,
            Kind::Todo,
            Kind::Note,
        ] {
            let s = serde_json::to_string(&canonical).unwrap();
            let back: Kind = serde_json::from_str(&s).unwrap();
            assert_eq!(canonical, back, "round-trip lost {canonical:?} via {s}");
        }
    }

    #[test]
    fn unknown_kind_round_trips_as_other_preserving_string() {
        // Future kinds + typos must NOT crash. The original string
        // survives the round trip so a later binary can re-interpret it.
        let raw = "\"experimental_kind_xyz\"";
        let parsed: Kind = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed, Kind::Other("experimental_kind_xyz".into()));
        let back = serde_json::to_string(&parsed).unwrap();
        assert_eq!(
            back, raw,
            "Other must serialize back to its original string"
        );
    }

    #[test]
    fn from_str_matches_serde_for_every_input() {
        for input in [
            "fact",
            "preference",
            "decision",
            "constraint",
            "todo",
            "note",
            "x",
        ] {
            let via_serde: Kind = serde_json::from_str(&format!("\"{input}\"")).unwrap();
            let via_fromstr: Kind = input.parse().unwrap();
            assert_eq!(via_serde, via_fromstr);
        }
    }

    #[test]
    fn is_canonical_distinguishes_known_from_other() {
        assert!(Kind::Fact.is_canonical());
        assert!(Kind::Preference.is_canonical());
        assert!(Kind::Decision.is_canonical());
        assert!(Kind::Constraint.is_canonical());
        assert!(Kind::Todo.is_canonical());
        assert!(Kind::Note.is_canonical());
        assert!(!Kind::Other("anything".into()).is_canonical());
    }

    #[test]
    fn allows_contradiction_resolution_is_false_only_for_decision() {
        assert!(Kind::Fact.allows_contradiction_resolution());
        assert!(Kind::Preference.allows_contradiction_resolution());
        assert!(!Kind::Decision.allows_contradiction_resolution());
        assert!(Kind::Constraint.allows_contradiction_resolution());
        assert!(Kind::Todo.allows_contradiction_resolution());
        assert!(Kind::Note.allows_contradiction_resolution());
        // Extension kinds: by default eligible. A future kind that
        // wants append-only semantics will need to opt in here.
        assert!(Kind::Other("custom".into()).allows_contradiction_resolution());
    }

    #[test]
    fn as_str_returns_the_inner_value_for_other() {
        assert_eq!(Kind::Other("recipe".into()).as_str(), "recipe");
    }

    #[test]
    fn display_matches_as_str() {
        for k in [Kind::Fact, Kind::Other("recipe".into())] {
            assert_eq!(format!("{k}"), k.as_str());
        }
    }
}
