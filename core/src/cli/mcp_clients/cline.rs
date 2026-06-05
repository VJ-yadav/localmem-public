//! Cline (VS Code extension) adapter.
//!
//! Cline is a VS Code extension (`saoudrizwan.claude-dev`); its MCP
//! config lives inside VS Code's `globalStorage` directory rather
//! than under the user's home root:
//!
//!   macOS:  `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json`
//!   Linux:  `~/.config/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json`
//!
//! Shape matches the Anthropic `mcpServers` convention.

use super::{install_jsonshape_mcp_servers, uninstall_jsonshape_mcp_servers};
use super::{ClientId, InstallReceipt, McpClient, McpServerEntry};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct Cline;

impl Cline {
    fn vscode_user_dir(home: &Path) -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            home.join("Library")
                .join("Application Support")
                .join("Code")
                .join("User")
        }
        #[cfg(target_os = "linux")]
        {
            home.join(".config").join("Code").join("User")
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            home.join(".vscode").join("User")
        }
    }
}

impl McpClient for Cline {
    fn id(&self) -> ClientId {
        ClientId::Cline
    }

    fn config_path(&self, home: &Path) -> PathBuf {
        Self::vscode_user_dir(home)
            .join("globalStorage")
            .join("saoudrizwan.claude-dev")
            .join("settings")
            .join("cline_mcp_settings.json")
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
    fn install_creates_the_full_globalstorage_chain() {
        let tmp = tempdir().unwrap();
        let entry = McpServerEntry {
            name: "localmem".into(),
            command: "bun".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        Cline.install(tmp.path(), &entry).unwrap();
        let path = Cline.config_path(tmp.path());
        assert!(path.exists(), "{} must exist after install", path.display());
        // The deeply-nested parent chain (globalStorage → ext id →
        // settings) is the riskiest part of this adapter; verify all
        // four nesting layers landed.
        assert!(path
            .to_string_lossy()
            .contains("saoudrizwan.claude-dev/settings"));
    }
}
