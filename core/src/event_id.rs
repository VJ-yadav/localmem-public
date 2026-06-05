//! Monotonic, sortable event identifier (ULID).
//!
//! Per ARCHITECTURE.md: every event gets a ULID. Lexicographic sort matches
//! temporal order, so `events.jsonl` is naturally time-ordered when written
//! in append order.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use ulid::Ulid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(Ulid);

impl EventId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for EventId {
    type Err = ulid::DecodeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ulid::from_str(s).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn monotonic_ordering() {
        // Two IDs created at least 2ms apart must sort in the order they were
        // created. ULID's lexicographic sort encodes timestamp first.
        let a = EventId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = EventId::new();
        assert!(a < b, "expected {} < {}", a, b);
    }

    #[test]
    fn serde_roundtrip() {
        let id = EventId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        // transparent serde: id serializes as a JSON string, not an object
        assert!(json.starts_with('"') && json.ends_with('"'));
        let parsed: EventId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, parsed);
    }

    #[test]
    fn string_parsing() {
        let id = EventId::new();
        let s = id.to_string();
        assert_eq!(s.len(), 26, "ULID Crockford encoding is 26 chars");
        let parsed: EventId = s.parse().expect("parse");
        assert_eq!(id, parsed);
    }

    #[test]
    fn unique_ids_burst() {
        // 1000 IDs from the same process must all be unique.
        let n = 1000;
        let ids: HashSet<EventId> = (0..n).map(|_| EventId::new()).collect();
        assert_eq!(ids.len(), n, "expected {n} unique IDs");
    }

    #[test]
    fn invalid_string_is_error() {
        let bad: Result<EventId, _> = "not-a-ulid".parse();
        assert!(bad.is_err());
    }
}
