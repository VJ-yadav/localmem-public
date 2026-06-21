//! Temporal envelope: timezone-correct, precision-aware time for every capture
//! and fact.
//!
//! Invariant (the immutable-log + recomputable-derived invariants applied to time): the IMMUTABLE original is
//! `local_wall` + `iana_zone` (+ `offset`). `instant_utc` and `interval` are
//! DERIVED and recomputable by `localmem replay` when the IANA tz database
//! changes. Store the original, derive the rest. A bare UTC instant is never
//! enough: it bakes in the tz rules at write time and cannot survive DST or
//! political zone changes. Store the zone NAME (a pointer into tzdb), not an
//! offset.

use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Format used for the immutable `local_wall` field (naive local time).
const LOCAL_WALL_FMT: &str = "%Y-%m-%dT%H:%M:%S";

/// Precision of a recorded time. Import sources range from an exact instant
/// down to "only ordering known". Never fabricate an instant we do not have:
/// coarse precision leaves `instant_utc` null and bounds the `interval`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Granularity {
    #[default]
    Instant,
    Day,
    Month,
    Year,
    Unknown,
}

/// Distilled EDTF uncertainty for a recorded time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TimeConfidence {
    #[default]
    Certain,
    Uncertain,
    Approximate,
}

/// Half-open `[earliest, latest)` UTC bounds for reasoning over imprecise time
/// (Allen-style). A zero-width interval is an exact instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interval {
    pub earliest: DateTime<Utc>,
    pub latest: DateTime<Utc>,
}

/// The full temporal record for a capture or fact. Every field except
/// `granularity`, `confidence`, and `system_observed_time` is optional so the
/// envelope degrades gracefully: a native capture fills it completely, an
/// import fills only what the source provides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeEnvelope {
    // --- immutable original (source of truth, lives in events.jsonl) ---
    /// Original local wall-clock time as written, no zone applied
    /// (e.g. "2024-11-03T01:30:00").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_wall: Option<String>,
    /// IANA/Olson zone NAME (e.g. "America/New_York"). Never an offset,
    /// never an abbreviation like "EST".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iana_zone: Option<String>,
    /// Original numeric offset as reported (e.g. "-05:00"), kept for
    /// instant-vs-walltime disambiguation after a tzdb rule change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<String>,
    /// EDTF / partial-date string for reduced precision (e.g. "2024-03").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edtf: Option<String>,

    // --- derived (recomputable from the original on tzdb upgrade) ---
    /// UTC instant for sort/range queries. Null when precision is coarse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instant_utc: Option<DateTime<Utc>>,
    /// Half-open UTC bounds. Present for both exact (zero-width) and
    /// imprecise times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<Interval>,

    // --- precision + provenance (always present) ---
    pub granularity: Granularity,
    pub confidence: TimeConfidence,
    /// What the source itself claimed (may be wrong or absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_reported_time: Option<String>,
    /// When our capturer/importer observed it. Always trustworthy; the
    /// fallback when a source gives no usable time.
    pub system_observed_time: DateTime<Utc>,
    /// IANA tzdb version used to derive `instant_utc` / `interval`. Lets a
    /// later replay recompute correctly after a tzdb upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tzdb_version: Option<String>,
}

impl TimeEnvelope {
    /// Native capture happening now on this machine: full fidelity. Records the
    /// exact instant, the local wall time, the machine's IANA zone name and
    /// offset, and the tzdb version, so the original is recoverable forever.
    pub fn capture_now() -> Self {
        let now = Utc::now();
        let local = Local::now();
        Self {
            local_wall: Some(local.format("%Y-%m-%dT%H:%M:%S").to_string()),
            iana_zone: iana_time_zone::get_timezone().ok(),
            offset: Some(local.format("%:z").to_string()),
            edtf: None,
            instant_utc: Some(now),
            interval: Some(Interval {
                earliest: now,
                latest: now,
            }),
            granularity: Granularity::Instant,
            confidence: TimeConfidence::Certain,
            source_reported_time: None,
            system_observed_time: now,
            tzdb_version: Some(chrono_tz::IANA_TZDB_VERSION.to_string()),
        }
    }

    /// Minimal envelope when only a UTC instant is known (e.g. some imports
    /// that report epoch time without a zone). Marks the instant as certain at
    /// second granularity but records no original local time or zone.
    pub fn from_instant(instant: DateTime<Utc>) -> Self {
        Self {
            local_wall: None,
            iana_zone: None,
            offset: None,
            edtf: None,
            instant_utc: Some(instant),
            interval: Some(Interval {
                earliest: instant,
                latest: instant,
            }),
            granularity: Granularity::Instant,
            confidence: TimeConfidence::Certain,
            source_reported_time: None,
            system_observed_time: Utc::now(),
            tzdb_version: None,
        }
    }

    /// Best-effort UTC instant for ordering and range queries. Prefers the
    /// derived instant, falls back to the interval start, then to the
    /// always-present observation time.
    pub fn effective_instant(&self) -> DateTime<Utc> {
        self.instant_utc
            .or_else(|| self.interval.as_ref().map(|i| i.earliest))
            .unwrap_or(self.system_observed_time)
    }

    /// Recompute the UTC instant from the IMMUTABLE original (`local_wall` +
    /// `iana_zone`) using the bundled tzdb. This is the "store the original,
    /// derive the instant" operation that lets `localmem replay` produce
    /// correct instants even after a tzdb upgrade changes a zone's rules: the
    /// original local time and zone NAME never move, only the derived instant
    /// is recomputed.
    ///
    /// Returns `None` when the original local time or zone is absent or
    /// unparseable (e.g. an import that only gave us a UTC instant), or when
    /// the wall time falls in a spring-forward gap that never existed in that
    /// zone. Callers keep the prior instant in those cases rather than guess.
    pub fn recompute_instant(&self) -> Option<DateTime<Utc>> {
        let wall = self.local_wall.as_ref()?;
        let zone = self.iana_zone.as_ref()?;
        let naive = NaiveDateTime::parse_from_str(wall, LOCAL_WALL_FMT).ok()?;
        let tz = Tz::from_str(zone).ok()?;
        match tz.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
            // DST fall-back: the wall time maps to two real instants. The
            // stored original offset is exactly what disambiguates which one
            // the user meant; absent that, prefer the earliest.
            chrono::LocalResult::Ambiguous(earliest, latest) => {
                Some(self.disambiguate(earliest, latest).with_timezone(&Utc))
            }
            chrono::LocalResult::None => None,
        }
    }

    fn disambiguate(&self, earliest: DateTime<Tz>, latest: DateTime<Tz>) -> DateTime<Tz> {
        if let Some(off) = &self.offset {
            if earliest.format("%:z").to_string() == *off {
                return earliest;
            }
            if latest.format("%:z").to_string() == *off {
                return latest;
            }
        }
        earliest
    }

    /// Return a copy with `instant_utc` (and the zero-width `interval`)
    /// refreshed from the immutable original via [`Self::recompute_instant`],
    /// stamped with the current bundled tzdb version. Used by replay after a
    /// tzdb upgrade. A no-op when the original cannot be recomputed.
    pub fn with_recomputed_instant(mut self) -> Self {
        if let Some(instant) = self.recompute_instant() {
            self.instant_utc = Some(instant);
            if self.granularity == Granularity::Instant {
                self.interval = Some(Interval {
                    earliest: instant,
                    latest: instant,
                });
            }
            self.tzdb_version = Some(chrono_tz::IANA_TZDB_VERSION.to_string());
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_now_is_full_fidelity() {
        let env = TimeEnvelope::capture_now();
        assert!(env.instant_utc.is_some());
        assert!(env.local_wall.is_some());
        assert!(env.offset.is_some());
        assert_eq!(env.granularity, Granularity::Instant);
        assert_eq!(env.confidence, TimeConfidence::Certain);
        assert!(env.tzdb_version.is_some());
    }

    #[test]
    fn absent_optional_fields_are_skipped_on_wire() {
        // from_instant leaves local_wall/iana_zone/offset/edtf/tzdb_version
        // unset; they must not appear in the serialized JSON.
        let env = TimeEnvelope::from_instant(Utc::now());
        let json = serde_json::to_value(&env).unwrap();
        assert!(json.get("local_wall").is_none());
        assert!(json.get("iana_zone").is_none());
        assert!(json.get("offset").is_none());
        assert!(json.get("edtf").is_none());
        assert!(json.get("instant_utc").is_some());
        assert!(json.get("system_observed_time").is_some());
    }

    #[test]
    fn roundtrips() {
        let env = TimeEnvelope::capture_now();
        let json = serde_json::to_string(&env).unwrap();
        let parsed: TimeEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env, parsed);
    }

    #[test]
    fn effective_instant_falls_back() {
        let t = Utc::now();
        let mut env = TimeEnvelope::from_instant(t);
        assert_eq!(env.effective_instant(), t);
        env.instant_utc = None;
        // interval start still present
        assert_eq!(env.effective_instant(), t);
    }

    fn wall_zone(wall: &str, zone: &str, offset: Option<&str>) -> TimeEnvelope {
        let mut env = TimeEnvelope::from_instant(Utc::now());
        env.local_wall = Some(wall.to_string());
        env.iana_zone = Some(zone.to_string());
        env.offset = offset.map(|o| o.to_string());
        env
    }

    #[test]
    fn recompute_instant_summer_new_york() {
        // June: America/New_York is EDT (-04:00), so 12:00 local = 16:00 UTC.
        let env = wall_zone("2024-06-15T12:00:00", "America/New_York", Some("-04:00"));
        let got = env.recompute_instant().unwrap();
        assert_eq!(got.to_rfc3339(), "2024-06-15T16:00:00+00:00");
    }

    #[test]
    fn recompute_instant_kolkata_half_hour_offset() {
        // Asia/Kolkata is +05:30 year-round, so 12:00 local = 06:30 UTC.
        let env = wall_zone("2024-01-15T12:00:00", "Asia/Kolkata", Some("+05:30"));
        let got = env.recompute_instant().unwrap();
        assert_eq!(got.to_rfc3339(), "2024-01-15T06:30:00+00:00");
    }

    #[test]
    fn recompute_instant_dst_fallback_uses_stored_offset() {
        // 2024-11-03 01:30 in America/New_York is AMBIGUOUS (the fall-back
        // hour repeats). The stored offset is what disambiguates:
        //   -04:00 (EDT) => 05:30 UTC, -05:00 (EST) => 06:30 UTC.
        let edt = wall_zone("2024-11-03T01:30:00", "America/New_York", Some("-04:00"));
        assert_eq!(
            edt.recompute_instant().unwrap().to_rfc3339(),
            "2024-11-03T05:30:00+00:00"
        );
        let est = wall_zone("2024-11-03T01:30:00", "America/New_York", Some("-05:00"));
        assert_eq!(
            est.recompute_instant().unwrap().to_rfc3339(),
            "2024-11-03T06:30:00+00:00"
        );
    }

    #[test]
    fn recompute_instant_none_without_original() {
        // from_instant has no local_wall/iana_zone, so there is nothing to
        // recompute from.
        let env = TimeEnvelope::from_instant(Utc::now());
        assert!(env.recompute_instant().is_none());
    }

    #[test]
    fn with_recomputed_instant_refreshes_and_stamps_tzdb() {
        let env = wall_zone("2024-06-15T12:00:00", "America/New_York", Some("-04:00"));
        let refreshed = env.with_recomputed_instant();
        assert_eq!(
            refreshed.instant_utc.unwrap().to_rfc3339(),
            "2024-06-15T16:00:00+00:00"
        );
        assert!(refreshed.tzdb_version.is_some());
    }
}
