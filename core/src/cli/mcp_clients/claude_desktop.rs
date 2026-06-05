//! Claude Desktop adapter.
//!
//! Config path on macOS:
//!   `~/Library/Application Support/Claude/claude_desktop_config.json`
//!
//! Linux is not officially supported by Claude Desktop, but the
//! Anthropic docs list `~/.config/Claude/claude_desktop_config.json`
//! for unofficial builds. We pick the platform-appropriate path at
//! runtime and refuse to guess on other OSes.
//!
//! Shape: `{ "mcpServers": { "<name>": { "command", "args", "env" } } }`.

use super::{install_jsonshape_mcp_servers, uninstall_jsonshape_mcp_servers};
use super::{ClientId, InstallReceipt, McpClient, McpServerEntry};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct ClaudeDesktop;

impl McpClient for ClaudeDesktop {
    fn id(&self) -> ClientId {
        ClientId::ClaudeDesktop
    }

    fn config_path(&self, home: &Path) -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            home.join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json")
        }
        #[cfg(target_os = "linux")]
        {
            home.join(".config")
                .join("Claude")
                .join("claude_desktop_config.json")
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            home.join(".claude_desktop_config.json")
        }
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

    fn sample_entry() -> McpServerEntry {
        McpServerEntry {
            name: "localmem".into(),
            command: "bun".into(),
            args: vec!["/srv/index.ts".into()],
            env: BTreeMap::from([("LOCALMEM_CORE_URL".into(), "http://127.0.0.1:7788".into())]),
        }
    }

    #[test]
    fn install_then_list_then_uninstall_round_trips() {
        let tmp = tempdir().unwrap();
        let client = ClaudeDesktop;
        assert!(!client.is_installed(tmp.path()).unwrap());
        client.install(tmp.path(), &sample_entry()).unwrap();
        assert!(client.is_installed(tmp.path()).unwrap());
        let removed = client.uninstall(tmp.path()).unwrap();
        assert!(removed);
        assert!(!client.is_installed(tmp.path()).unwrap());
    }

    #[test]
    fn config_path_on_macos_uses_library_application_support() {
        let p = ClaudeDesktop.config_path(Path::new("/Users/foo"));
        // Test only meaningful on macOS hosts; on others the path
        // differs and the assertion below would be wrong. Skip
        // cleanly so we don't regress cross-platform CI.
        if cfg!(target_os = "macos") {
            assert_eq!(
                p,
                PathBuf::from(
                    "/Users/foo/Library/Application Support/Claude/claude_desktop_config.json"
                ),
            );
        } else {
            assert!(p.ends_with("claude_desktop_config.json"));
        }
    }
}
