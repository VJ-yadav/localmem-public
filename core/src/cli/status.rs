//! `localmem status` — concise health + memory summary (Phase 5).
//!
//! A lightweight companion to `localmem doctor`: doctor diagnoses problems and
//! offers fixes; status answers "is it running, and how much do I have?" in a
//! few lines. Read-only and side-effect-free (it never creates stores), so it's
//! safe to call from a dashboard health pill.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn run(home: Option<&str>, core_addr: &str, as_json: bool) -> Result<()> {
    let home_path = resolve_home(home)?;
    let initialized = home_path.join(crate::event_log::EVENTS_FILE).exists();
    let model_present = crate::embed::model_present(&home_path);
    let server_up = probe_health(core_addr);
    let events = count_events(&home_path);
    let entities = count_entities(&home_path);
    // Decomposition backlog: how much SIGNAL the understanding layer has actually
    // processed. Read straight from the event log (not the DuckDB facts store), so
    // it stays accurate even while the running service holds the facts lock — and
    // a backlog that built up while the LLM backend was off is visible here, not
    // just in `localmem understand` or the dashboard.
    let coverage = compute_coverage(&home_path);
    // The AI tools wired to localmem. They all read/write this one store via the
    // core, so surfacing them is what makes the shared-memory model legible.
    let tools = crate::cli::mcp::wired_clients(None);

    if as_json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "home": home_path.display().to_string(),
                "initialized": initialized,
                "server_up": server_up,
                "core_addr": core_addr,
                "model_present": model_present,
                "events": events,
                "entities": entities,
                "decomposed": coverage.decomposed,
                "signal_captures": coverage.signal_captures,
                "undecomposed": coverage.undecomposed(),
                "coverage_percent": coverage.percent(),
                "tools": tools,
            })
        );
        return Ok(());
    }

    println!("localmem status");
    println!("  home       {}", home_path.display());
    if server_up {
        println!("  server     up ({core_addr})");
    } else {
        println!("  server     down — run `localmem serve`");
    }
    if model_present {
        println!("  embedder   present (hybrid search)");
    } else {
        println!("  embedder   absent — lexical-only (run `localmem fetch-model`)");
    }
    println!("  events     {events}");
    match entities {
        Some(n) => println!("  entities   {n}"),
        None => println!(
            "  entities   unavailable (the running service holds the database — see the dashboard)"
        ),
    }
    // Only speak up when there is signal to decompose; an empty store owes nothing.
    if coverage.signal_captures > 0 {
        if coverage.undecomposed() > 0 {
            println!(
                "  decomp     {}/{} understood ({}%) — {} undecomposed; run `localmem understand --backfill`",
                coverage.decomposed,
                coverage.signal_captures,
                coverage.percent(),
                coverage.undecomposed(),
            );
        } else {
            println!(
                "  decomp     {}/{} understood (100%)",
                coverage.decomposed, coverage.signal_captures,
            );
        }
    }
    if tools.is_empty() {
        println!("  tools      none wired yet — `localmem setup` connects your AI apps");
    } else {
        println!(
            "  tools      {} sharing this memory: {}",
            tools.len(),
            tools.join(", ")
        );
    }
    if !initialized {
        println!("  note       home not initialized — run `localmem setup`");
    }
    Ok(())
}

fn probe_health(core_addr: &str) -> bool {
    let url = format!("http://{core_addr}/health");
    let client = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(1000))
        .build();
    matches!(client.get(&url).call(), Ok(r) if r.status() == 200)
}

/// Count events without creating the log (returns 0 if not initialized).
fn count_events(home: &Path) -> u64 {
    if !home.join(crate::event_log::EVENTS_FILE).exists() {
        return 0;
    }
    crate::event_log::EventLog::open(home)
        .and_then(|log| Ok(log.iter()?.count() as u64))
        .unwrap_or(0)
}

/// Count distinct fact subjects. `Some(0)` when no facts exist yet; `None` when
/// the store can't be read (e.g. the running service holds the DuckDB write
/// lock). Returning `None` instead of `0` kills the silent-degrade where status
/// showed `entities 0` while facts.duckdb held thousands of subjects.
fn count_entities(home: &Path) -> Option<u64> {
    let facts_path = home
        .join(crate::facts::FACTS_DIR)
        .join(crate::facts::FACTS_FILE);
    if !facts_path.exists() {
        return Some(0);
    }
    crate::facts::FactsStore::open(home)
        .and_then(|store| Ok(store.subjects()?.len() as u64))
        .ok()
}

/// Decomposition coverage straight from the event log (returns an empty/full
/// coverage if not initialized). Reads only the append-only `events.jsonl`, so it
/// never touches the DuckDB facts store the running service locks.
fn compute_coverage(home: &Path) -> crate::understanding::Coverage {
    if !home.join(crate::event_log::EVENTS_FILE).exists() {
        return crate::understanding::compute_coverage(std::iter::empty());
    }
    match crate::event_log::EventLog::open(home).and_then(|log| Ok(log.iter()?)) {
        Ok(iter) => crate::understanding::compute_coverage(iter.filter_map(|r| r.ok())),
        Err(_) => crate::understanding::compute_coverage(std::iter::empty()),
    }
}

fn resolve_home(override_: Option<&str>) -> Result<PathBuf> {
    if let Some(h) = override_.filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var("HOME")
        .context("HOME environment variable is not set; pass --home explicitly")?;
    Ok(PathBuf::from(home).join(".localmem"))
}
