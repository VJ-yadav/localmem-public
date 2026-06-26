//! `localmem get` handler (read-path R1): expand one memory by event_id into its
//! FULL content + understanding (summary, intent, entities). Routes through the
//! running service, which holds the stores. This is the CLI face of the
//! `memory_get` MCP tool: the drill-down from a search hit (whose `sources`
//! carry the event_id) to the whole memory, the cure for "search returned a
//! title, not the content".

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

pub fn run(home: Option<&str>, event_id: &str, as_json: bool) -> Result<()> {
    let home_path = resolve_home(home)?;
    let body = serde_json::json!({ "event_id": event_id });
    let resp = crate::cli::server_post(&home_path, "/get", body).ok_or_else(|| {
        anyhow!("localmem service not reachable on its configured address; start it with `localmem serve`")
    })?;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }

    let found = resp.get("found").and_then(|v| v.as_bool()).unwrap_or(false);
    if !found {
        println!("no memory found for event_id {event_id}");
        return Ok(());
    }
    if let Some(c) = resp.get("content").and_then(|v| v.as_str()) {
        println!("{c}");
    }
    if let Some(u) = resp.get("understanding") {
        let summary = u.get("summary").and_then(|v| v.as_str()).unwrap_or("");
        let intent = u.get("intent").and_then(|v| v.as_str()).unwrap_or("");
        if !summary.is_empty() {
            println!("\nsummary:  {summary}");
        }
        if !intent.is_empty() {
            println!("intent:   {intent}");
        }
        if let Some(ents) = u.get("entities").and_then(|v| v.as_array()) {
            let names: Vec<String> = ents
                .iter()
                .filter_map(|e| {
                    let n = e.get("name")?.as_str()?;
                    let k = e.get("kind")?.as_str()?;
                    Some(format!("{n} ({k})"))
                })
                .collect();
            if !names.is_empty() {
                println!("entities: {}", names.join(", "));
            }
        }
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
