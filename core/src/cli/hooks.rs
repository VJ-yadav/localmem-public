//! `localmem hook <event>` — Claude Code hook handler (auto-capture).
//!
//! These are the auto-capture entry points wired into Claude Code's
//! `settings.json` by `localmem hooks install` (P1b). Each invocation is a
//! short-lived process: Claude Code runs it, pipes the event JSON on stdin, and
//! (for context-injecting events) reads our stdout back as additional context.
//!
//! Design rules (see the design docs):
//! - **Never spawn an LLM.** We only write/read via the fast local core, so a
//!   hook can never re-trigger the agent — no Stop-hook recursion (the bug
//!   the naive version shipped with).
//! - **Smart capture, not everything.** Prompts + decisions are permanent;
//!   tool-use is a compact ephemeral `trace` that auto-expires, so memory stays
//!   signal not noise.
//! - **Never block, never lose.** Capture POSTs to the running core with a short
//!   timeout; if the core is momentarily down it spools to an append-only file
//!   the daemon drains. The hook ALWAYS exits 0 and never emits anything that
//!   could disrupt the agent.
//! - **Valid-time preserved.** Each capture carries the real moment it happened,
//!   so `--at-time` / the Timeline work on live sessions exactly like imports.

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Short per-hook network budget. The core is loopback + async-embeds, so a
/// healthy write returns well under this; a dead core fails fast to the spool.
const HOOK_HTTP_TIMEOUT_MS: u64 = 800;

/// Default TTL for tool-use traces. Long enough to be useful within a work
/// session, short enough that the noise self-evicts.
const TRACE_RETENTION: &str = "ephemeral:7d";

/// Source tag stamped on everything the Claude Code hooks capture.
const HOOK_SOURCE: &str = "claude-code";

/// Entry point for `localmem hook <event>`. Reads the Claude Code event JSON on
/// stdin and acts. ALWAYS returns `Ok(())` to the caller path that maps it to
/// exit 0 — a hook must never fail the agent. Errors are swallowed (capture is
/// best-effort; the event log, not the hook, is the durability guarantee).
pub fn run(home: Option<&str>, event: &str) -> Result<()> {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let home = resolve_home(home);

    // Each arm swallows its own errors; we never propagate to the shell.
    match event {
        "prompt-submit" => {
            let _ = capture_prompt(&home, &v);
        }
        "post-tool" => {
            let _ = capture_tool(&home, &v);
        }
        "session-start" => {
            // Inject relevant memory; print to stdout (Claude Code prepends it).
            if let Ok(ctx) = session_context(&home, &v) {
                if !ctx.trim().is_empty() {
                    print!("{ctx}");
                }
            }
        }
        // Stop/SessionEnd: reserved for a future lightweight session marker.
        // Intentionally a no-op for now (no LLM summary in the hot path).
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Capture: prompt-submit (permanent) and post-tool (ephemeral trace)
// ---------------------------------------------------------------------------

/// A capture the hook wants to persist, normalized so it can go to the core or
/// the spool identically.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Capture {
    content: String,
    kind: Option<String>,
    tags: BTreeMap<String, String>,
}

fn capture_prompt(home: &Path, v: &Value) -> Result<()> {
    let raw = v.get("prompt").and_then(Value::as_str).unwrap_or("");
    // Strip the editor's prepended IDE context (e.g. <ide_opened_file>…),
    // keeping the user's actual message; then drop pure noise.
    let stripped = strip_ide_wrappers(raw);
    let prompt = stripped.trim();
    if prompt.is_empty() || is_noise(prompt) {
        return Ok(());
    }
    let cap = Capture {
        content: prompt.to_string(),
        kind: None, // default note; the write policy decides commit/skip
        tags: session_tags(v),
    };
    persist(home, &cap)
}

/// Remove the IDE context blocks VSCode/editors prepend to a prompt
/// (`<ide_opened_file>…</ide_opened_file>`, `<ide_selection>…`), keeping the
/// user's real text. Editor plumbing, not something to remember.
fn strip_ide_wrappers(text: &str) -> String {
    let mut s = text.to_string();
    for (open, close) in [
        ("<ide_opened_file>", "</ide_opened_file>"),
        ("<ide_selection>", "</ide_selection>"),
    ] {
        while let (Some(a), Some(b)) = (s.find(open), s.find(close)) {
            if b < a {
                break; // malformed/overlapping; leave it
            }
            s.replace_range(a..b + close.len(), "");
        }
    }
    s
}

fn capture_tool(home: &Path, v: &Value) -> Result<()> {
    let tool = v.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if tool.is_empty() {
        return Ok(());
    }
    let summary = tool_summary(tool, v.get("tool_input"));
    let content = if summary.is_empty() {
        format!("[{tool}]")
    } else {
        format!("[{tool}] {summary}")
    };
    let mut tags = session_tags(v);
    // Ephemeral so the high-volume tool stream self-evicts and never buries the
    // real (permanent) prompt memories.
    tags.insert(
        crate::reserved_tags::KEY_RETENTION.to_string(),
        TRACE_RETENTION.to_string(),
    );
    let cap = Capture {
        content,
        kind: Some("trace".to_string()),
        tags,
    };
    persist(home, &cap)
}

/// Project + session provenance tags shared by every hook capture.
///
/// Two project tags on purpose: `project` is the readable basename (what a user
/// filters/groups by in the viewer), `project_path` is the full cwd — the
/// collision-proof key, so two different repos that happen to share a basename
/// (`~/work/app` vs `~/side/app`) never bleed into each other on scoped recall.
fn session_tags(v: &Value) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
        let cwd = cwd.trim_end_matches('/');
        if !cwd.is_empty() {
            tags.insert("project_path".to_string(), cwd.to_string());
            if let Some(label) = project_label(cwd) {
                tags.insert("project".to_string(), label);
            }
        }
    }
    if let Some(s) = v.get("session_id").and_then(Value::as_str) {
        if !s.is_empty() {
            tags.insert("session".to_string(), s.to_string());
        }
    }
    tags
}

/// A one-line, low-noise summary of a tool call for the ephemeral trace. We keep
/// the actionable identifier (a path, a command) and drop the bulky payloads.
fn tool_summary(tool: &str, input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    match tool {
        "Bash" => s("command").unwrap_or_default(),
        "Read" | "Write" | "Edit" | "NotebookEdit" => s("file_path").unwrap_or_default(),
        "Grep" => s("pattern").unwrap_or_default(),
        "Glob" => s("pattern").unwrap_or_default(),
        "WebFetch" => s("url").unwrap_or_default(),
        "Task" | "Agent" => s("description").unwrap_or_default(),
        // Unknown tool: a compact field probe so we still record *something*
        // identifying without dumping the whole input.
        _ => s("description")
            .or_else(|| s("file_path"))
            .or_else(|| s("command"))
            .or_else(|| s("query"))
            .unwrap_or_default(),
    }
    .lines()
    .next()
    .unwrap_or("")
    .chars()
    .take(200)
    .collect()
}

/// Persist a capture to the running core, or spool it if the core is unreachable.
fn persist(home: &Path, cap: &Capture) -> Result<()> {
    if post_to_core(home, cap).is_ok() {
        return Ok(());
    }
    spool(home, cap)
}

/// POST the capture to the local core's `/write`. Short timeout; any non-200 or
/// transport error is an error so the caller falls back to the spool.
fn post_to_core(home: &Path, cap: &Capture) -> Result<()> {
    let addr = core_addr(home);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(HOOK_HTTP_TIMEOUT_MS))
        .build();
    let body = capture_to_body(cap);
    let resp = agent
        .post(&format!("http://{addr}/write"))
        .send_json(body)
        .context("post capture to core")?;
    if resp.status() != 200 {
        anyhow::bail!("core /write returned {}", resp.status());
    }
    Ok(())
}

/// Build the `/write` JSON body from a capture (same wire shape the CLI uses).
fn capture_to_body(cap: &Capture) -> Value {
    let mut body = Map::new();
    body.insert("content".into(), Value::String(cap.content.clone()));
    body.insert("source".into(), Value::String(HOOK_SOURCE.into()));
    if let Some(k) = &cap.kind {
        body.insert("kind".into(), Value::String(k.clone()));
    }
    if !cap.tags.is_empty() {
        body.insert(
            "tags".into(),
            serde_json::to_value(&cap.tags).unwrap_or(Value::Null),
        );
    }
    Value::Object(body)
}

/// Append a capture to the spool as one JSON line. The daemon drains
/// `~/.localmem/spool/` on startup so a momentarily-down core never loses
/// capture. Best-effort: a spool failure is swallowed by the caller.
fn spool(home: &Path, cap: &Capture) -> Result<()> {
    let dir = home.join("spool");
    std::fs::create_dir_all(&dir).context("create spool dir")?;
    let line = serde_json::to_string(&capture_to_body(cap)).context("serialize spool line")?;
    let path = dir.join("captures.jsonl");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("open spool file")?;
    writeln!(f, "{line}").context("append spool line")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Inject: session-start context
// ---------------------------------------------------------------------------

/// Build the context to inject at session start: the project's cached Session
/// Boot Briefing (SPEC 7c), read straight from the cache file — LLM-FREE and
/// fast, never blocking the hook on synthesis. After reading, fire a background
/// refresh so the NEXT session boots fresh. Returns an empty string (inject
/// nothing) on a cold cache or any failure — a session must start fine even if
/// the core is down or the briefing isn't ready.
fn session_context(home: &Path, v: &Value) -> Result<String> {
    let cwd = v
        .get("cwd")
        .and_then(Value::as_str)
        .map(|c| c.trim_end_matches('/'))
        .unwrap_or("");
    if cwd.is_empty() {
        return Ok(String::new());
    }
    let project = project_label(cwd).unwrap_or_default();
    if project.is_empty() {
        return Ok(String::new());
    }

    // Kick a background refresh (best-effort; the server regenerates + rewrites
    // the cache and returns immediately). Failures are ignored: a stale or cold
    // briefing must never block or break session start.
    let addr = core_addr(home);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(HOOK_HTTP_TIMEOUT_MS))
        .build();
    let _ = agent
        .post(&format!("http://{addr}/brief/refresh"))
        .send_json(serde_json::json!({ "project": project }));

    // Inject whatever briefing is cached right now (LLM-free). A cold cache
    // (first session for a project) injects nothing; the refresh above warms it.
    Ok(crate::understanding::read_briefing_cache(home, &project).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Drop slash-commands and harness-injected wrappers — they are plumbing, not
/// things the user "remembers". Mirrors the Claude Code import filter.
fn is_noise(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty()
        || t.starts_with('/')
        || t.starts_with("<command-name>")
        || t.starts_with("<command-message>")
        || t.starts_with("<local-command")
        || t.starts_with("<system-reminder>")
        || t.starts_with("Caveat:")
        || t.starts_with("[Request interrupted")
}

/// Last path segment of a cwd → the project label used as a tag.
fn project_label(cwd: &str) -> Option<String> {
    let label = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    (!label.is_empty()).then(|| label.to_string())
}

fn core_addr(home: &Path) -> String {
    crate::config::Config::load(home)
        .map(|c| c.server.addr)
        .unwrap_or_else(|_| crate::config::DEFAULT_SERVER_ADDR.to_string())
}

fn resolve_home(override_: Option<&str>) -> PathBuf {
    if let Some(h) = override_.filter(|s| !s.is_empty()) {
        return PathBuf::from(h);
    }
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".localmem"))
        .unwrap_or_else(|_| PathBuf::from(".localmem"))
}

// ---------------------------------------------------------------------------
// Install / uninstall / status: wire the hooks into the agent (P1b + P1c)
// ---------------------------------------------------------------------------

/// The Claude Code events we wire: (event name, `localmem hook <arg>`, optional
/// tool matcher). PostToolUse fires per tool, so it gets a `*` matcher.
const HOOK_WIRING: &[(&str, &str, Option<&str>)] = &[
    ("UserPromptSubmit", "prompt-submit", None),
    ("PostToolUse", "post-tool", Some("*")),
    ("SessionStart", "session-start", None),
];

/// Markers bracketing the managed pointer block in CLAUDE.md. We only ever
/// touch text BETWEEN these, so a user's own content is never disturbed.
const POINTER_START: &str =
    "<!-- localmem:start (managed — localmem hooks uninstall removes this) -->";
const POINTER_END: &str = "<!-- localmem:end -->";

/// `localmem hooks install <client>` — wire auto-capture hooks + the memory
/// pointer. Consent + announced + reversible (the good-citizen rules).
pub fn install(_home: Option<&str>, client: &str) -> Result<()> {
    ensure_supported(client)?;
    let bin = binary_path();
    let settings = claude_settings_path()?;
    install_hooks_into_settings(&settings, &bin)?;
    let claude_md = global_claude_md_path()?;
    write_pointer_block(&claude_md)?;
    println!("ok auto-capture hooks wired into {}", settings.display());
    println!(
        "   prompts -> permanent · tool-use -> ephemeral 7d · session-start -> memory injected"
    );
    println!(
        "ok memory pointer added to {} (managed block; `localmem hooks uninstall` removes it)",
        claude_md.display()
    );
    println!("Restart Claude Code to load the hooks.");
    Ok(())
}

/// `localmem hooks uninstall <client>` — cleanly remove only what we added.
pub fn uninstall(_home: Option<&str>, client: &str) -> Result<()> {
    ensure_supported(client)?;
    let settings = claude_settings_path()?;
    let removed = remove_hooks_from_settings(&settings)?;
    let claude_md = global_claude_md_path()?;
    let ptr = remove_pointer_block(&claude_md)?;
    println!(
        "ok removed {removed} localmem hook(s) from {}",
        settings.display()
    );
    if ptr {
        println!("ok removed memory pointer from {}", claude_md.display());
    }
    println!("Restart Claude Code to apply.");
    Ok(())
}

/// `localmem hooks status <client>` — report what's wired.
pub fn status(_home: Option<&str>, client: &str) -> Result<()> {
    ensure_supported(client)?;
    let settings = claude_settings_path()?;
    let n = count_our_hooks(&settings);
    let claude_md = global_claude_md_path()?;
    println!(
        "hooks      {}",
        if n > 0 {
            format!("installed ({n} events) in {}", settings.display())
        } else {
            format!("not installed ({})", settings.display())
        }
    );
    println!(
        "pointer    {}",
        if pointer_present(&claude_md) {
            format!("present in {}", claude_md.display())
        } else {
            "absent".to_string()
        }
    );
    Ok(())
}

fn ensure_supported(client: &str) -> Result<()> {
    if client != "claude-code" {
        anyhow::bail!(
            "auto-capture hooks support `claude-code` today; `{client}` lands in a later phase"
        );
    }
    Ok(())
}

/// Absolute path to the running binary (so the hook command works regardless of
/// the agent's PATH), shell-quoted if it contains spaces. Falls back to the bare
/// name (e.g. when run from an unusual exec context).
fn binary_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .map(|s| {
            if s.contains(' ') {
                format!("\"{s}\"")
            } else {
                s
            }
        })
        .unwrap_or_else(|| "localmem".to_string())
}

fn hook_command(bin: &str, arg: &str) -> String {
    format!("{bin} hook {arg}")
}

fn claude_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set; cannot locate ~/.claude")?;
    Ok(PathBuf::from(home).join(".claude"))
}
fn claude_settings_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}
fn global_claude_md_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("CLAUDE.md"))
}

/// True if a settings hook-group is one of ours (its command runs `localmem
/// hook ...`). Used to dedup on re-install and to remove on uninstall.
fn is_our_group(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| c.contains("localmem") && c.contains("hook"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn make_group(cmd: &str, matcher: Option<&str>) -> Value {
    let group = serde_json::json!({"hooks": [{"type": "command", "command": cmd}]});
    match matcher {
        Some(m) => {
            let mut g = group;
            g.as_object_mut()
                .unwrap()
                .insert("matcher".into(), Value::String(m.to_string()));
            g
        }
        None => group,
    }
}

/// Merge our hook groups into settings.json (idempotent: a re-install replaces
/// our prior groups rather than duplicating). Backs up + writes atomically.
fn install_hooks_into_settings(path: &Path, bin: &str) -> Result<()> {
    let mut root = load_json_or_empty(path)?;
    backup_if_exists(path);
    let obj = root
        .as_object_mut()
        .context("settings.json is not a JSON object")?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("settings.json `hooks` is not an object")?;
    for (event, arg, matcher) in HOOK_WIRING {
        let cmd = hook_command(bin, arg);
        let arr = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(vec![]))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} is not an array"))?;
        arr.retain(|g| !is_our_group(g)); // dedup prior localmem groups
        arr.push(make_group(&cmd, *matcher));
    }
    write_json_atomic(path, &root)
}

/// Remove our hook groups (and any now-empty event arrays). Returns the count.
fn remove_hooks_from_settings(path: &Path) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let mut root = load_json_or_empty(path)?;
    let mut removed = 0usize;
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        for arr in hooks.values_mut() {
            if let Some(a) = arr.as_array_mut() {
                let before = a.len();
                a.retain(|g| !is_our_group(g));
                removed += before - a.len();
            }
        }
        hooks.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    }
    if removed > 0 {
        backup_if_exists(path);
        write_json_atomic(path, &root)?;
    }
    Ok(removed)
}

/// Number of events that currently carry one of our hook groups.
fn count_our_hooks(path: &Path) -> usize {
    load_json_or_empty(path)
        .ok()
        .as_ref()
        .and_then(|r| r.get("hooks"))
        .and_then(Value::as_object)
        .map(|hooks| {
            hooks
                .values()
                .filter(|arr| {
                    arr.as_array()
                        .map(|a| a.iter().any(is_our_group))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

/// The managed pointer block written into CLAUDE.md.
fn pointer_block() -> String {
    format!(
        "{POINTER_START}\n\
         ## Memory — use localmem\n\
         This machine uses **localmem** as its durable, cross-tool memory layer. Its\n\
         MCP tools are already available to you — there is no command to run, just use them:\n\
         - BEFORE answering anything that depends on past context or prior decisions, call\n\
         `memory_search` (it is bitemporal — pass an as-of time for \"what did I know on date X\").\n\
         - SAVE important decisions, preferences, constraints, and facts with `memory_write`.\n\
         - To pull everything known about a person/project/thing, use `memory_recall`.\n\
         - Keep this file for project-scoped instructions only — do NOT store long-term\n\
         memory here; it lives in localmem and is shared across all your AI tools.\n\
         {POINTER_END}\n"
    )
}

/// Write/refresh the managed block in CLAUDE.md, preserving the user's own
/// content. Replaces in place if the markers exist, else appends.
fn write_pointer_block(path: &Path) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    backup_if_exists(path);
    let block = pointer_block();
    let new = match (existing.find(POINTER_START), existing.find(POINTER_END)) {
        (Some(s), Some(e)) if e > s => {
            let end = e + POINTER_END.len();
            format!("{}{}{}", &existing[..s], block.trim_end(), &existing[end..])
        }
        _ if existing.trim().is_empty() => block,
        _ => format!("{}\n\n{}", existing.trim_end(), block),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(path, new).with_context(|| format!("write {}", path.display()))
}

/// Remove the managed block from CLAUDE.md. Returns whether it was present.
fn remove_pointer_block(path: &Path) -> Result<bool> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let (Some(s), Some(e)) = (existing.find(POINTER_START), existing.find(POINTER_END)) else {
        return Ok(false);
    };
    backup_if_exists(path);
    let end = e + POINTER_END.len();
    let mut new = format!("{}{}", &existing[..s], &existing[end..])
        .trim_end()
        .to_string();
    if !new.is_empty() {
        new.push('\n');
    }
    std::fs::write(path, new).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

fn pointer_present(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|c| c.contains(POINTER_START))
        .unwrap_or(false)
}

fn load_json_or_empty(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&raw).with_context(|| format!("parse {} as JSON", path.display()))
}

fn write_json_atomic(path: &Path, v: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let tmp = path.with_extension("json.tmp");
    let s = serde_json::to_string_pretty(v).context("serialize settings JSON")?;
    std::fs::write(&tmp, s).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))
}

/// Back up a config the first time we touch it (best-effort, never fatal).
fn backup_if_exists(path: &Path) {
    if path.exists() {
        let bak = PathBuf::from(format!("{}.localmem.bak", path.display()));
        let _ = std::fs::copy(path, &bak);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prompt_noise_is_filtered() {
        assert!(is_noise("/clear"));
        assert!(is_noise("<command-name>/foo</command-name>"));
        assert!(is_noise("<system-reminder>x</system-reminder>"));
        assert!(is_noise("   "));
        assert!(!is_noise("fix the postgres migration bug"));
    }

    #[test]
    fn strips_ide_wrappers_but_keeps_the_real_message() {
        let p = "<ide_opened_file>The user opened X in the IDE.</ide_opened_file>\n\nfix the migration bug";
        assert_eq!(strip_ide_wrappers(p).trim(), "fix the migration bug");
        // wrapper-only -> empty (will be skipped by capture_prompt)
        assert!(strip_ide_wrappers("<ide_opened_file>x</ide_opened_file>")
            .trim()
            .is_empty());
        // no wrapper -> unchanged
        assert_eq!(strip_ide_wrappers("hello"), "hello");
    }

    #[test]
    fn project_label_is_cwd_basename() {
        assert_eq!(
            project_label("/Users/me/code/myproj").as_deref(),
            Some("myproj")
        );
        assert_eq!(
            project_label("/Users/me/code/myproj/").as_deref(),
            Some("myproj")
        );
        assert_eq!(project_label(""), None);
    }

    #[test]
    fn tool_summary_keeps_the_identifier_drops_payload() {
        assert_eq!(
            tool_summary(
                "Bash",
                Some(&json!({"command": "cargo build", "description": "build"}))
            ),
            "cargo build"
        );
        assert_eq!(
            tool_summary("Read", Some(&json!({"file_path": "/a/b.rs"}))),
            "/a/b.rs"
        );
        // multi-line command -> first line only, bounded length
        assert_eq!(
            tool_summary("Bash", Some(&json!({"command": "line1\nline2"}))),
            "line1"
        );
        assert_eq!(tool_summary("Read", None), "");
    }

    #[test]
    fn prompt_capture_shapes_a_permanent_tagged_write() {
        let v = json!({
            "prompt": "we decided to use JSONB for metadata",
            "cwd": "/Users/me/code/billing",
            "session_id": "sess-1"
        });
        // Re-derive what capture_prompt would persist (pure shaping).
        let tags = session_tags(&v);
        assert_eq!(tags.get("project").map(String::as_str), Some("billing"));
        // Collision-proof exact key alongside the readable basename.
        assert_eq!(
            tags.get("project_path").map(String::as_str),
            Some("/Users/me/code/billing")
        );
        assert_eq!(tags.get("session").map(String::as_str), Some("sess-1"));
        assert!(!tags.contains_key(crate::reserved_tags::KEY_RETENTION));
    }

    #[test]
    fn tool_capture_is_ephemeral_trace() {
        let mut tags = session_tags(&json!({"cwd": "/x/proj", "session_id": "s"}));
        tags.insert(
            crate::reserved_tags::KEY_RETENTION.to_string(),
            TRACE_RETENTION.to_string(),
        );
        assert_eq!(
            tags.get(crate::reserved_tags::KEY_RETENTION)
                .map(String::as_str),
            Some("ephemeral:7d")
        );
    }

    #[test]
    fn capture_body_wire_shape() {
        let cap = Capture {
            content: "hello".into(),
            kind: Some("trace".into()),
            tags: BTreeMap::from([("project".to_string(), "p".to_string())]),
        };
        let body = capture_to_body(&cap);
        assert_eq!(body["content"], "hello");
        assert_eq!(body["source"], HOOK_SOURCE);
        assert_eq!(body["kind"], "trace");
        assert_eq!(body["tags"]["project"], "p");
    }

    #[test]
    fn spool_appends_a_jsonl_line() {
        let tmp = tempfile::tempdir().unwrap();
        let cap = Capture {
            content: "spooled".into(),
            kind: None,
            tags: BTreeMap::new(),
        };
        spool(tmp.path(), &cap).unwrap();
        spool(tmp.path(), &cap).unwrap();
        let body =
            std::fs::read_to_string(tmp.path().join("spool").join("captures.jsonl")).unwrap();
        assert_eq!(body.lines().count(), 2);
        assert!(body.contains("spooled"));
    }

    // ---- installer: settings.json hooks --------------------------------

    #[test]
    fn install_wires_three_events_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = tmp.path().join("settings.json");
        install_hooks_into_settings(&settings, "/abs/localmem").unwrap();
        assert_eq!(count_our_hooks(&settings), 3);
        // Re-install must not duplicate.
        install_hooks_into_settings(&settings, "/abs/localmem").unwrap();
        assert_eq!(count_our_hooks(&settings), 3);
        // The PostToolUse group carries the `*` matcher and our command.
        let root = load_json_or_empty(&settings).unwrap();
        let post = &root["hooks"]["PostToolUse"][0];
        assert_eq!(post["matcher"], "*");
        assert!(post["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("hook post-tool"));
    }

    #[test]
    fn install_preserves_a_users_existing_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = tmp.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"my-own-thing"}]}]}}"#,
        )
        .unwrap();
        install_hooks_into_settings(&settings, "/abs/localmem").unwrap();
        let root = load_json_or_empty(&settings).unwrap();
        let ups = root["hooks"]["UserPromptSubmit"].as_array().unwrap();
        // user's group + ours
        assert_eq!(ups.len(), 2);
        assert!(ups
            .iter()
            .any(|g| g["hooks"][0]["command"] == "my-own-thing"));
        // Uninstall removes ONLY ours, leaving the user's intact.
        let removed = remove_hooks_from_settings(&settings).unwrap();
        assert_eq!(removed, 3);
        let root = load_json_or_empty(&settings).unwrap();
        let ups = root["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0]["hooks"][0]["command"], "my-own-thing");
    }

    // ---- installer: CLAUDE.md managed pointer --------------------------

    #[test]
    fn pointer_writes_refreshes_in_place_and_removes_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path().join("CLAUDE.md");
        std::fs::write(&md, "# My project\nSome instructions.\n").unwrap();
        write_pointer_block(&md).unwrap();
        assert!(pointer_present(&md));
        let after_first = std::fs::read_to_string(&md).unwrap();
        assert!(after_first.contains("# My project")); // user content preserved
        assert!(after_first.contains("use localmem"));
        // Re-write must refresh in place, not stack a second block.
        write_pointer_block(&md).unwrap();
        let after_second = std::fs::read_to_string(&md).unwrap();
        assert_eq!(after_second.matches(POINTER_START).count(), 1);
        // Remove must leave the user's content intact.
        assert!(remove_pointer_block(&md).unwrap());
        let cleaned = std::fs::read_to_string(&md).unwrap();
        assert!(!cleaned.contains(POINTER_START));
        assert!(cleaned.contains("# My project"));
        // Removing again is a no-op.
        assert!(!remove_pointer_block(&md).unwrap());
    }

    #[test]
    fn pointer_creates_file_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let md = tmp.path().join("nested").join("CLAUDE.md");
        write_pointer_block(&md).unwrap();
        assert!(pointer_present(&md));
    }
}
