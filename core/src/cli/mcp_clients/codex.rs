//! Codex MCP client adapter.
//!
//! Codex stores MCP servers in `~/.codex/config.toml` under
//! `mcp_servers.<name>`. This adapter validates the full TOML document before
//! mutation, preserves unrelated settings, and uses the shared atomic writer.

use super::{
    backup_path_for, write_config_text_atomic, ClientId, InstallReceipt, McpClient, McpServerEntry,
};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use toml::{Table, Value};

pub struct Codex;

impl McpClient for Codex {
    fn id(&self) -> ClientId {
        ClientId::Codex
    }

    fn config_path(&self, home: &Path) -> PathBuf {
        home.join(".codex").join("config.toml")
    }

    fn is_installed(&self, home: &Path) -> Result<bool> {
        let path = self.config_path(home);
        if !path.exists() {
            return Ok(false);
        }
        let root = read_config(&path)?;
        Ok(root
            .get("mcp_servers")
            .and_then(Value::as_table)
            .and_then(|servers| servers.get("localmem"))
            .is_some())
    }

    fn install(&self, home: &Path, entry: &McpServerEntry) -> Result<InstallReceipt> {
        let config_path = self.config_path(home);
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create config directory at {}", parent.display()))?;
        }

        let backup_path = backup_path_for(&config_path);
        let mut root = match read_config(&config_path) {
            Ok(root) => root,
            Err(error) => {
                if config_path.exists() {
                    back_up(&config_path, &backup_path)?;
                }
                return Err(error);
            }
        };
        let rendered = render_entry(entry);
        let servers = root
            .entry("mcp_servers")
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .context("`mcp_servers` must be a TOML table")?;

        if servers.get(&entry.name) == Some(&rendered) {
            return Ok(InstallReceipt {
                config_path,
                backup_path,
            });
        }

        if config_path.exists() {
            back_up(&config_path, &backup_path)?;
        }
        servers.insert(entry.name.clone(), rendered);
        let serialized = toml::to_string_pretty(&root).context("serialize Codex MCP config")?;
        write_config_text_atomic(&config_path, &serialized, "config.toml")?;

        Ok(InstallReceipt {
            config_path,
            backup_path,
        })
    }

    fn uninstall(&self, home: &Path) -> Result<bool> {
        let config_path = self.config_path(home);
        if !config_path.exists() {
            return Ok(false);
        }
        let mut root = read_config(&config_path)?;
        let removed = root
            .get_mut("mcp_servers")
            .and_then(Value::as_table_mut)
            .map(|servers| servers.remove("localmem").is_some())
            .unwrap_or(false);
        if removed {
            let serialized = toml::to_string_pretty(&root).context("serialize Codex MCP config")?;
            write_config_text_atomic(&config_path, &serialized, "config.toml")?;
        }
        Ok(removed)
    }
}

fn back_up(config_path: &Path, backup_path: &Path) -> Result<()> {
    fs::copy(config_path, backup_path).with_context(|| {
        format!(
            "back up {} to {} before mutating",
            config_path.display(),
            backup_path.display()
        )
    })?;
    Ok(())
}

fn read_config(path: &Path) -> Result<Table> {
    if !path.exists() {
        return Ok(Table::new());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read existing config at {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Table::new());
    }
    toml::from_str(&raw).with_context(|| {
        format!(
            "parse existing config at {} as TOML (refusing to clobber a malformed file)",
            path.display()
        )
    })
}

fn render_entry(entry: &McpServerEntry) -> Value {
    let mut server = Table::new();
    server.insert("command".into(), Value::String(entry.command.clone()));
    if !entry.args.is_empty() {
        server.insert(
            "args".into(),
            Value::Array(entry.args.iter().cloned().map(Value::String).collect()),
        );
    }
    if !entry.env.is_empty() {
        let env = entry
            .env
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();
        server.insert("env".into(), Value::Table(env));
    }
    Value::Table(server)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn entry() -> McpServerEntry {
        McpServerEntry {
            name: "localmem".into(),
            command: "/usr/local/bin/bun".into(),
            args: vec!["/opt/localmem/mcp-server/src/index.ts".into()],
            env: BTreeMap::from([("LOCALMEM_CORE_URL".into(), "http://127.0.0.1:7788".into())]),
        }
    }

    #[test]
    fn installs_codex_entry_and_preserves_unrelated_settings() {
        let home = tempdir().unwrap();
        let config = home.path().join(".codex").join("config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "model = \"gpt-5\"\n").unwrap();

        Codex.install(home.path(), &entry()).unwrap();

        let parsed = read_config(&config).unwrap();
        assert_eq!(parsed["model"].as_str(), Some("gpt-5"));
        let localmem = parsed["mcp_servers"]["localmem"].as_table().unwrap();
        assert_eq!(localmem["command"].as_str(), Some("/usr/local/bin/bun"));
        assert_eq!(
            localmem["env"]["LOCALMEM_CORE_URL"].as_str(),
            Some("http://127.0.0.1:7788")
        );
        assert!(Codex.is_installed(home.path()).unwrap());
    }

    #[test]
    fn repeated_install_is_a_no_op() {
        let home = tempdir().unwrap();
        Codex.install(home.path(), &entry()).unwrap();
        let config = Codex.config_path(home.path());
        let first = fs::read(&config).unwrap();
        let backup = backup_path_for(&config);
        assert!(!backup.exists());

        Codex.install(home.path(), &entry()).unwrap();

        assert_eq!(fs::read(config).unwrap(), first);
        assert!(!backup.exists());
    }

    #[test]
    fn backs_up_existing_config_before_mutating() {
        let home = tempdir().unwrap();
        let config = Codex.config_path(home.path());
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = b"model = \"gpt-5\"\n";
        fs::write(&config, original).unwrap();

        let receipt = Codex.install(home.path(), &entry()).unwrap();

        assert_eq!(fs::read(receipt.backup_path).unwrap(), original);
    }

    #[test]
    fn malformed_config_is_backed_up_but_never_clobbered() {
        let home = tempdir().unwrap();
        let config = Codex.config_path(home.path());
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let malformed = b"[mcp_servers.localmem\ncommand = \"bun\"";
        fs::write(&config, malformed).unwrap();

        let error = Codex.install(home.path(), &entry()).unwrap_err();

        assert!(format!("{error:#}").contains("refusing to clobber"));
        assert_eq!(fs::read(&config).unwrap(), malformed);
        assert_eq!(fs::read(backup_path_for(&config)).unwrap(), malformed);
    }

    #[test]
    fn uninstall_removes_only_localmem() {
        let home = tempdir().unwrap();
        Codex.install(home.path(), &entry()).unwrap();
        assert!(Codex.uninstall(home.path()).unwrap());
        assert!(!Codex.is_installed(home.path()).unwrap());
    }
}
