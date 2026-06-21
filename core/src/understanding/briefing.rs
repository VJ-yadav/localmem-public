//! The Session Boot Briefing (the unified memory-layer design, Output B).
//!
//! A synthesized, ranked, current-state-first digest of a project's memory,
//! built to REPLACE the raw top-k search dump that SessionStart used to inject
//! (the "five useless rows" failure). Six blocks, ordered by volatility x
//! influence-on-next-action:
//!
//!   1. NOW              - active state + the immediate next action
//!   2. OPEN LOOPS       - unresolved items, each with what it's blocked on
//!   3. WATCH-OUTS       - conflicts: the winner, with the loser marked SUPERSEDED
//!   4. DURABLE RULES    - hard constraints, one line each
//!   5. PREFERENCES      - how the user likes to work
//!   6. POINTERS         - topics to query in depth, not inlined
//!
//! This module owns the pure, testable halves (the schema, the synthesis
//! prompt, the tolerant parse, and the markdown render). The corpus gather and
//! the LLM round-trip live at the call site (the server `/brief` handler), and
//! the synthesizer is abstracted so tests run without a model.
//!
//! Grounding: the briefing is synthesized ONLY from memories the caller gathers
//! and passes in (each carrying its source id + date), never from the model's
//! own knowledge. The caller returns those source ids alongside the briefing so
//! the viewer can drill down to the raw memories.

use super::ollama::chat_json;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Per-project briefing cache dir. A CACHE (regenerable from the log + facts),
/// NOT a source-of-truth derived store: losing it just means regenerate. The
/// SessionStart hook reads it directly so injection stays fast + LLM-free.
const BRIEFINGS_DIR: &str = "derived/briefings";

/// Cache file path for a project's briefing. Empty project -> `_all`.
pub fn briefing_cache_path(home: &Path, project: &str) -> PathBuf {
    home.join(BRIEFINGS_DIR)
        .join(format!("{}.md", sanitize_project(project)))
}

/// Filesystem-safe filename stem for a project tag. Tags are usually already
/// safe (e.g. `atlas_onboarding`); this guards odd characters and the empty
/// (all-projects) case rather than trusting the input.
fn sanitize_project(project: &str) -> String {
    let trimmed = project.trim();
    if trimmed.is_empty() {
        return "_all".to_string();
    }
    trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Write a project's rendered briefing to the cache (write-through from /brief
/// and the background refresh).
pub fn write_briefing_cache(home: &Path, project: &str, md: &str) -> std::io::Result<()> {
    let path = briefing_cache_path(home, project);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, md)
}

/// Read a project's cached briefing, or `None` when absent/empty (a cold cache
/// the caller should not inject).
pub fn read_briefing_cache(home: &Path, project: &str) -> Option<String> {
    std::fs::read_to_string(briefing_cache_path(home, project))
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// The six blocks of a Session Boot Briefing. Every field defaults to empty so
/// a model that omits a block (or returns a partial object) still parses.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Briefing {
    /// One short paragraph: current state + the immediate next action.
    pub now: String,
    pub open_loops: Vec<String>,
    /// Conflicts: the winning fact, with the older one marked SUPERSEDED + date.
    pub watch_outs: Vec<String>,
    pub durable_rules: Vec<String>,
    pub preferences: Vec<String>,
    /// Topics available to query in more depth (an index, not inlined content).
    pub pointers: Vec<String>,
}

impl Briefing {
    /// True when the synthesis produced nothing usable (so the caller can fall
    /// back rather than inject an empty block).
    pub fn is_empty(&self) -> bool {
        self.now.trim().is_empty()
            && self.open_loops.is_empty()
            && self.watch_outs.is_empty()
            && self.durable_rules.is_empty()
            && self.preferences.is_empty()
            && self.pointers.is_empty()
    }

    /// Render the briefing as a compact markdown block for injection or display.
    /// Empty sections are omitted so the payload stays tight (the size-budget
    /// discipline of SPEC 7c: volatile blocks lead, stable blocks compress).
    pub fn to_markdown(&self, project: &str) -> String {
        let mut out = format!("## Session boot briefing (localmem · project {project})\n");
        if !self.now.trim().is_empty() {
            out.push_str(&format!("\n**Now:** {}\n", self.now.trim()));
        }
        push_list(&mut out, "Open loops", &self.open_loops);
        push_list(&mut out, "Watch-outs", &self.watch_outs);
        push_list(&mut out, "Durable rules", &self.durable_rules);
        push_list(&mut out, "Preferences", &self.preferences);
        push_list(&mut out, "Pointers", &self.pointers);
        out
    }
}

fn push_list(out: &mut String, heading: &str, items: &[String]) {
    let items: Vec<&String> = items.iter().filter(|i| !i.trim().is_empty()).collect();
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("\n**{heading}:**\n"));
    for item in items {
        out.push_str(&format!("- {}\n", item.trim()));
    }
}

/// Build the synthesis instruction. Pure + testable. The user message (passed
/// separately) is the gathered, dated memories; this fixes the JSON shape so
/// [`parse_briefing`] can rely on it.
pub fn briefing_system_prompt(subject: &str, project: &str) -> String {
    format!(
        "Produce a session-boot briefing for {subject} on the project \"{project}\", \
         from the dated memories the user message provides. Respond with ONLY JSON \
         of the exact form {{\"now\":\"...\",\"open_loops\":[\"...\"],\
         \"watch_outs\":[\"...\"],\"durable_rules\":[\"...\"],\"preferences\":[\"...\"],\
         \"pointers\":[\"...\"]}}. \
         `now` is one short paragraph: the current state and the immediate next action. \
         `open_loops` are unresolved items, each noting what it is blocked on. \
         `watch_outs` are conflicts: when two memories disagree, state the winner and \
         mark the older one SUPERSEDED with its date. \
         `durable_rules` are hard constraints, one line each. \
         `preferences` are how the user likes to work. \
         `pointers` are topics available to query in more depth, NOT inlined here. \
         Every array entry is a PLAIN STRING (one line of text), never an object. \
         Rank by how recently things changed and how much they steer the next action; \
         lead with current state and open loops, and keep those volatile blocks to about \
         half the total length. Put a date on each line when the memory has one. \
         Use ONLY the provided memories; never invent facts or sources. \
         GUARDRAILS against hallucination: never invent specifics, no percentages, \
         counts, dates, durations, or status numbers unless they appear VERBATIM in a \
         memory. A memory phrased as a question, a worry, or thinking-out-loud is NOT an \
         established fact: do not promote it to current status. If the memories are mostly \
         chatter and you cannot ground a confident `now`, set `now` to a brief honest note \
         that the current state is unclear from memory, rather than fabricating one. \
         Prefer omitting a line to guessing."
    )
}

/// Parse the synthesizer's JSON content into a [`Briefing`]. Built to survive a
/// small local model's output, which is not reliably schema-conformant. It
/// strips any prose/garbage around the JSON (extracts the first balanced
/// `{...}` object, so trailing tokens don't break parsing), coerces each list
/// entry to a string whether the model returned a string or an object (e.g.
/// `watch_outs: [{date, conflict, ...}]`), and defaults any missing block to
/// empty. Only content with no JSON object at all is a hard error (the caller
/// retries).
pub fn parse_briefing(content: &str) -> Result<Briefing> {
    let json = extract_json_object(content)
        .ok_or_else(|| anyhow::anyhow!("briefing returned no JSON object: {content:?}"))?;
    let v: Value = serde_json::from_str(json)
        .with_context(|| format!("briefing JSON did not parse: {json:?}"))?;
    Ok(Briefing {
        now: coerce_line(v.get("now")),
        open_loops: coerce_list(v.get("open_loops")),
        watch_outs: coerce_list(v.get("watch_outs")),
        durable_rules: coerce_list(v.get("durable_rules")),
        preferences: coerce_list(v.get("preferences")),
        pointers: coerce_list(v.get("pointers")),
    })
}

/// Return the first balanced `{...}` JSON object in `s`, ignoring braces inside
/// strings. Lets us salvage a model response wrapped in prose or followed by
/// trailing tokens. The structural characters are all ASCII, so byte scanning
/// stays on char boundaries.
fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            match c {
                _ if esc => esc = false,
                b'\\' => esc = true,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&s[start..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Coerce a JSON value to a single readable line (for `now` and list entries).
/// Strings pass through; objects are rendered from a primary text field plus an
/// optional date (the shapes a small model tends to emit for `watch_outs`);
/// other shapes fall back to their scalar string values joined.
fn coerce_line(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Object(map)) => {
            let primary = [
                "winner",
                "conflict",
                "text",
                "item",
                "summary",
                "rule",
                "preference",
                "value",
                "description",
                "loop",
                "pointer",
            ]
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_str).map(str::trim))
            .filter(|s| !s.is_empty());
            let date = map.get("date").and_then(Value::as_str).map(str::trim);
            match (primary, date) {
                (Some(p), Some(d)) if !d.is_empty() => format!("{p} ({d})"),
                (Some(p), _) => p.to_string(),
                (None, _) => map
                    .values()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" — "),
            }
        }
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|x| coerce_line(Some(x)))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("; "),
        _ => String::new(),
    }
}

/// Coerce a value into a list of non-empty lines. Accepts an array (each entry
/// coerced) or a lone value (wrapped), so a model that returns a string where an
/// array was asked for still yields one item.
fn coerce_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|x| coerce_line(Some(x)))
            .filter(|s| !s.trim().is_empty())
            .collect(),
        Some(other) => {
            let line = coerce_line(Some(other));
            if line.trim().is_empty() {
                vec![]
            } else {
                vec![line]
            }
        }
        None => vec![],
    }
}

/// Turns gathered memories into a [`Briefing`]. Abstracted so the server handler
/// can be exercised with a stub in tests.
#[async_trait]
pub trait Synthesizer: Send + Sync {
    /// `context` is the caller-gathered, dated memories (the only grounding).
    async fn synthesize(&self, subject: &str, project: &str, context: &str) -> Result<Briefing>;
}

/// Live synthesizer backed by Ollama (shares `chat_json` with the decomposer).
pub struct OllamaSynthesizer {
    model: String,
    endpoint: String,
}

impl OllamaSynthesizer {
    pub fn new(model: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            endpoint: endpoint.into(),
        }
    }
}

#[async_trait]
impl Synthesizer for OllamaSynthesizer {
    async fn synthesize(&self, subject: &str, project: &str, context: &str) -> Result<Briefing> {
        let system = briefing_system_prompt(subject, project);
        // A small local model occasionally emits malformed JSON; the call is
        // non-deterministic, so one retry salvages most transient garbage. A
        // network/transport error short-circuits (no point retrying that here).
        let mut last_err = None;
        for _ in 0..2 {
            let content = chat_json(&self.endpoint, &self.model, &system, context).await?;
            match parse_briefing(&content) {
                Ok(briefing) => return Ok(briefing),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.expect("loop runs at least once"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_briefing() {
        let content = r#"{
            "now": "DS-X-03 is in rubric review; next gate is graded trajectories.",
            "open_loops": ["Grader API-key fix unconfirmed (Harry Li)"],
            "watch_outs": ["Gemini-only policy (2026-05-22) SUPERSEDED by 2026-06-12 authorization"],
            "durable_rules": ["Opus difficulty < 40% short / < 50% detailed"],
            "preferences": ["American spelling; short casual Slack drafts"],
            "pointers": ["rubric weights", "AI-tell audit"]
        }"#;
        let b = parse_briefing(content).unwrap();
        assert!(b.now.starts_with("DS-X-03"));
        assert_eq!(b.open_loops.len(), 1);
        assert_eq!(b.watch_outs.len(), 1);
        assert!(b.watch_outs[0].contains("SUPERSEDED"));
        assert_eq!(b.pointers.len(), 2);
    }

    #[test]
    fn parse_partial_briefing_defaults_missing_blocks() {
        let b = parse_briefing(r#"{"now": "just the gist"}"#).unwrap();
        assert_eq!(b.now, "just the gist");
        assert!(b.open_loops.is_empty());
        assert!(b.pointers.is_empty());
        assert!(!b.is_empty());
    }

    #[test]
    fn empty_object_is_empty_briefing() {
        let b = parse_briefing("{}").unwrap();
        assert!(b.is_empty());
    }

    #[test]
    fn non_json_is_an_error() {
        assert!(parse_briefing("sorry, I can't help").is_err());
    }

    #[test]
    fn coerces_object_list_entries_to_strings() {
        // The exact shape llama3.2 returned that used to fail: watch_outs as an
        // array of objects rather than strings.
        let content = r#"{
            "now": "defining a failure point",
            "watch_outs": [
                {"date": "2026-06-13", "conflict": "task world conflict", "superseeded_by": "2026-06-13"},
                {"winner": "run the python script", "date": "2026-06-13"}
            ],
            "preferences": [{"preference": "verify note content before processing"}]
        }"#;
        let b = parse_briefing(content).unwrap();
        assert_eq!(b.now, "defining a failure point");
        assert_eq!(b.watch_outs.len(), 2);
        assert!(b.watch_outs[0].contains("task world conflict"));
        assert!(b.watch_outs[0].contains("2026-06-13"));
        assert!(b.watch_outs[1].contains("run the python script"));
        assert_eq!(b.preferences, vec!["verify note content before processing"]);
    }

    #[test]
    fn strips_prose_and_trailing_junk_around_the_json() {
        let content =
            "Here is the briefing:\n{\"now\":\"x\",\"pointers\":[\"a\"]}\n\nassistant\n\n\n";
        let b = parse_briefing(content).unwrap();
        assert_eq!(b.now, "x");
        assert_eq!(b.pointers, vec!["a"]);
    }

    #[test]
    fn coerce_list_wraps_a_lone_string() {
        let content = r#"{"open_loops": "just one loop"}"#;
        let b = parse_briefing(content).unwrap();
        assert_eq!(b.open_loops, vec!["just one loop"]);
    }

    #[test]
    fn markdown_leads_with_now_and_omits_empty_sections() {
        let b = Briefing {
            now: "Active: increment 3.".into(),
            open_loops: vec!["wire /brief".into()],
            watch_outs: vec![],
            durable_rules: vec!["no LLM in the hook".into()],
            preferences: vec![],
            pointers: vec![],
        };
        let md = b.to_markdown("localmem");
        assert!(md.contains("project localmem"));
        // Now leads.
        let now_pos = md.find("**Now:**").unwrap();
        let loops_pos = md.find("**Open loops:**").unwrap();
        assert!(now_pos < loops_pos);
        // Empty sections are omitted.
        assert!(!md.contains("Watch-outs"));
        assert!(!md.contains("Preferences"));
        assert!(md.contains("- no LLM in the hook"));
    }

    #[test]
    fn cache_path_sanitizes_and_handles_all_projects() {
        let home = Path::new("/home/.localmem");
        assert!(briefing_cache_path(home, "atlas_onboarding")
            .ends_with("derived/briefings/atlas_onboarding.md"));
        assert!(briefing_cache_path(home, "").ends_with("derived/briefings/_all.md"));
        // Odd characters collapse to underscores; no path traversal escapes.
        let weird = briefing_cache_path(home, "a/b ../c");
        let stem = weird.file_name().unwrap().to_str().unwrap();
        assert_eq!(stem, "a_b____c.md", "every non-[alnum-_] char becomes _");
        assert!(
            !weird.to_string_lossy().contains(".."),
            "no traversal escapes"
        );
    }

    #[test]
    fn system_prompt_names_subject_project_and_the_json_shape() {
        let p = briefing_system_prompt("Vijay", "localmem");
        assert!(p.contains("Vijay"));
        assert!(p.contains("localmem"));
        assert!(p.contains("\"now\""));
        assert!(p.contains("SUPERSEDED"));
        assert!(p.contains("\"pointers\""));
    }
}
