//! `localmem mcp {install,list,uninstall}` subcommand handlers (T-50).
//!
//! The viral lever of v0.2: a one-liner that wires localmem into any
//! supported MCP-capable AI tool with a backed-up, atomic edit of
//! the client's config file. See `cli/mcp_clients/` for per-client
//! adapters.

use crate::cli::mcp_clients::{
    adapter, default_localmem_entry, ClientId, InstallReceipt, McpServerEntry,
};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::PathBuf;

/// Resolve the user's home directory the way the rest of the CLI
/// does: explicit override > `$HOME` > error.
fn resolve_home(home_override: Option<&str>) -> Result<PathBuf> {
    if let Some(h) = home_override.filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var("HOME")
        .context("HOME environment variable is not set; pass --home explicitly")?;
    Ok(PathBuf::from(home))
}

/// Slugs of MCP clients currently wired to localmem (their config carries the
/// localmem entry). Read-only and best-effort: a client whose config can't be
/// read is treated as not wired. Used by `localmem status` to show that one
/// store sits under several tools. `home_override` mirrors the other handlers;
/// pass `None` to check the real `$HOME` where client configs live.
pub fn wired_clients(home_override: Option<&str>) -> Vec<&'static str> {
    let Ok(home) = resolve_home(home_override) else {
        return Vec::new();
    };
    let mut wired = Vec::new();
    for id in ClientId::all() {
        let a = adapter(*id);
        if a.unsupported_msg().is_none() && matches!(a.is_installed(&home), Ok(true)) {
            wired.push(id.slug());
        }
    }
    wired
}

/// Every SUPPORTED MCP client as `(slug, display_label, wired)`, for the
/// onboarding "wire another client" surface (§8). Skips clients localmem cannot
/// wire on this platform. `wired` is whether localmem is already registered.
pub fn all_clients_status(home_override: Option<&str>) -> Vec<(String, String, bool)> {
    let home = resolve_home(home_override).ok();
    let mut out = Vec::new();
    for id in ClientId::all() {
        let a = adapter(*id);
        if a.unsupported_msg().is_some() {
            continue;
        }
        let wired = home
            .as_ref()
            .map(|h| matches!(a.is_installed(h), Ok(true)))
            .unwrap_or(false);
        out.push((id.slug().to_string(), id.display_name().to_string(), wired));
    }
    out
}

/// `localmem mcp install <client>` handler.
///
/// `core_addr` is the localmem core HTTP server address (used to
/// build the `LOCALMEM_CORE_URL` env var on the MCP entry). Defaults
/// to the value in config.toml; the caller (main.rs) resolves it.
///
/// `home_override` lets tests + advanced users point at a fake home
/// dir without setting `$HOME` process-wide.
pub fn run_install(
    home_override: Option<&str>,
    client_slug: &str,
    core_addr: &str,
    as_json: bool,
) -> Result<()> {
    let id = ClientId::from_slug(client_slug).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown MCP client {client_slug:?}; supported: {}",
            ClientId::all()
                .iter()
                .map(|c| c.slug())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let adapter = adapter(id);
    if let Some(msg) = adapter.unsupported_msg() {
        bail!(msg);
    }
    let home = resolve_home(home_override)?;
    let entry = default_localmem_entry(core_addr)
        .context("resolve localmem MCP server entry (set LOCALMEM_MCP_SERVER_PATH if missing)")?;
    let receipt = adapter
        .install(&home, &entry)
        .with_context(|| format!("install localmem into {}", id.display_name()))?;
    emit_install(id, &entry, &receipt, as_json)?;
    // Render the SHARED onboarding next-steps (§8): the same source `localmem
    // setup` and the MCP `localmem://getting-started` resource use, so a user
    // who arrived via `localmem mcp install` (or `npx localmem-mcp install`,
    // which delegates here) gets identical guidance, never a divergent one.
    if !as_json {
        let understanding = crate::config::Config::load(home.as_ref())
            .map(|c| c.understanding.enabled)
            .unwrap_or(false);
        let imports = crate::cli::import_wizard::scan_default_locations()
            .map(|d| d.len())
            .unwrap_or(0);
        let clients = all_clients_status(home_override);
        let gs = crate::onboarding::build(
            crate::onboarding::dashboard_url(core_addr),
            crate::onboarding::model_present(home.as_ref()),
            crate::onboarding::core_reachable(core_addr),
            &clients,
            understanding,
            imports,
        );
        println!();
        print!("{}", gs.render_terminal());
    }
    Ok(())
}

/// `localmem mcp uninstall <client>` handler. Removes only the
/// `localmem` entry; leaves every other MCP server registered with
/// the client untouched. Reports the file path even when nothing was
/// removed so users can locate the config to inspect manually.
pub fn run_uninstall(home_override: Option<&str>, client_slug: &str, as_json: bool) -> Result<()> {
    let id = ClientId::from_slug(client_slug)
        .ok_or_else(|| anyhow::anyhow!("unknown MCP client {client_slug:?}"))?;
    let adapter = adapter(id);
    if let Some(msg) = adapter.unsupported_msg() {
        bail!(msg);
    }
    let home = resolve_home(home_override)?;
    let removed = adapter
        .uninstall(&home)
        .with_context(|| format!("uninstall localmem from {}", id.display_name()))?;
    let config_path = adapter.config_path(&home);
    emit_uninstall(id, removed, &config_path, as_json)
}

/// `localmem mcp list` handler. Walks every known client and reports
/// whether localmem is currently registered, plus the path where the
/// client would store its config. Unsupported clients (Codex, Aider)
/// surface their unsupported reason so users see the full picture.
pub fn run_list(home_override: Option<&str>, as_json: bool) -> Result<()> {
    let home = resolve_home(home_override)?;
    let mut rows: Vec<ListRow> = Vec::new();
    for id in ClientId::all() {
        let adapter = adapter(*id);
        let path = adapter.config_path(&home);
        let status = match adapter.unsupported_msg() {
            Some(msg) => ListStatus::Unsupported(msg.to_string()),
            None => match adapter.is_installed(&home) {
                Ok(true) => ListStatus::Installed,
                Ok(false) => {
                    if path.exists() {
                        ListStatus::ConfigPresent
                    } else {
                        ListStatus::NotConfigured
                    }
                }
                // Surface the error in the row rather than bailing
                // the entire `list` call: one broken config file
                // shouldn't hide the status of every other client.
                Err(e) => ListStatus::Error(format!("{e:#}")),
            },
        };
        rows.push(ListRow {
            slug: id.slug(),
            display_name: id.display_name(),
            config_path: path,
            status: status.slug(),
            note: status.note().map(str::to_string),
        });
    }
    emit_list(&rows, as_json)
}

/// Per-client install state surfaced by `localmem mcp list`.
/// Using a flat struct (status + optional note string) keeps the
/// JSON shape simple and round-trips cleanly through serde without
/// running into the flatten+tagged-newtype limitation.
#[derive(Debug, Clone)]
enum ListStatus {
    /// Localmem is registered in the client's config.
    Installed,
    /// Client has a config file but no `localmem` entry — typical
    /// fresh-install case.
    ConfigPresent,
    /// No config file at all; the user likely hasn't run the client
    /// yet, or hasn't installed it.
    NotConfigured,
    /// Adapter does not yet support auto-install in v0.2. `note`
    /// carries the unsupported-message string for display + JSON.
    Unsupported(String),
    /// Adapter errored while checking. `note` carries the formatted
    /// anyhow chain.
    Error(String),
}

impl ListStatus {
    fn slug(&self) -> &'static str {
        match self {
            ListStatus::Installed => "installed",
            ListStatus::ConfigPresent => "available",
            ListStatus::NotConfigured => "not_configured",
            ListStatus::Unsupported(_) => "unsupported",
            ListStatus::Error(_) => "error",
        }
    }
    fn note(&self) -> Option<&str> {
        match self {
            ListStatus::Unsupported(m) | ListStatus::Error(m) => Some(m.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ListRow {
    slug: &'static str,
    display_name: &'static str,
    config_path: PathBuf,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

fn emit_install(
    id: ClientId,
    entry: &McpServerEntry,
    receipt: &InstallReceipt,
    as_json: bool,
) -> Result<()> {
    if as_json {
        let body = serde_json::json!({
            "ok": true,
            "client": id.slug(),
            "config_path": receipt.config_path.display().to_string(),
            "backup_path": receipt.backup_path.display().to_string(),
            "entry": {
                "command": entry.command,
                "args": entry.args,
                "env": entry.env,
            },
        });
        println!("{body}");
    } else {
        println!(
            "ok client={} config={}",
            id.slug(),
            receipt.config_path.display()
        );
        println!("   backup={}", receipt.backup_path.display());
        println!("   command={} args={:?}", entry.command, entry.args);
        if let Some(url) = entry.env.get("LOCALMEM_CORE_URL") {
            println!("   LOCALMEM_CORE_URL={url}");
        }
        println!(
            "Restart {} to load the new MCP server entry.",
            id.display_name()
        );
    }
    Ok(())
}

fn emit_uninstall(
    id: ClientId,
    removed: bool,
    config_path: &std::path::Path,
    as_json: bool,
) -> Result<()> {
    if as_json {
        let body = serde_json::json!({
            "ok": true,
            "client": id.slug(),
            "config_path": config_path.display().to_string(),
            "removed": removed,
        });
        println!("{body}");
    } else if removed {
        println!(
            "ok removed localmem from {} ({})",
            id.display_name(),
            config_path.display()
        );
    } else {
        println!(
            "no-op localmem not registered in {} ({})",
            id.display_name(),
            config_path.display()
        );
    }
    Ok(())
}

fn emit_list(rows: &[ListRow], as_json: bool) -> Result<()> {
    if as_json {
        let body = serde_json::json!({
            "ok": true,
            "clients": rows,
        });
        println!("{body}");
        return Ok(());
    }
    // Human-readable: fixed-width status column makes the install
    // state obvious at a glance.
    println!("client         status           config_path");
    for row in rows {
        println!(
            "{:<14} {:<16} {}",
            row.slug,
            row.status,
            row.config_path.display()
        );
        if let Some(note) = &row.note {
            println!("    note: {note}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn faked_env() -> impl Drop {
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                std::env::remove_var("LOCALMEM_MCP_SERVER_CMD");
            }
        }
        std::env::set_var(
            "LOCALMEM_MCP_SERVER_CMD",
            "/usr/local/bin/bun;/srv/mcp-server/src/index.ts",
        );
        Guard
    }

    #[test]
    fn install_then_uninstall_round_trip_against_tempdir() {
        let tmp = tempdir().unwrap();
        let _g = faked_env();
        run_install(tmp.path().to_str(), "claude", "127.0.0.1:7788", true).unwrap();
        run_uninstall(tmp.path().to_str(), "claude", true).unwrap();
    }

    #[test]
    fn install_unknown_client_errors_clearly() {
        let err = run_install(None, "not-a-client", "127.0.0.1:7788", true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown MCP client"),
            "expected unknown-client error, got: {msg}"
        );
    }

    #[test]
    fn install_codex_returns_unsupported_msg() {
        let tmp = tempdir().unwrap();
        let _g = faked_env();
        let err = run_install(tmp.path().to_str(), "codex", "127.0.0.1:7788", true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("codex") && msg.contains("TOML"),
            "expected unsupported-codex message, got: {msg}"
        );
    }

    #[test]
    fn list_includes_every_known_client() {
        // Faked env so we can construct an entry if anything calls
        // through to it (list itself doesn't, but a future regression
        // shouldn't accidentally invoke default_localmem_entry).
        let _g = faked_env();
        let tmp = tempdir().unwrap();
        // Just verify list runs without panicking on a clean home.
        run_list(tmp.path().to_str(), true).unwrap();
    }
}
