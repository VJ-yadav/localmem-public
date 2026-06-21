//! Layer 2: the understanding layer (the unified memory-layer design).
//!
//! Layer 1 (events.jsonl) is the lossless source of truth. This layer sits on
//! TOP of it and is entirely DERIVED + recomputable by `localmem replay`. It
//! has two kinds of output:
//!
//!   A. per-capture decomposition  -> `decompose` (this increment)
//!   B/C. corpus-level synthesis    -> the Session Boot Briefing + the user
//!        persona, which AGGREGATE many decompositions (later increments).
//!
//! Two invariants hold for everything in here:
//!
//! * It runs ASYNC, off the write path, NEVER inside a capture hook (the
//!   hook-recursion footgun). The hook stays a dumb-fast raw write.
//! * It is platform-agnostic. It only ever sees capture text plus an opaque
//!   `source` label, so an ever-expanding set of AI tools all flow through the
//!   same path with no per-tool branching.

pub mod briefing;
pub mod decompose;
pub mod ollama;
pub mod remote;

pub use briefing::{
    read_briefing_cache, write_briefing_cache, Briefing, OllamaSynthesizer, Synthesizer,
};
pub use decompose::{
    decompose_system_prompt, parse_decomposition, DecomposeOptions, DecomposedEntity, Decomposition,
};
pub use ollama::{installed_models, resolve_model, Decomposer, ModelResolution, OllamaDecomposer};
pub use remote::{build_decomposer, AnthropicDecomposer, OpenAiDecomposer};

use crate::event::{Event, EventKind};
use std::collections::HashSet;

/// Coverage of the understanding layer: how many SIGNAL captures have at least
/// one `Understanding` event derived from them. The denominator excludes
/// ephemeral tool-traces (they never seed understanding by design — P1 hygiene).
/// The point is to never be silently idle: surface the gap so a stale 8% is
/// visible and actionable (`localmem understand --backfill`) instead of unseen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    pub decomposed: usize,
    pub signal_captures: usize,
}

impl Coverage {
    /// Whole-percent coverage; an empty store is reported as 100% (nothing owed).
    pub fn percent(&self) -> u32 {
        if self.signal_captures == 0 {
            100
        } else {
            ((self.decomposed * 100) / self.signal_captures) as u32
        }
    }

    pub fn undecomposed(&self) -> usize {
        self.signal_captures.saturating_sub(self.decomposed)
    }
}

/// Compute coverage from an event stream. Pure (no I/O) so it is unit-testable
/// without a live store; the CLI feeds it `EventLog::iter()`.
pub fn compute_coverage(events: impl Iterator<Item = Event>) -> Coverage {
    let mut signal: HashSet<String> = HashSet::new();
    let mut understood: HashSet<String> = HashSet::new();
    for e in events {
        match &e.kind {
            EventKind::Capture(p) if !p.is_ephemeral() => {
                signal.insert(e.id.to_string());
            }
            EventKind::Understanding(u) => {
                understood.insert(u.source_id.to_string());
            }
            _ => {}
        }
    }
    let decomposed = signal.iter().filter(|id| understood.contains(*id)).count();
    Coverage {
        decomposed,
        signal_captures: signal.len(),
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::event::{CapturePayload, Source, UnderstandingPayload};
    use chrono::Utc;
    use serde_json::Map;
    use std::collections::BTreeMap;

    fn src() -> Source {
        Source {
            app: "t".into(),
            host: "t".into(),
            user: None,
        }
    }

    fn capture(text: &str, ephemeral: bool) -> Event {
        let mut tags = BTreeMap::new();
        if ephemeral {
            tags.insert(
                crate::reserved_tags::KEY_RETENTION.to_string(),
                "ephemeral:7d".to_string(),
            );
        }
        Event::new(
            EventKind::Capture(CapturePayload {
                text: text.into(),
                tags,
                ..Default::default()
            }),
            src(),
        )
    }

    fn understanding_of(source_id: crate::event_id::EventId) -> Event {
        Event::new(
            EventKind::Understanding(UnderstandingPayload {
                source_id,
                summary: "s".into(),
                intent: String::new(),
                entities: vec![],
                references: vec![],
                salience: String::new(),
                model: "m".into(),
                valid_from: Utc::now(),
                tags: BTreeMap::new(),
                extra: Map::new(),
            }),
            src(),
        )
    }

    #[test]
    fn coverage_counts_only_signal_and_understood() {
        let c1 = capture("a real decision about the architecture", false);
        let c2 = capture("another signal capture worth understanding", false);
        let trace = capture("[Bash] ls", true); // ephemeral: not in denominator
        let u1 = understanding_of(c1.id);
        // c2 has no understanding; trace is ephemeral.
        let events = vec![c1, c2, trace, u1];
        let cov = compute_coverage(events.into_iter());
        assert_eq!(cov.signal_captures, 2, "ephemeral trace excluded");
        assert_eq!(cov.decomposed, 1, "only c1 understood");
        assert_eq!(cov.percent(), 50);
        assert_eq!(cov.undecomposed(), 1);
    }

    #[test]
    fn empty_store_is_full_coverage() {
        let cov = compute_coverage(std::iter::empty());
        assert_eq!(cov.percent(), 100);
    }
}
