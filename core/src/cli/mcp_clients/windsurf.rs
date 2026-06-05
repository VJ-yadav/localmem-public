//! Windsurf (Codeium) adapter.
//!
//! Windsurf reads MCP server registrations from
//! `~/.codeium/windsurf/mcp_config.json`. Shape matches the
//! Anthropic `mcpServers` convention adopted across most MCP clients.
//! Codeium also ships a "Cascade" sub-product with its own config;
//! this adapter targets Windsurf-the-IDE.

use super::{install_jsonshape_mcp_servers, uninstall_jsonshape_mcp_servers};
use super::{ClientId, InstallReceipt, McpClient, McpServerEntry};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct Windsurf;

impl McpClient for Windsurf {
    fn id(&self) -> ClientId {
        ClientId::Windsurf
    }

    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".codeium")
            .join("windsurf")
            .join("mcp_config.json")
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
    fn install_creates_nested_codeium_windsurf_dir() {
        let tmp = tempdir().unwrap();
        let entry = McpServerEntry {
            name: "localmem".into(),
            command: "bun".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        Windsurf.install(tmp.path(), &entry).unwrap();
        assert!(tmp
            .path()
            .join(".codeium")
            .join("windsurf")
            .join("mcp_config.json")
            .exists());
    }
}
