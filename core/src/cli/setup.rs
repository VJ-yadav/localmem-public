//! `localmem setup` — one-command onboarding (Phase 5).
//!
//! Composes the steps a new user would otherwise run by hand: initialize the
//! home, fetch the embedder model, wire detected MCP clients, and verify. Every
//! step is best-effort: a failure (no network for the model, a client that
//! isn't installed) prints a note and setup continues, so the user always ends
//! up with a working local-first install. `--no-model` skips the (largest)
//! download for an offline or minimal setup.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// MCP clients setup attempts to wire. Only the installed ones succeed; the
/// rest are skipped with a note.
const CLIENTS: &[&str] = &["claude-code", "claude", "cursor", "windsurf", "cline"];

pub fn run(home: Option<&str>, no_model: bool, no_service: bool, json: bool) -> Result<()> {
    let home_path = resolve_home(home)?;
    let core_addr = crate::config::Config::load(&home_path)
        .map(|c| c.server.addr)
        .unwrap_or_else(|_| crate::config::DEFAULT_SERVER_ADDR.to_string());

    println!("localmem setup");

    // 1. Initialize the home (idempotent).
    match crate::cli::init::init_home(&home_path) {
        Ok(_) => println!("[1/5] home ready at {}", home_path.display()),
        Err(e) => println!("[1/5] init failed: {e:#}"),
    }

    // 2. Embedder + reranker models (best-effort; lexical + facts work without
    // them). The reranker backs the default-on [retriever].rerank: fetching it
    // here keeps the shipped config coherent, so a fresh install gets the
    // precision that cleared the 75% eval gate instead of silently degrading to
    // first-stage ranking.
    if no_model {
        println!("[2/5] skipping model fetch (--no-model); search stays lexical-only");
    } else {
        println!("[2/5] fetching embedder model (bge-small):");
        if let Err(e) = crate::cli::fetch_model::run(home, None, None, false, json) {
            println!("    skipped: {e:#}");
            println!("    search runs lexical-only until you run `localmem fetch-model`");
        }
        println!("[2/5] fetching reranker model (ms-marco-MiniLM-L-6-v2):");
        if let Err(e) = crate::cli::fetch_model::run(home, Some("reranker"), None, false, json) {
            println!("    skipped: {e:#}");
            println!("    rerank stays off until you run `localmem fetch-model reranker`");
        }
    }

    // 3. Wire MCP clients (best-effort; only installed clients succeed).
    println!("[3/5] wiring MCP clients:");
    let mut wired: Vec<&str> = Vec::new();
    for client in CLIENTS {
        match crate::cli::mcp::run_install(home, client, &core_addr, json) {
            Ok(_) => {
                println!("    + {client}");
                wired.push(client);
            }
            Err(e) => println!("    - {client}: skipped ({e})"),
        }
    }
    if wired.is_empty() {
        println!(
            "    no MCP clients wired yet — install one, then `localmem mcp install <client>`"
        );
    }

    // 4. Always-on auto-launch service (best-effort), so the core runs at login
    // without the user keeping a terminal open. Reversible: `service uninstall`.
    let mut serviced = false;
    if no_service {
        println!("[4/5] skipping auto-launch service (--no-service)");
    } else {
        println!("[4/5] installing always-on service:");
        match crate::cli::service::run("install", home) {
            Ok(_) => serviced = true, // service::install prints its own lines
            Err(e) => {
                println!("    skipped: {e:#}");
                println!("    start it yourself with `localmem serve`, or `localmem service install` later");
            }
        }
    }

    // 5. Verify.
    println!("[5/5] verifying:");
    for c in crate::cli::doctor::run_checks(&home_path, &core_addr) {
        println!("    {:?} {} — {}", c.status, c.name, c.detail);
    }

    // The point users miss: this is ONE store under all their AI tools, not a
    // per-tool plugin. Say it explicitly so they understand what they just got.
    println!();
    match wired.len() {
        0 => {
            println!("No AI tools are wired yet. Install one (Claude Code, Cursor, ...),");
            println!("then `localmem mcp install <client>` — every tool you wire shares");
            println!("this one memory at {}.", home_path.display());
        }
        1 => println!(
            "✓ {} is now wired to your memory at {}.",
            wired[0],
            home_path.display()
        ),
        n => {
            println!(
                "✓ {n} AI tools now share ONE memory at {}: {}.",
                home_path.display(),
                wired.join(", ")
            );
            println!("  A memory written in any one of them shows up in all the others.");
        }
    }

    println!();
    if serviced {
        println!("Setup complete. The core is running as an always-on service.");
        println!("Check anytime:    localmem status   ·   localmem service status");
    } else {
        println!("Setup complete. Start the core:  localmem serve");
        println!("Check anytime with:              localmem status");
    }

    // Shared onboarding next-steps: rendered from the SAME source the MCP
    // `localmem://getting-started` resource shows, so every entry point gives
    // identical guidance (§8, the cohesion principle).
    let understanding_enabled = crate::config::Config::load(home_path.as_ref())
        .map(|c| c.understanding.enabled)
        .unwrap_or(false);
    let mut import_candidates = 0usize;
    if let Ok(dets) = crate::cli::import_wizard::scan_default_locations() {
        import_candidates += dets.len();
    }
    let clients = crate::cli::mcp::all_clients_status(None);
    let gs = crate::onboarding::build(
        crate::onboarding::dashboard_url(&core_addr),
        crate::onboarding::model_present(home_path.as_ref()),
        serviced || crate::onboarding::core_reachable(&core_addr),
        &clients,
        understanding_enabled,
        import_candidates,
    );
    println!();
    print!("{}", gs.render_terminal());
    println!("Learn more:                      https://localmem.org");
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
