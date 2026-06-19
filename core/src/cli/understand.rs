//! `localmem understand` — readiness + setup UX for the local-LLM understanding
//! layer (the async worker that decomposes each capture into summary + entities
//! + intent on top of the raw text).
//!
//! This command is the user-facing front door to that layer: it detects Ollama,
//! checks the configured model, and explains — in plain terms — what enabling it
//! does, what it costs (nothing), and what it touches (only your machine). It
//! NEVER auto-installs Ollama or pulls a model without the user running the
//! shown command: installing a third-party runtime silently is exactly the
//! anti-pattern we rejected (see SPEC-unified-memory-layer Decision D).

use crate::understanding::{installed_models, resolve_model, ModelResolution};
use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn run_status(home: Option<&str>) -> Result<()> {
    let home = resolve_home(home);
    let cfg = crate::config::Config::load(&home).unwrap_or_default();
    // Report the config the ASYNC understanding worker actually uses
    // ([understanding]), not the legacy sync extractor path
    // ([extractor].plugins). Reading the wrong section is how this status used
    // to disagree with what the worker did.
    let endpoint = cfg.understanding.ollama_endpoint.clone();
    let model = cfg.understanding.model.clone();
    let enabled = cfg.understanding.enabled;

    println!("Understanding layer (local LLM) — what it is");
    println!("  Adds, on top of the raw text we always keep, a short summary + the key");
    println!("  entities + the intent/decision of each memory — so recall and the viewer");
    println!("  show meaning, not a wall of raw prompts.");
    println!();
    println!("  Cost + privacy (the honest version):");
    println!("  - FREE. Runs a local model via Ollama; no API key, no subscription.");
    println!("  - PRIVATE. Nothing leaves your machine — Ollama is loopback ({endpoint}).");
    println!("  - It uses some background CPU/RAM while it thinks, throttled and OFF the");
    println!("    write path, so your prompts and captures never wait on it.");
    println!("  - Entirely optional: with it off, localmem still captures + searches as now.");
    println!();

    // Verify against reality: ask Ollama what is actually installed, then
    // resolve EXACTLY what the worker will do (the same `resolve_model` the
    // server uses at startup), so this status never disagrees with the worker.
    println!("Readiness:");
    let installed = installed_models(&endpoint);
    match &installed {
        Some(models) => println!("  ollama     up — {} model(s) installed", models.len()),
        None => {
            // Ollama is OPTIONAL. localmem captures + searches fully without it,
            // so this is guidance offered once on request, never a demand and
            // never a forced install.
            println!("  ollama     not reachable on {endpoint}");
            println!(
                "             understanding is optional — capture + search work fully without it."
            );
            println!("             to enable later: install https://ollama.com/download,");
            println!(
                "             run `ollama serve`, pull any model, then set [understanding].model"
            );
        }
    }
    if let Some(models) = &installed {
        match resolve_model(Some(models.as_slice()), &model) {
            ModelResolution::Exact(m) => println!("  model      {m}  ✓ ready"),
            ModelResolution::Substituted { used, configured } => {
                // The worker auto-substitutes a same-family tag, so it IS ready,
                // just not on the configured tag. Report that reality.
                println!("  model      {configured}  not installed — worker will use {used} (same family)  ✓");
                println!("             to pin it:  set [understanding].model = \"{used}\"");
            }
            ModelResolution::NoMatch { installed } => {
                println!(
                    "  model      {model}  not installed — worker idle until a model is present"
                );
                println!("             pull it:   ollama pull {model}");
                println!(
                    "             or use an installed one: {}",
                    installed.join(", ")
                );
                println!("             (then set [understanding].model accordingly)");
            }
            // installed is Some here, so resolve_model never returns Unprobed.
            ModelResolution::Unprobed(_) => {}
        }
    }
    println!(
        "  enabled    {}",
        if enabled {
            "yes — [understanding].enabled = true".to_string()
        } else {
            "no — set [understanding].enabled = true in your config.toml to turn it on".to_string()
        }
    );

    // Coverage: never be silently idle. Show how much SIGNAL is actually
    // understood (ephemeral traces are excluded from the denominator), so a
    // stale low number is visible and actionable rather than unseen.
    if let Ok(log) = crate::event_log::EventLog::open(&home) {
        if let Ok(iter) = log.iter() {
            let cov = crate::understanding::compute_coverage(iter.filter_map(|r| r.ok()));
            println!();
            println!(
                "Coverage:  {}/{} signal captures decomposed ({}%)",
                cov.decomposed,
                cov.signal_captures,
                cov.percent()
            );
            if cov.undecomposed() > 0 {
                println!(
                    "           {} undecomposed — run `localmem understand --backfill` (needs the worker running)",
                    cov.undecomposed()
                );
            }
        }
    }
    Ok(())
}

/// `localmem understand --backfill`: ask the running server to enqueue captures
/// that have no understanding yet (idempotent). Routes to the server because the
/// worker lives there; the worker decomposes them asynchronously afterward.
pub async fn run_backfill(
    home: Option<&str>,
    project: Option<String>,
    limit: Option<usize>,
) -> Result<()> {
    let home = resolve_home(home);
    let cfg = crate::config::Config::load(&home).context("load config")?;
    let addr = cfg.server.addr.clone();
    let body = serde_json::json!({ "project": project, "limit": limit });

    let (status, value) =
        tokio::task::spawn_blocking(move || -> Result<(u16, serde_json::Value)> {
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(30))
                .build();
            match agent
                .post(&format!("http://{addr}/understand/backfill"))
                .send_json(body)
            {
                Ok(r) => {
                    let status = r.status();
                    Ok((status, r.into_json().context("parse backfill response")?))
                }
                Err(ureq::Error::Status(code, r)) => {
                    Ok((code, r.into_json().unwrap_or(serde_json::Value::Null)))
                }
                Err(e) => anyhow::bail!(
                    "could not reach the localmem server ({e}). Start it with `localmem serve`."
                ),
            }
        })
        .await
        .context("join backfill request")??;

    if status != 200 {
        let msg = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("backfill failed");
        anyhow::bail!("{msg}");
    }
    let enqueued = value.get("enqueued").and_then(|v| v.as_u64()).unwrap_or(0);
    let remaining = value.get("remaining").and_then(|v| v.as_u64()).unwrap_or(0);
    println!(
        "Enqueued {enqueued} capture(s) for understanding ({remaining} still pending beyond this \
         batch). The worker decomposes them in the background; run `localmem brief` once it settles."
    );
    Ok(())
}

/// `localmem understand --rebuild-graph`: rebuild the typed-graph NODE layer
/// (`entity_mentions`) from the Understanding events already in the log. Offline
/// and idempotent (clears first), so it backfills the P2 node store on an
/// existing home without the cost of a full `replay` (which re-embeds every
/// vector). A subsequent full replay reproduces the same table from the same
/// events, so this never diverges from the recomputable source of truth.
pub fn run_rebuild_graph(home: Option<&str>) -> Result<()> {
    use crate::event::EventKind;
    use crate::event_log::EventLog;
    use std::collections::HashSet;

    let home = resolve_home(home);
    let event_log = EventLog::open(&home).context("open event log")?;
    let facts = crate::cli::open_facts(&home)?;
    facts
        .clear_entity_mentions()
        .context("clear entity mentions before rebuild")?;

    // A capture always precedes its understanding in the log, so a single
    // forward pass knows each understanding's source ephemerality by the time
    // it is reached, matching the replay invariant.
    let mut ephemeral: HashSet<String> = HashSet::new();
    let mut understandings = 0u64;
    let mut mentions = 0u64;
    for ev in event_log.iter().context("open event log iterator")? {
        let ev = ev.context("read event")?;
        match &ev.kind {
            EventKind::Capture(p) if p.is_ephemeral() => {
                ephemeral.insert(ev.id.to_string());
            }
            EventKind::Understanding(p) => {
                if ephemeral.contains(&p.source_id.to_string()) {
                    continue;
                }
                understandings += 1;
                for e in &p.entities {
                    facts
                        .insert_entity_mention(
                            &e.name,
                            &e.kind,
                            p.valid_from,
                            &p.source_id.to_string(),
                        )
                        .context("insert entity mention")?;
                    mentions += 1;
                }
            }
            _ => {}
        }
    }
    let nodes = facts.entity_count().unwrap_or(0);
    println!(
        "Rebuilt typed graph: {nodes} resolved entit{} from {mentions} mention(s) across \
         {understandings} understanding(s).",
        if nodes == 1 { "y" } else { "ies" }
    );
    Ok(())
}

fn resolve_home(override_: Option<&str>) -> PathBuf {
    if let Some(h) = override_.filter(|s| !s.is_empty()) {
        return PathBuf::from(h);
    }
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".localmem"))
        .unwrap_or_else(|_| PathBuf::from(".localmem"))
}
