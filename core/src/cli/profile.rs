//! `localmem profile` handler.
//!
//! Synthesizes a markdown profile from the facts table. See SPEC.md
//! "localmem profile" and TASKS.md T-39.
//!
//! SPEC.md is intentionally vague on "synthesize"; v0.1's contract is a
//! deterministic template that groups by subject -> predicate. No LLM
//! call. This keeps `localmem profile` local-only and reproducible across
//! runs given the same facts.

use crate::facts::{Fact, FactsStore};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Serialize)]
pub struct ProfileOutput {
    pub profile_md: String,
    pub generated_at: String,
    pub fact_count: usize,
}

/// Entry point for the `profile` subcommand.
///
/// `tags` is the T-51b container-tag filter applied to the rendered
/// facts (subset match against `Fact.tags` inherited from the source
/// capture). An empty map disables filtering. Tags scope and subject
/// scope (`--scope`) compose with AND semantics.
pub fn run(
    home: Option<&str>,
    scope: Option<&str>,
    tags: BTreeMap<String, String>,
    as_json: bool,
) -> Result<()> {
    run_with_kind(home, scope, tags, None, as_json)
}

/// Entry point used by both `localmem profile` and `localmem
/// summarize` (T-53). Adds a kind filter on top of the tag + subject
/// filters; the rendering pipeline is shared so a future tweak to
/// the grouping/order lands in one place. `Some(kind)` keeps only
/// facts whose closed-core kind equals it (`Other(_)` matches when
/// the inner string matches). `None` returns the full multi-kind
/// brief.
pub fn run_with_kind(
    home: Option<&str>,
    scope: Option<&str>,
    tags: BTreeMap<String, String>,
    kind: Option<crate::kind::Kind>,
    as_json: bool,
) -> Result<()> {
    let home = resolve_home(home)?;
    let store = FactsStore::open(&home).context("open facts store")?;
    let tag_filter = if tags.is_empty() { None } else { Some(&tags) };
    // Default visibility per SPEC_V0_2: profile is a synthesis path,
    // not an audit pull, so `visibility=private` captures stay hidden
    // (T-51c).
    let now = Utc::now();
    let mut facts = store
        .all_live_facts_filtered(
            now,
            scope,
            tag_filter,
            crate::reserved_tags::Visibility::Default,
            now,
        )
        .context("read live facts for profile")?;
    if let Some(k) = &kind {
        facts.retain(|f| &f.kind == k);
    }
    // T-52b: load capture-level todos so the profile can render a
    // checkbox list with the latest done state. Skipped when the
    // caller filtered to a non-Todo kind (the section would be
    // empty by construction).
    let todos = if matches!(kind, Some(crate::kind::Kind::Todo) | None) {
        load_todos(&home, scope, &tags).context("load todo captures for profile")?
    } else {
        Vec::new()
    };
    let out = synthesize_profile_with_todos(&facts, scope, &todos);
    emit(&out, as_json)
}

/// T-52b: surface a capture-level todo in the profile.
#[derive(Debug, Clone, Serialize)]
struct TodoRow {
    event_id: String,
    text: String,
    done: bool,
}

/// Walk the event log for `Kind::Todo` captures that haven't been
/// forgotten, look up each one's latest `done` state in the lex
/// index, and return rows in chronological (oldest-first) order.
///
/// Scope + tags filter inline so the resulting list matches the
/// fact section's scoping discipline. Capture-level todos that
/// don't pass the tag filter never reach the rendered profile, the
/// same as their fact-level counterparts.
fn load_todos(
    home: &std::path::Path,
    scope: Option<&str>,
    tags: &BTreeMap<String, String>,
) -> Result<Vec<TodoRow>> {
    use crate::event::{Event, EventKind, ForgetPayload};
    use std::collections::HashSet;

    let log = crate::event_log::EventLog::open(home).context("open event log")?;
    // Two passes: first collect forgotten capture ids so a todo that
    // was retired stays off the list; second build the rows. The
    // log is append-only so two passes over the same iterator
    // produce identical results.
    let mut forgotten: HashSet<String> = HashSet::new();
    for ev in log.iter().context("scan event log for forgets")? {
        let ev: Event = ev.context("read event")?;
        if let EventKind::Forget(ForgetPayload { target_id, .. }) = ev.kind {
            forgotten.insert(target_id.to_string());
        }
    }
    // Look up done state via the lex index (reader-only so we don't
    // fight the server for the writer lock).
    let lex = crate::lexical::LexicalIndex::open_reader_only(home)
        .context("open lex index for profile todos")?;

    let mut rows: Vec<TodoRow> = Vec::new();
    for ev in log.iter().context("scan event log for todos")? {
        let ev: Event = ev.context("read event")?;
        let EventKind::Capture(payload) = &ev.kind else {
            continue;
        };
        if !matches!(payload.kind, crate::kind::Kind::Todo) {
            continue;
        }
        let id_str = ev.id.to_string();
        if forgotten.contains(&id_str) {
            continue;
        }
        if let Some(s) = scope {
            // Capture-level scope filter: SPEC_V0_2 doesn't define a
            // subject for raw captures (subjects live on extracted
            // facts), so we approximate by matching against the
            // source app. v0.2.1 can revisit if users complain.
            if ev.source.app != s {
                continue;
            }
        }
        if !tags.is_empty()
            && !crate::tag_match::matches(&payload.tags, tags)
        {
            continue;
        }
        let meta = lex
            .meta_for(&id_str)
            .context("meta_for todo capture")?;
        // Fall back to the raw text when the rewriter produced
        // nothing — same display rule the lex layer follows.
        let text = payload.indexable_text().to_string();
        rows.push(TodoRow {
            event_id: id_str,
            text,
            done: meta.done,
        });
    }
    Ok(rows)
}

/// Display order for kind sections in the rendered profile. Fixed
/// so a re-run on the same DB produces byte-identical output. The
/// order matters for UX: preferences and decisions are the most
/// useful surfaces, so they come first. `Other` (extension kinds)
/// is the catch-all that absorbs `Note` and any unrecognised kind.
const KIND_DISPLAY_ORDER: &[KindGroup] = &[
    KindGroup::Preference,
    KindGroup::Decision,
    KindGroup::Constraint,
    KindGroup::Fact,
    KindGroup::Todo,
    KindGroup::Other,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum KindGroup {
    Preference,
    Decision,
    Constraint,
    Fact,
    Todo,
    /// Catch-all for `Note` and any non-canonical kind. Profile
    /// surfaces notes here rather than promoting them to their own
    /// section: they're the freeform default and would otherwise
    /// dominate the output for users who don't tag.
    Other,
}

impl KindGroup {
    fn from_kind(k: &crate::kind::Kind) -> Self {
        match k {
            crate::kind::Kind::Preference => KindGroup::Preference,
            crate::kind::Kind::Decision => KindGroup::Decision,
            crate::kind::Kind::Constraint => KindGroup::Constraint,
            crate::kind::Kind::Fact => KindGroup::Fact,
            crate::kind::Kind::Todo => KindGroup::Todo,
            crate::kind::Kind::Note | crate::kind::Kind::Other(_) => KindGroup::Other,
        }
    }
    fn heading(self) -> &'static str {
        match self {
            KindGroup::Preference => "Preferences",
            KindGroup::Decision => "Decisions",
            KindGroup::Constraint => "Constraints",
            KindGroup::Fact => "Facts",
            KindGroup::Todo => "Todos",
            KindGroup::Other => "Other",
        }
    }
}

/// Group facts first by kind, then subject, then predicate, and
/// render markdown. Deterministic: BTreeMap keeps lexicographic
/// order on subjects + predicates so a re-run on the same DB
/// produces byte-identical output, and the kind sections render in
/// the fixed [`KIND_DISPLAY_ORDER`].
fn synthesize_profile(facts: &[Fact], scope: Option<&str>) -> ProfileOutput {
    // KindGroup -> subject -> predicate -> facts. The outer key is
    // not BTreeMap-ordered because we render in a fixed sequence;
    // HashMap-via-Vec keeps lookup simple.
    let mut by_kind: std::collections::HashMap<
        KindGroup,
        BTreeMap<String, BTreeMap<String, Vec<&Fact>>>,
    > = std::collections::HashMap::new();
    for f in facts {
        let group = KindGroup::from_kind(&f.kind);
        by_kind
            .entry(group)
            .or_default()
            .entry(f.subject.clone())
            .or_default()
            .entry(f.predicate.clone())
            .or_default()
            .push(f);
    }

    let mut md = String::new();
    md.push_str("# localmem profile\n\n");
    if let Some(s) = scope {
        md.push_str(&format!("**Scope:** `{s}`\n\n"));
    }
    md.push_str(&format!("**Facts:** {}\n\n", facts.len()));
    if by_kind.is_empty() {
        md.push_str("_No facts to display._\n");
    } else {
        for group in KIND_DISPLAY_ORDER {
            let Some(subjects) = by_kind.get(group) else {
                continue;
            };
            md.push_str(&format!("## {}\n\n", group.heading()));
            for (subject, predicates) in subjects {
                md.push_str(&format!("### {subject}\n\n"));
                for (predicate, fs) in predicates {
                    md.push_str(&format!("- **{predicate}**\n"));
                    for f in fs {
                        let valid_from = f.valid_from.to_rfc3339_opts(SecondsFormat::Millis, true);
                        md.push_str(&format!(
                            "  - {} _(conf={:.2}, valid_from={})_\n",
                            f.object, f.confidence, valid_from,
                        ));
                    }
                }
                md.push('\n');
            }
        }
    }

    ProfileOutput {
        profile_md: md,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        fact_count: facts.len(),
    }
}

/// T-52b wrapper: render the fact section via [`synthesize_profile`],
/// then append capture-level todos as a checklist if any exist.
/// Captures aren't facts, but the user-facing surface ("what do you
/// remember about me?") treats them as part of the same brief.
fn synthesize_profile_with_todos(
    facts: &[Fact],
    scope: Option<&str>,
    todos: &[TodoRow],
) -> ProfileOutput {
    let mut out = synthesize_profile(facts, scope);
    if !todos.is_empty() {
        out.profile_md.push_str("## Open + done todos\n\n");
        for t in todos {
            let box_ = if t.done { "[x]" } else { "[ ]" };
            // Truncate long bodies on the profile line; users go to
            // `localmem audit <id>` for the full text.
            let snippet = first_chars(&t.text, 200);
            out.profile_md
                .push_str(&format!("- {box_} {snippet} _({})_\n", t.event_id));
        }
        out.profile_md.push('\n');
    }
    out
}

/// Truncate `s` at `n` characters, appending an ellipsis when we
/// dropped any tail. Counts chars, not bytes, so multi-byte text
/// renders without splitting a codepoint.
fn first_chars(s: &str, n: usize) -> String {
    let mut count = 0;
    let mut end = s.len();
    for (i, _) in s.char_indices() {
        if count == n {
            end = i;
            break;
        }
        count += 1;
    }
    if end < s.len() {
        let mut truncated = String::with_capacity(end + 1);
        truncated.push_str(&s[..end]);
        truncated.push('…');
        truncated
    } else {
        s.to_string()
    }
}

fn emit(out: &ProfileOutput, as_json: bool) -> Result<()> {
    if as_json {
        let json = serde_json::json!({
            "ok": true,
            "profile_md": out.profile_md,
            "generated_at": out.generated_at,
            "fact_count": out.fact_count,
        });
        println!("{json}");
    } else {
        print!("{}", out.profile_md);
    }
    Ok(())
}

fn resolve_home(override_: Option<&str>) -> Result<PathBuf> {
    if let Some(h) = override_.filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var("HOME")
        .context("HOME environment variable is not set; pass --home explicitly")?;
    Ok(PathBuf::from(home).join(".localmem"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::init::init_home;
    use crate::event_id::EventId;
    use chrono::DateTime;
    use tempfile::tempdir;

    fn ts(epoch: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(epoch, 0).unwrap()
    }

    fn sample_fact(subject: &str, predicate: &str, object: &str) -> Fact {
        Fact {
            id: EventId::new(),
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.into(),
            confidence: 0.8,
            valid_from: ts(1_700_000_000),
            valid_to: None,
            recorded_at: ts(1_700_000_000),
            retired_at: None,
            source_events: vec![],
            tags: Default::default(),
            policy_id: None,
            kind: Default::default(),
        }
    }

    #[test]
    fn empty_store_renders_empty_profile() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let facts = store.all_live_facts(Utc::now(), None).unwrap();
        let out = synthesize_profile(&facts, None);
        assert_eq!(out.fact_count, 0);
        assert!(out.profile_md.contains("No facts to display"));
    }

    #[test]
    fn grouping_orders_subjects_and_predicates_deterministically() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        store
            .insert(&sample_fact("user", "prefers", "rust"))
            .unwrap();
        store
            .insert(&sample_fact("user", "lives_in", "berlin"))
            .unwrap();
        store
            .insert(&sample_fact("alice", "prefers", "haskell"))
            .unwrap();
        let facts = store.all_live_facts(Utc::now(), None).unwrap();
        let out = synthesize_profile(&facts, None);
        assert_eq!(out.fact_count, 3);
        // alice comes before user (lexicographic).
        let alice_pos = out.profile_md.find("## alice").unwrap();
        let user_pos = out.profile_md.find("## user").unwrap();
        assert!(alice_pos < user_pos);
        // lives_in before prefers under user.
        let lives_pos = out.profile_md[user_pos..].find("lives_in").unwrap();
        let prefers_pos = out.profile_md[user_pos..].find("prefers").unwrap();
        assert!(lives_pos < prefers_pos);
    }

    #[test]
    fn scope_filter_limits_subjects() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        store
            .insert(&sample_fact("user", "prefers", "rust"))
            .unwrap();
        store
            .insert(&sample_fact("alice", "prefers", "haskell"))
            .unwrap();
        let facts = store.all_live_facts(Utc::now(), Some("alice")).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].subject, "alice");
    }

    #[test]
    fn retired_facts_excluded_from_profile() {
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let mut f = sample_fact("user", "lives_in", "Tokyo");
        f.retired_at = Some(ts(1_600_000_000)); // already retired
        store.insert(&f).unwrap();
        let facts = store.all_live_facts(Utc::now(), None).unwrap();
        assert!(
            facts.is_empty(),
            "retired fact should not appear in profile"
        );
    }

    #[test]
    fn tags_filter_restricts_profile_to_matching_facts() {
        // T-51b: profile honours --tags by routing through
        // FactsStore::all_live_facts_filtered. We exercise the store
        // call here directly so the test stays decoupled from the
        // synthesize_profile rendering details.
        let tmp = tempdir().unwrap();
        init_home(tmp.path()).unwrap();
        let store = FactsStore::open(tmp.path()).unwrap();
        let mut lm = sample_fact("user", "prefers", "rust");
        lm.tags.insert("project".into(), "localmem".into());
        let mut other = sample_fact("user", "prefers", "go");
        other.tags.insert("project".into(), "other".into());
        store.insert(&lm).unwrap();
        store.insert(&other).unwrap();

        let mut filter = BTreeMap::new();
        filter.insert("project".into(), "localmem".into());
        let now = Utc::now();
        let facts = store
            .all_live_facts_filtered(
                now,
                None,
                Some(&filter),
                crate::reserved_tags::Visibility::Default,
                now,
            )
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].object, "rust");
    }

    // ---- T-52: kind grouping in the rendered profile ----

    fn fact_with_kind(
        subject: &str,
        predicate: &str,
        object: &str,
        kind: crate::kind::Kind,
    ) -> Fact {
        let mut f = sample_fact(subject, predicate, object);
        f.kind = kind;
        f
    }

    #[test]
    fn profile_groups_facts_by_kind_in_canonical_section_order() {
        // Two facts with different kinds must surface under their
        // own headings, and the heading order must follow
        // KIND_DISPLAY_ORDER (preferences before decisions before
        // constraints before facts before todos before other).
        let pref = fact_with_kind("user", "prefers", "rust", crate::kind::Kind::Preference);
        let dec = fact_with_kind("team", "chose", "duckdb", crate::kind::Kind::Decision);
        let cstr = fact_with_kind(
            "policy",
            "requires",
            "no-mock",
            crate::kind::Kind::Constraint,
        );
        let fct = fact_with_kind("user", "lives_in", "berlin", crate::kind::Kind::Fact);
        let nt = fact_with_kind("user", "noted", "rainy", crate::kind::Kind::Note);
        let out = synthesize_profile(&[pref, dec, cstr, fct, nt], None);

        // All five sections render.
        for section in [
            "## Preferences",
            "## Decisions",
            "## Constraints",
            "## Facts",
            "## Other",
        ] {
            assert!(
                out.profile_md.contains(section),
                "missing section {section} in:\n{}",
                out.profile_md,
            );
        }

        // Order: Preferences < Decisions < Constraints < Facts < Other.
        let p = out.profile_md.find("## Preferences").unwrap();
        let d = out.profile_md.find("## Decisions").unwrap();
        let c = out.profile_md.find("## Constraints").unwrap();
        let f = out.profile_md.find("## Facts").unwrap();
        let o = out.profile_md.find("## Other").unwrap();
        assert!(p < d, "Preferences must come before Decisions");
        assert!(d < c, "Decisions must come before Constraints");
        assert!(c < f, "Constraints must come before Facts");
        assert!(f < o, "Facts must come before Other");
    }

    #[test]
    fn profile_treats_note_and_extension_kinds_under_other() {
        // Spec: "Extension kinds round-trip as note-equivalent."
        // Both Note and an unrecognised kind should land in the
        // Other section; neither should get its own heading.
        let note = fact_with_kind("user", "noted", "x", crate::kind::Kind::Note);
        let ext = fact_with_kind(
            "user",
            "tagged",
            "y",
            crate::kind::Kind::Other("recipe".into()),
        );
        let out = synthesize_profile(&[note, ext], None);
        assert!(out.profile_md.contains("## Other"));
        // No section heading for raw "Note" or "recipe".
        assert!(!out.profile_md.contains("## Note"));
        assert!(!out.profile_md.contains("## Recipe"));
        assert!(!out.profile_md.contains("## recipe"));
        // Both facts surface under Other (find their objects).
        assert!(out.profile_md.contains(" x "));
        assert!(out.profile_md.contains(" y "));
    }

    #[test]
    fn profile_does_not_render_empty_kind_sections() {
        // Only preferences exist: only that section must render.
        let pref = fact_with_kind("user", "prefers", "rust", crate::kind::Kind::Preference);
        let out = synthesize_profile(&[pref], None);
        assert!(out.profile_md.contains("## Preferences"));
        assert!(!out.profile_md.contains("## Decisions"));
        assert!(!out.profile_md.contains("## Constraints"));
        assert!(!out.profile_md.contains("## Facts"));
        assert!(!out.profile_md.contains("## Todos"));
        assert!(!out.profile_md.contains("## Other"));
    }
}
