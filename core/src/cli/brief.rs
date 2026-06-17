//! `localmem brief` — render the Session Boot Briefing for a project (SPEC 7c).
//!
//! Routes to the running server's `/brief`. The synthesis lives in the server
//! because it owns the store handles + the resolved model, and routing there
//! avoids the DuckDB writer lock a CLI `FactsStore::open` would hit while
//! `serve` runs. The briefing is an LLM synthesis, so there is no offline path:
//! if the server is down (or understanding is off) we print clear guidance.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::Duration;

/// Generous budget: synthesis is one local LLM call (a few seconds, more on a
/// cold model load).
const BRIEF_TIMEOUT_SECS: u64 = 180;

pub async fn run(home: Option<&str>, project: Option<String>, as_json: bool) -> Result<()> {
    let home = resolve_home(home);
    let cfg = crate::config::Config::load(&home).context("load config")?;
    let addr = cfg.server.addr.clone();
    let base = format!("http://{addr}");
    let body = match &project {
        Some(p) => serde_json::json!({ "project": p }),
        None => serde_json::json!({}),
    };

    let (status, value) =
        tokio::task::spawn_blocking(move || -> Result<(u16, serde_json::Value)> {
            let agent = ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(BRIEF_TIMEOUT_SECS))
                .build();
            match agent.post(&format!("{base}/brief")).send_json(body) {
                Ok(r) => {
                    let status = r.status();
                    let v = r.into_json().context("parse /brief response")?;
                    Ok((status, v))
                }
                // The server returns a non-2xx (e.g. 400 when understanding is off)
                // as an Err carrying the response; surface its body, don't drop it.
                Err(ureq::Error::Status(code, r)) => {
                    let v = r.into_json().unwrap_or(serde_json::Value::Null);
                    Ok((code, v))
                }
                Err(e) => bail!(
                    "could not reach the localmem server at {addr} ({e}). \
                 Start it with `localmem serve`."
                ),
            }
        })
        .await
        .context("join brief request")??;

    if status != 200 {
        let msg = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("brief request failed");
        bail!("{msg}");
    }

    if as_json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    let md = value
        .get("briefing_md")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .trim();
    if md.is_empty() {
        println!(
            "No briefing yet — no understood memories in this scope. Capture some \
             work with understanding enabled (`localmem understand`) and try again."
        );
    } else {
        println!("{md}");
    }
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
