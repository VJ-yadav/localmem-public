//! Write policy: COMMIT / UPDATE / DEDUP / SKIP / FORGET.
//!
//! Default rules ship in `core/policies/default.yaml` (embedded into the
//! binary via `include_str!`). User overrides go in
//! `<localmem-home>/policies/user.yaml` and replace defaults rule-by-rule by
//! `id`. See ARCHITECTURE.md (Write policy) and TASKS.md task T-15.
//!
//! The engine (T-16) layers `Policy::evaluate` on top of these structs:
//! given an incoming [`Event`] and a recent-context window, it walks rules
//! in order and returns the first match as a [`Decision`]. No rule matched
//! → SKIP under the synthetic `default_skip` rule.

use crate::event::{Event, EventKind, PolicyAction};
use anyhow::{Context, Result};
use chrono::Duration;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// On-disk location of the policy directory relative to the localmem home.
pub const POLICIES_DIR: &str = "policies";
pub const DEFAULT_POLICY_FILE: &str = "default.yaml";
pub const USER_POLICY_FILE: &str = "user.yaml";

/// Shipped defaults, embedded at compile time. `core/policies/default.yaml`
/// is the canonical copy; this constant guarantees the binary always has a
/// working policy even before `localmem init` writes the file to `~/.localmem/`.
pub const DEFAULT_POLICY_YAML: &str = include_str!("../policies/default.yaml");

/// A parsed policy document: an ordered list of rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// One named rule. The first rule whose [`Condition`] matches a capture
/// decides the [`PolicyAction`]; the engine returns its `id` and `reasoning`
/// in the journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub id: String,
    #[serde(default)]
    pub when: Condition,
    pub action: PolicyAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

/// Predicates a rule can assert over the incoming capture and recent context.
///
/// All set fields must hold for the rule to fire (AND semantics). A
/// [`Condition`] with no fields set is treated as "always" by the engine and
/// is useful as a catch-all when placed at the end of the rule list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Condition {
    /// Capture text must be at least this many Unicode scalars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_content_length: Option<usize>,

    /// Capture text must be at most this many Unicode scalars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_length: Option<usize>,

    /// True iff another recent capture (within `N` seconds of the current
    /// event's timestamp) has byte-identical text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_match_within_seconds: Option<u64>,

    /// At least one of these patterns matches the capture text. Patterns are
    /// regular expressions in the `regex` crate's syntax.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_matches_any: Vec<String>,
}

impl Policy {
    /// Parse the bundled defaults. Infallible in normal builds; returns Err
    /// only if `core/policies/default.yaml` has been corrupted between
    /// compile and run (it is checked into the repo).
    pub fn defaults() -> Result<Self> {
        Self::from_yaml_str(DEFAULT_POLICY_YAML).context("parse bundled default policy")
    }

    /// Parse a [`Policy`] from a YAML document string.
    pub fn from_yaml_str(s: &str) -> Result<Self> {
        serde_yaml::from_str(s).context("parse policy yaml")
    }

    /// Load `<home>/policies/default.yaml` if it exists, else fall back to
    /// the bundled defaults. Then merge `<home>/policies/user.yaml` if it
    /// exists. Either file missing is not an error.
    pub fn load(home: impl AsRef<Path>) -> Result<Self> {
        let home = home.as_ref();
        let default_path = home.join(POLICIES_DIR).join(DEFAULT_POLICY_FILE);
        let mut policy = if default_path.exists() {
            let text = std::fs::read_to_string(&default_path)
                .with_context(|| format!("read default policy at {}", default_path.display()))?;
            Self::from_yaml_str(&text)?
        } else {
            Self::defaults()?
        };
        let user_path = home.join(POLICIES_DIR).join(USER_POLICY_FILE);
        if user_path.exists() {
            let text = std::fs::read_to_string(&user_path)
                .with_context(|| format!("read user policy at {}", user_path.display()))?;
            let overrides = Self::from_yaml_str(&text)?;
            policy.merge(overrides);
        }
        Ok(policy)
    }

    /// Apply `other` over `self`. Rules in `other` whose `id` matches an
    /// existing rule REPLACE it in place (preserving order); new ids are
    /// appended at the end.
    ///
    /// We deliberately do not provide a "remove rule" operation: removing a
    /// shipped rule by id would silently change behavior of any future
    /// localmem release that adds rules. Users who want to disable a rule
    /// override it with a rule that has the same id and a no-op action.
    pub fn merge(&mut self, other: Policy) {
        for new_rule in other.rules {
            match self.rules.iter_mut().find(|r| r.id == new_rule.id) {
                Some(existing) => *existing = new_rule,
                None => self.rules.push(new_rule),
            }
        }
    }

    /// Look up a rule by id.
    pub fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }

    /// Decide what to do with `event` given the surrounding recent context.
    ///
    /// Walks `self.rules` in order, returning the first match. If no rule
    /// matches (including the case where `event` is not a capture), the
    /// decision is SKIP under the synthetic `default_skip` rule id.
    ///
    /// Returns `Err` only if a rule's regex pattern fails to compile. The
    /// caller (typically the write pipeline) can surface this as a 500-class
    /// error; bundled defaults are regex-validated by `defaults_parse_*`
    /// tests, so this is effectively a user-config error.
    pub fn evaluate(&self, event: &Event, ctx: &EvalContext) -> Result<Decision> {
        for rule in &self.rules {
            if rule.matches(event, ctx)? {
                return Ok(Decision {
                    action: rule.action,
                    rule_id: rule.id.clone(),
                    reasoning: rule.reasoning.clone().unwrap_or_default(),
                });
            }
        }
        Ok(Decision {
            action: PolicyAction::Skip,
            rule_id: DEFAULT_SKIP_RULE_ID.to_string(),
            reasoning: "no rule matched".to_string(),
        })
    }
}

/// Synthetic rule id returned by [`Policy::evaluate`] when no rule matches.
pub const DEFAULT_SKIP_RULE_ID: &str = "default_skip";

/// What the policy decided for a single event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub action: PolicyAction,
    pub rule_id: String,
    pub reasoning: String,
}

/// Read-only context available to a rule during evaluation.
///
/// `recent` holds the events the engine should treat as "the recent past"
/// for time-window predicates (e.g., `exact_match_within_seconds`). The
/// caller is responsible for capping this slice (typically the last 100
/// events per TASKS.md T-16). Order does not matter for correctness.
#[derive(Debug, Default, Clone, Copy)]
pub struct EvalContext<'a> {
    pub recent: &'a [Event],
}

impl Rule {
    fn matches(&self, event: &Event, ctx: &EvalContext) -> Result<bool> {
        // Policy applies to captures only in v0.1. Other kinds bypass rules
        // and fall through to the default-skip case.
        let Some(text) = capture_text(event) else {
            return Ok(false);
        };
        let when = &self.when;

        if let Some(n) = when.min_content_length {
            if text.chars().count() < n {
                return Ok(false);
            }
        }
        if let Some(n) = when.max_content_length {
            if text.chars().count() > n {
                return Ok(false);
            }
        }
        if let Some(secs) = when.exact_match_within_seconds {
            if !has_recent_exact_match(text, event, ctx, secs) {
                return Ok(false);
            }
        }
        if !when.content_matches_any.is_empty() {
            let mut matched = false;
            for pat in &when.content_matches_any {
                let re = regex::Regex::new(pat)
                    .with_context(|| format!("compile regex `{pat}` for rule `{}`", self.id))?;
                if re.is_match(text) {
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Ok(false);
            }
        }
        // Empty Condition is intentionally a catch-all: a rule like
        // `{ id: fallback, when: {}, action: SKIP }` placed last makes the
        // engine's behavior obvious from the YAML alone rather than relying
        // on the synthetic default_skip.
        Ok(true)
    }
}

fn capture_text(event: &Event) -> Option<&str> {
    match &event.kind {
        EventKind::Capture(p) => Some(p.text.as_str()),
        _ => None,
    }
}

fn has_recent_exact_match(text: &str, event: &Event, ctx: &EvalContext, window_secs: u64) -> bool {
    // Anchor on event.ts (the candidate's wall time), not "now". This makes
    // the rule deterministic during replay: re-evaluating a historical event
    // never sees a different context than the day it was first written.
    let window = Duration::seconds(window_secs as i64);
    let cutoff = event.ts - window;
    for prior in ctx.recent {
        if prior.id == event.id {
            continue;
        }
        if prior.ts < cutoff || prior.ts > event.ts {
            continue;
        }
        if let Some(pt) = capture_text(prior) {
            if pt == text {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CapturePayload, ForgetPayload, Source};
    use crate::event_id::EventId;
    use chrono::{TimeZone, Utc};
    use serde_json::Map;
    use tempfile::tempdir;

    fn capture(text: &str) -> Event {
        Event::new(
            EventKind::Capture(CapturePayload {
                text: text.into(),
                rewritten_text: None,
                kind: Default::default(),
                mime: None,
                attachments: vec![],
                tags: Default::default(),
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        )
    }

    #[test]
    fn defaults_parse_and_contain_expected_rules() {
        let p = Policy::defaults().expect("bundled defaults must parse");
        // The three rules called out by SPEC.md / TASKS.md T-15 must all be
        // present. Order matters: forget_pii before high_signal so PII never
        // gets committed before the PII rule runs.
        let ids: Vec<&str> = p.rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["forget_pii", "dedup", "high_signal"]);

        let pii = p.rule("forget_pii").unwrap();
        assert_eq!(pii.action, PolicyAction::Forget);
        assert!(!pii.when.content_matches_any.is_empty());

        let dedup = p.rule("dedup").unwrap();
        assert_eq!(dedup.action, PolicyAction::Dedup);
        assert_eq!(dedup.when.exact_match_within_seconds, Some(60));

        let hi = p.rule("high_signal").unwrap();
        assert_eq!(hi.action, PolicyAction::Commit);
        assert_eq!(hi.when.min_content_length, Some(20));
    }

    #[test]
    fn default_pii_patterns_compile_as_regex() {
        // The default.yaml ships regex strings; if any are malformed we want
        // to fail at test time, not at first capture in production.
        let p = Policy::defaults().unwrap();
        let pii = p.rule("forget_pii").unwrap();
        for pat in &pii.when.content_matches_any {
            regex::Regex::new(pat).unwrap_or_else(|e| panic!("bad PII regex {pat:?}: {e}"));
        }
    }

    #[test]
    fn from_yaml_str_parses_minimal_rule() {
        let yaml = r#"
rules:
  - id: catch_all
    action: SKIP
"#;
        let p = Policy::from_yaml_str(yaml).unwrap();
        assert_eq!(p.rules.len(), 1);
        let r = &p.rules[0];
        assert_eq!(r.id, "catch_all");
        assert_eq!(r.action, PolicyAction::Skip);
        // Missing `when` yields the all-None default condition.
        assert_eq!(r.when, Condition::default());
    }

    #[test]
    fn from_yaml_str_rejects_unknown_action() {
        let yaml = r#"
rules:
  - id: oops
    action: PROBABLY_COMMIT
"#;
        assert!(Policy::from_yaml_str(yaml).is_err());
    }

    #[test]
    fn merge_replaces_existing_rule_by_id_in_place() {
        let mut base = Policy::defaults().unwrap();
        let original_len = base.rules.len();
        let user = Policy::from_yaml_str(
            r#"
rules:
  - id: high_signal
    when:
      min_content_length: 5
    action: COMMIT
    reasoning: looser threshold
"#,
        )
        .unwrap();
        base.merge(user);
        assert_eq!(base.rules.len(), original_len, "replace, not append");
        let hi = base.rule("high_signal").unwrap();
        assert_eq!(hi.when.min_content_length, Some(5));
        assert_eq!(hi.reasoning.as_deref(), Some("looser threshold"));
        // Replacement preserves original position.
        let ids: Vec<&str> = base.rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["forget_pii", "dedup", "high_signal"]);
    }

    #[test]
    fn merge_appends_new_rule_ids() {
        let mut base = Policy::defaults().unwrap();
        let original_len = base.rules.len();
        let user = Policy::from_yaml_str(
            r#"
rules:
  - id: skip_short
    when:
      max_content_length: 5
    action: SKIP
"#,
        )
        .unwrap();
        base.merge(user);
        assert_eq!(base.rules.len(), original_len + 1);
        let last = base.rules.last().unwrap();
        assert_eq!(last.id, "skip_short");
        assert_eq!(last.when.max_content_length, Some(5));
    }

    #[test]
    fn load_uses_bundled_defaults_when_home_is_empty() {
        let tmp = tempdir().unwrap();
        // No policies/ dir exists under tmp; load() must still succeed by
        // falling back to the embedded defaults.
        let p = Policy::load(tmp.path()).unwrap();
        assert_eq!(p, Policy::defaults().unwrap());
    }

    #[test]
    fn load_reads_disk_default_when_present() {
        let tmp = tempdir().unwrap();
        let pol_dir = tmp.path().join(POLICIES_DIR);
        std::fs::create_dir_all(&pol_dir).unwrap();
        std::fs::write(
            pol_dir.join(DEFAULT_POLICY_FILE),
            r#"
rules:
  - id: only_one
    action: COMMIT
"#,
        )
        .unwrap();
        let p = Policy::load(tmp.path()).unwrap();
        assert_eq!(p.rules.len(), 1);
        assert_eq!(p.rules[0].id, "only_one");
    }

    #[test]
    fn load_merges_user_overrides_on_top_of_defaults() {
        let tmp = tempdir().unwrap();
        let pol_dir = tmp.path().join(POLICIES_DIR);
        std::fs::create_dir_all(&pol_dir).unwrap();
        // Disk default is the bundled one.
        std::fs::write(pol_dir.join(DEFAULT_POLICY_FILE), DEFAULT_POLICY_YAML).unwrap();
        std::fs::write(
            pol_dir.join(USER_POLICY_FILE),
            r#"
rules:
  - id: high_signal
    when:
      min_content_length: 1
    action: COMMIT
    reasoning: user override
  - id: my_new_rule
    when:
      max_content_length: 3
    action: SKIP
"#,
        )
        .unwrap();
        let p = Policy::load(tmp.path()).unwrap();
        let hi = p.rule("high_signal").unwrap();
        assert_eq!(hi.when.min_content_length, Some(1));
        assert_eq!(hi.reasoning.as_deref(), Some("user override"));
        // forget_pii and dedup survive unchanged.
        assert!(p.rule("forget_pii").is_some());
        assert!(p.rule("dedup").is_some());
        // New rule appended.
        assert!(p.rule("my_new_rule").is_some());
    }

    // ---- T-16: engine ---------------------------------------------------

    #[test]
    fn evaluate_commits_long_capture_under_default_policy() {
        let p = Policy::defaults().unwrap();
        let ev = capture("I prefer functional Rust over OO ceremony.");
        let d = p.evaluate(&ev, &EvalContext::default()).unwrap();
        assert_eq!(d.action, PolicyAction::Commit);
        assert_eq!(d.rule_id, "high_signal");
        assert!(!d.reasoning.is_empty(), "reasoning should be populated");
    }

    #[test]
    fn evaluate_skips_short_capture() {
        let p = Policy::defaults().unwrap();
        let ev = capture("ok");
        let d = p.evaluate(&ev, &EvalContext::default()).unwrap();
        assert_eq!(d.action, PolicyAction::Skip);
        // No rule matched: synthetic default_skip kicks in.
        assert_eq!(d.rule_id, DEFAULT_SKIP_RULE_ID);
    }

    #[test]
    fn evaluate_dedups_exact_match_within_window() {
        let p = Policy::defaults().unwrap();
        let mut prior = capture("Hello world, this is long enough to commit.");
        prior.ts = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        let mut now = capture("Hello world, this is long enough to commit.");
        now.ts = prior.ts + Duration::seconds(30);

        let ctx = EvalContext {
            recent: std::slice::from_ref(&prior),
        };
        let d = p.evaluate(&now, &ctx).unwrap();
        assert_eq!(d.action, PolicyAction::Dedup);
        assert_eq!(d.rule_id, "dedup");
    }

    #[test]
    fn evaluate_does_not_dedup_outside_window() {
        let p = Policy::defaults().unwrap();
        let mut prior = capture("Hello world, this is long enough to commit.");
        prior.ts = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        let mut now = capture("Hello world, this is long enough to commit.");
        // 61 seconds is just past the 60s dedup window.
        now.ts = prior.ts + Duration::seconds(61);

        let ctx = EvalContext {
            recent: std::slice::from_ref(&prior),
        };
        let d = p.evaluate(&now, &ctx).unwrap();
        // Outside window → dedup doesn't fire; falls through to high_signal.
        assert_eq!(d.action, PolicyAction::Commit);
        assert_eq!(d.rule_id, "high_signal");
    }

    #[test]
    fn evaluate_forgets_when_pii_detected() {
        let p = Policy::defaults().unwrap();
        // SSN-shaped digits embedded in a longer string.
        let ev = capture("My SSN is 123-45-6789, please don't store this.");
        let d = p.evaluate(&ev, &EvalContext::default()).unwrap();
        assert_eq!(d.action, PolicyAction::Forget);
        assert_eq!(d.rule_id, "forget_pii");
    }

    #[test]
    fn evaluate_forgets_when_credit_card_detected() {
        let p = Policy::defaults().unwrap();
        let ev = capture("card number 4111 1111 1111 1111 for testing");
        let d = p.evaluate(&ev, &EvalContext::default()).unwrap();
        assert_eq!(d.action, PolicyAction::Forget);
        assert_eq!(d.rule_id, "forget_pii");
    }

    #[test]
    fn evaluate_pii_beats_dedup_and_high_signal() {
        // forget_pii is the first rule; even if a capture would also dedup
        // or commit, PII must win because order is significance.
        let p = Policy::defaults().unwrap();
        let mut prior = capture("My SSN is 123-45-6789, please forget.");
        prior.ts = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        let mut now = capture("My SSN is 123-45-6789, please forget.");
        now.ts = prior.ts + Duration::seconds(5);

        let d = p
            .evaluate(
                &now,
                &EvalContext {
                    recent: std::slice::from_ref(&prior),
                },
            )
            .unwrap();
        assert_eq!(d.action, PolicyAction::Forget);
        assert_eq!(d.rule_id, "forget_pii");
    }

    #[test]
    fn evaluate_non_capture_event_skips() {
        let p = Policy::defaults().unwrap();
        let ev = Event::new(
            EventKind::Forget(ForgetPayload {
                target_id: EventId::new(),
                reason: "unit test".into(),
                scope: None,
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "h".into(),
                user: None,
            },
        );
        let d = p.evaluate(&ev, &EvalContext::default()).unwrap();
        assert_eq!(d.action, PolicyAction::Skip);
        assert_eq!(d.rule_id, DEFAULT_SKIP_RULE_ID);
    }

    #[test]
    fn evaluate_empty_condition_acts_as_catchall() {
        // A rule with an empty `when` matches any capture. Useful as a
        // user-defined fallback that gives a meaningful rule_id in the
        // journal instead of the synthetic default_skip.
        let p = Policy::from_yaml_str(
            r#"
rules:
  - id: catchall
    action: SKIP
    reasoning: explicit user catch-all
"#,
        )
        .unwrap();
        let ev = capture("anything goes");
        let d = p.evaluate(&ev, &EvalContext::default()).unwrap();
        assert_eq!(d.action, PolicyAction::Skip);
        assert_eq!(d.rule_id, "catchall");
        assert_eq!(d.reasoning, "explicit user catch-all");
    }

    #[test]
    fn evaluate_propagates_bad_regex_error() {
        // A user with a malformed regex must see a compile error, not silent
        // rule-skipping. Use a rule that runs BEFORE high_signal so we hit
        // the regex path on a capture that would otherwise commit.
        let p = Policy::from_yaml_str(
            r#"
rules:
  - id: broken
    when:
      content_matches_any:
        - "["
    action: SKIP
"#,
        )
        .unwrap();
        let err = p
            .evaluate(&capture("hello world"), &EvalContext::default())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("regex"), "expected regex error, got: {msg}");
    }

    #[test]
    fn evaluate_ignores_recent_event_with_same_id_as_candidate() {
        // Defensive: if the caller accidentally includes the candidate
        // itself in `recent`, we must not fire dedup against ourselves.
        let p = Policy::defaults().unwrap();
        let ev = capture("a duplicate of itself, but long enough to commit");
        let d = p
            .evaluate(
                &ev,
                &EvalContext {
                    recent: std::slice::from_ref(&ev),
                },
            )
            .unwrap();
        assert_eq!(d.action, PolicyAction::Commit);
        assert_eq!(d.rule_id, "high_signal");
    }

    #[test]
    fn evaluate_dedup_ignores_future_events_in_context() {
        // Events with ts > candidate.ts should not count as "prior".
        // Otherwise replay order could yield non-deterministic dedup.
        let p = Policy::defaults().unwrap();
        let mut candidate = capture("Hello world, this is long enough to commit.");
        candidate.ts = Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap();
        let mut future_twin = capture("Hello world, this is long enough to commit.");
        future_twin.ts = candidate.ts + Duration::seconds(5);

        let d = p
            .evaluate(
                &candidate,
                &EvalContext {
                    recent: std::slice::from_ref(&future_twin),
                },
            )
            .unwrap();
        assert_eq!(d.action, PolicyAction::Commit);
        assert_eq!(d.rule_id, "high_signal");
    }
}
