//! Claude Code (Anthropic CLI) adapter.
//!
//! Claude Code stores MCP server registrations at `~/.claude.json`
//! (single global file used by every project the user runs `claude`
//! in). Shape matches the Claude Desktop convention — same
//! `mcpServers` object at the root — so we delegate to the shared
//! JSON-shape install helpers.
//!
//! The CLI also supports per-project `.claude.json` overrides, but
//! v0.2 only wires the global file. Project-scoped install is a
//! follow-up.

use super::{install_jsonshape_mcp_servers, uninstall_jsonshape_mcp_servers};
use super::{ClientId, InstallReceipt, McpClient, McpServerEntry};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct ClaudeCode;

impl McpClient for ClaudeCode {
    fn id(&self) -> ClientId {
        ClientId::ClaudeCode
    }

    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".claude.json")
    }

    fn install(&self, home: &Path, entry: &McpServerEntry) -> Result<InstallReceipt> {
        install_jsonshape_mcp_servers(&self.config_path(home), entry)
    }

    fn uninstall(&self, home: &Path) -> Result<bool> {
        uninstall_jsonshape_mcp_servers(&self.config_path(home), "localmem")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn install_creates_dot_claude_json_at_home_root() {
        let tmp = tempdir().unwrap();
        let client = ClaudeCode;
        let entry = McpServerEntry {
            name: "localmem".into(),
            command: "bun".into(),
            args: vec!["x.ts".into()],
            env: BTreeMap::new(),
        };
        client.install(tmp.path(), &entry).unwrap();
        assert!(tmp.path().join(".claude.json").exists());
    }

    #[test]
    fn install_preserves_unrelated_top_level_fields() {
        // The real ~/.claude.json carries project history,
        // permissions, and statsig settings; mutating mcpServers must
        // leave them untouched.
        let tmp = tempdir().unwrap();
        let path = tmp.path().join(".claude.json");
        fs::write(
            &path,
            r#"{
              "projects": {"/Users/x/repo": {"hasShownPrompt": true}},
              "userId": "abc-123"
            }"#,
        )
        .unwrap();
        let client = ClaudeCode;
        let entry = McpServerEntry {
            name: "localmem".into(),
            command: "bun".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        client.install(tmp.path(), &entry).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v.pointer("/userId").and_then(|x| x.as_str()),
            Some("abc-123")
        );
        assert!(v.pointer("/projects").is_some());
        assert!(v.pointer("/mcpServers/localmem").is_some());
    }
}
