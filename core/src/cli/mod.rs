//! CLI subcommand handlers.
//!
//! Each clap subcommand in `main.rs` delegates to a handler here. Keeping
//! handlers out of `main.rs` lets us test them as ordinary library code.

use std::path::Path;
use std::time::Duration;

/// Resolve the server address the CLI should talk to (config → default).
fn server_addr(home: &Path) -> String {
    crate::config::Config::load(home)
        .map(|c| c.server.addr)
        .unwrap_or_else(|_| crate::config::DEFAULT_SERVER_ADDR.to_string())
}

/// Fast liveness probe so a read command falls back to its in-process path
/// quickly when no server is running.
fn server_up(addr: &str) -> bool {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(250))
        .build()
        .get(&format!("http://{addr}/health"))
        .call()
        .map(|r| r.status() == 200)
        .unwrap_or(false)
}

/// Open the DuckDB facts store for an in-process CLI path, turning DuckDB's raw
/// "Conflicting lock" error into an actionable message when the always-on
/// service is the process holding the lock. Commands that have a server route
/// should prefer `server_post`/`server_get`; this is for the genuinely
/// in-process paths and the server-down fallback, so a user never sees the bare
/// lock error with no idea what to do.
pub(crate) fn open_facts(home: &Path) -> anyhow::Result<crate::facts::FactsStore> {
    crate::facts::FactsStore::open(home).map_err(|e| {
        let msg = format!("{e:#}");
        if msg.contains("Conflicting lock") || msg.contains("set lock on file") {
            anyhow::anyhow!(
                "localmem's background service is running and holds the memory database \
                 (DuckDB allows one writer at a time).\n  \
                 Reach your memory through the running service instead: open the dashboard at \
                 http://127.0.0.1:7788, ask your AI client (MCP), or run `localmem search --mode lex`.\n  \
                 To run this command directly, stop the service first with `localmem service uninstall`."
            )
        } else {
            e
        }
    })
}

/// POST a read endpoint on the running server, returning its JSON, or `None`
/// when no server is up (the caller then reads in-process).
///
/// Read commands whose store is the DuckDB facts file MUST route here while a
/// server is running: DuckDB allows a single read-write attachment OR multiple
/// read-only ones, never both — so an in-process read fails while `serve` holds
/// the write lock. Routing to the server is the correct CLI/server peer pattern,
/// not a lock-dodge (the lock is fundamental to DuckDB, not an unneeded one).
pub(crate) fn server_post(
    home: &Path,
    path: &str,
    body: serde_json::Value,
) -> Option<serde_json::Value> {
    let addr = server_addr(home);
    if !server_up(&addr) {
        return None;
    }
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build()
        .post(&format!("http://{addr}{path}"))
        .send_json(body)
        .ok()?
        .into_json()
        .ok()
}

/// GET variant of [`server_post`].
pub(crate) fn server_get(home: &Path, path: &str) -> Option<serde_json::Value> {
    let addr = server_addr(home);
    if !server_up(&addr) {
        return None;
    }
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build()
        .get(&format!("http://{addr}{path}"))
        .call()
        .ok()?
        .into_json()
        .ok()
}

pub mod audit;
pub mod brief;
pub mod doctor;
pub mod export;
pub mod fetch_model;
pub mod forget;
pub mod hooks;
pub mod import_wizard;
pub mod init;
pub mod journal;
pub mod mcp;
pub mod mcp_clients;
pub mod profile;
pub mod recall;
pub mod recent;
pub mod reindex;
pub mod replay;
pub mod search;
pub mod service;
pub mod setup;
pub mod status;
pub mod subjects;
pub mod summarize;
pub mod tag_arg;
pub mod tags;
pub mod todo;
pub mod understand;
pub mod write;
