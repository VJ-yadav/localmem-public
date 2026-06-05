//! Cursor adapter.
//!
//! Cursor reads MCP server registrations from `~/.cursor/mcp.json`
//! (global) or `<workspace>/.cursor/mcp.json` (workspace). v0.2
//! wires only the global file; per-workspace install is a follow-up.
//! Shape matches Anthropic's `mcpServers` convention.

use super::{install_jsonshape_mcp_servers, uninstall_jsonshape_mcp_servers};
use super::{ClientId, InstallReceipt, McpClient, McpServerEntry};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct Cursor;

impl McpClient for Cursor {
    fn id(&self) -> ClientId {
        ClientId::Cursor
    }

    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".cursor").join("mcp.json")
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
    use tempfile::tempdir;

    #[test]
    fn install_creates_dot_cursor_dir_when_missing() {
        let tmp = tempdir().unwrap();
        let client = Cursor;
        let entry = McpServerEntry {
            name: "localmem".into(),
            command: "bun".into(),
            args: vec!["x.ts".into()],
            env: BTreeMap::new(),
        };
        client.install(tmp.path(), &entry).unwrap();
        assert!(tmp.path().join(".cursor").join("mcp.json").exists());
    }
}
