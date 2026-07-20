//! MCP client adapters for `localmem mcp install <client>` (T-50).
//!
//! Each adapter knows how to read, mutate, and write a single AI
//! tool's MCP configuration file. The common `mcpServers.{name}`
//! JSON shape (Anthropic's convention, adopted by Cursor, Cline,
//! Windsurf) is handled by a shared base; tools that use a different
//! format (Codex's TOML; Aider has no MCP support yet) get individual
//! impls or stubs.
//!
//! Why a trait per client (instead of a giant switch): every config
//! file has its own path, optional defaults to preserve, and edge
//! cases (Cline's deeply-nested VS Code globalStorage path; Claude
//! Code's per-project `~/.claude.json` vs global config). Isolating
//! each in its own module keeps the dispatch logic in `cli/mcp.rs`
//! small and review-friendly.
//!
//! Atomicity discipline: every config mutation goes through
//! `write_config_atomic` (write to `.tmp`, fsync, rename). A crash
//! mid-write leaves the original config intact. Every install also
//! drops a `.bak` next to the live config before the first mutation
//! so the user can recover from a misconfiguration without our help.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub mod claude_code;
pub mod claude_desktop;
pub mod cline;
pub mod codex;
pub mod cursor;
pub mod windsurf;

/// Identifies a supported MCP client. Slugs match the CLI subcommand
/// argument the user types, e.g. `localmem mcp install claude`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientId {
    ClaudeDesktop,
    ClaudeCode,
    Cursor,
    Windsurf,
    Cline,
    /// Stub: TOML config, no install support yet. See
    /// [`unsupported_msg`].
    Codex,
    /// Stub: aider does not yet ship MCP support in its core CLI.
    Aider,
}

impl ClientId {
    pub fn slug(self) -> &'static str {
        match self {
            ClientId::ClaudeDesktop => "claude",
            ClientId::ClaudeCode => "claude-code",
            ClientId::Cursor => "cursor",
            ClientId::Windsurf => "windsurf",
            ClientId::Cline => "cline",
            ClientId::Codex => "codex",
            ClientId::Aider => "aider",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ClientId::ClaudeDesktop => "Claude Desktop",
            ClientId::ClaudeCode => "Claude Code",
            ClientId::Cursor => "Cursor",
            ClientId::Windsurf => "Windsurf",
            ClientId::Cline => "Cline",
            ClientId::Codex => "Codex",
            ClientId::Aider => "Aider",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "claude" | "claude-desktop" => Some(ClientId::ClaudeDesktop),
            "claude-code" | "claudecode" => Some(ClientId::ClaudeCode),
            "cursor" => Some(ClientId::Cursor),
            "windsurf" => Some(ClientId::Windsurf),
            "cline" => Some(ClientId::Cline),
            "codex" => Some(ClientId::Codex),
            "aider" => Some(ClientId::Aider),
            _ => None,
        }
    }

    pub fn all() -> &'static [ClientId] {
        &[
            ClientId::ClaudeDesktop,
            ClientId::ClaudeCode,
            ClientId::Cursor,
            ClientId::Windsurf,
            ClientId::Cline,
            ClientId::Codex,
            ClientId::Aider,
        ]
    }
}

/// One MCP server entry to add to a client's config. Currently
/// produces the Anthropic-style `mcpServers.{name}` shape every JSON
/// adapter shares. Codex renders the same entry into its TOML shape.
#[derive(Debug, Clone)]
pub struct McpServerEntry {
    /// MCP server name key (always `"localmem"` for now).
    pub name: String,
    /// Executable to spawn (`bun`, `node`, or an absolute binary path).
    pub command: String,
    /// Arguments passed to `command`.
    pub args: Vec<String>,
    /// Environment variables set on the spawned process.
    pub env: BTreeMap<String, String>,
}

/// The MCP-server entry localmem installs into every client. Pulls
/// `bun` and the mcp-server source path from environment + sensible
/// fallbacks so a fresh install can succeed without flags.
///
/// Resolution order for the entry's `command` + `args`:
/// 1. `LOCALMEM_MCP_SERVER_CMD` env var (semicolon-separated
///    `<command>;<arg1>;<arg2>...`) — escape hatch for tests and
///    bespoke setups.
/// 2. `LOCALMEM_MCP_SERVER_PATH` env var pointing at the
///    `mcp-server/src/index.ts` source file → `bun <path>`.
/// 3. A sibling-of-binary check at `<binary-dir>/../mcp-server/src/index.ts`
///    (the standard layout for a source checkout) → `bun <path>`.
///
/// Returns a clear error when none resolve so the user knows what to
/// fix instead of writing a broken config.
pub fn default_localmem_entry(core_addr: &str) -> Result<McpServerEntry> {
    if let Ok(raw) = std::env::var("LOCALMEM_MCP_SERVER_CMD") {
        let parts: Vec<String> = raw
            .split(';')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if let Some((cmd, rest)) = parts.split_first() {
            return Ok(McpServerEntry {
                name: "localmem".into(),
                command: cmd.clone(),
                args: rest.to_vec(),
                env: localmem_env(core_addr),
            });
        }
        bail!("LOCALMEM_MCP_SERVER_CMD set but empty; expected <command>;<arg>;<arg>...");
    }

    let bun = which("bun").context(
        "could not find `bun` on PATH; install Bun (https://bun.sh) or set \
         LOCALMEM_MCP_SERVER_CMD to override",
    )?;

    let server_src = resolve_mcp_server_path()?;
    Ok(McpServerEntry {
        name: "localmem".into(),
        command: bun.display().to_string(),
        args: vec![server_src.display().to_string()],
        env: localmem_env(core_addr),
    })
}

fn localmem_env(core_addr: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("LOCALMEM_CORE_URL".into(), format!("http://{core_addr}"));
    env
}

/// `which`-style lookup against PATH, with a tweak: we resolve every
/// component to an absolute path so the resulting MCP entry is
/// invariant to the user's shell PATH at spawn time. Returns the
/// first executable found.
pub fn which(cmd: &str) -> Result<PathBuf> {
    let path = std::env::var("PATH").context("PATH not set")?;
    for component in std::env::split_paths(&path) {
        let candidate = component.join(cmd);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("{cmd} not found on PATH")
}

fn resolve_mcp_server_path() -> Result<PathBuf> {
    if let Ok(raw) = std::env::var("LOCALMEM_MCP_SERVER_PATH") {
        let p = PathBuf::from(raw);
        if !p.exists() {
            bail!(
                "LOCALMEM_MCP_SERVER_PATH points at {} which does not exist",
                p.display()
            );
        }
        return Ok(p);
    }

    // Sibling-of-binary check: in a source checkout, the binary lives
    // at <repo>/core/target/{debug,release}/localmem and the MCP
    // server source at <repo>/mcp-server/src/index.ts. We walk up
    // looking for a `mcp-server/src/index.ts` sibling so devs running
    // an in-tree build don't need to set the env var.
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent();
        while let Some(d) = dir {
            let candidate = d.join("mcp-server").join("src").join("index.ts");
            if candidate.is_file() {
                return Ok(candidate);
            }
            dir = d.parent();
        }
    }

    bail!(
        "could not auto-resolve MCP server path; set LOCALMEM_MCP_SERVER_PATH=/path/to/mcp-server/src/index.ts \
         (or LOCALMEM_MCP_SERVER_CMD to override the full command)"
    );
}

/// Trait every MCP client adapter implements. Each method takes the
/// home dir override so tests can point at a tempdir without touching
/// the real `~/.cursor` etc.
pub trait McpClient {
    fn id(&self) -> ClientId;

    /// Path the client reads its MCP config from. The `home` param is
    /// the user's home dir (`$HOME`); pass a tempdir in tests.
    fn config_path(&self, home: &Path) -> PathBuf;

    /// Whether localmem is currently registered in this client.
    /// Default impl: read the config, look for an `mcpServers.localmem`
    /// (or equivalent shape-specific key) entry.
    fn is_installed(&self, home: &Path) -> Result<bool> {
        let path = self.config_path(home);
        if !path.exists() {
            return Ok(false);
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("read {} for install check", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(false);
        }
        let v: Value = serde_json::from_str(&raw)
            .with_context(|| format!("parse {} as JSON", path.display()))?;
        Ok(v.pointer("/mcpServers/localmem").is_some())
    }

    /// Install localmem into this client's config. Idempotent:
    /// re-running replaces any existing `localmem` entry. Returns
    /// the path written.
    fn install(&self, home: &Path, entry: &McpServerEntry) -> Result<InstallReceipt>;

    /// Remove the localmem entry. Returns `true` when something was
    /// removed, `false` when no entry existed. Leaves other servers
    /// alone.
    fn uninstall(&self, home: &Path) -> Result<bool>;

    /// Optional error message for clients that don't yet support
    /// auto-install (currently Aider). When this returns `Some`, the
    /// dispatcher prints it and skips touching any files.
    fn unsupported_msg(&self) -> Option<&'static str> {
        None
    }
}

/// Audit data returned from a successful install: the file we wrote
/// and the path of the pre-install backup.
#[derive(Debug, Clone, PartialEq)]
pub struct InstallReceipt {
    pub config_path: PathBuf,
    pub backup_path: PathBuf,
}

/// Shared install routine for clients that use the
/// `mcpServers.{name}` JSON shape. Each adapter delegates here after
/// resolving its own path.
pub(crate) fn install_jsonshape_mcp_servers(
    config_path: &Path,
    entry: &McpServerEntry,
) -> Result<InstallReceipt> {
    // Ensure the parent directory exists. Clients like Claude Desktop
    // happily create the dir on first launch but we run before that,
    // so we own the mkdir.
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create config directory at {}", parent.display()))?;
    }

    // Backup the existing config before mutating. The .bak path is
    // the live path with `.localmem.bak` appended (so it's grouped
    // next to the original in `ls`). Backups overwrite previous
    // backups for the same client by design: every install captures
    // the pre-install state, not a deeper history.
    let backup_path = backup_path_for(config_path);
    if config_path.exists() {
        fs::copy(config_path, &backup_path).with_context(|| {
            format!(
                "back up {} to {} before mutating",
                config_path.display(),
                backup_path.display(),
            )
        })?;
    }

    let mut root = read_or_default_object(config_path)?;
    {
        let obj = root
            .as_object_mut()
            .context("top-level MCP config must be a JSON object")?;
        let servers = obj
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Default::default()));
        let servers_obj = servers
            .as_object_mut()
            .context("`mcpServers` must be a JSON object")?;
        servers_obj.insert(entry.name.clone(), render_entry_value(entry));
    }

    write_config_atomic(config_path, &root)?;
    Ok(InstallReceipt {
        config_path: config_path.to_path_buf(),
        backup_path,
    })
}

/// Shared uninstall routine for the same JSON shape.
pub(crate) fn uninstall_jsonshape_mcp_servers(
    config_path: &Path,
    server_name: &str,
) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }
    let mut root = read_or_default_object(config_path)?;
    let removed = {
        let Some(obj) = root.as_object_mut() else {
            return Ok(false);
        };
        let Some(servers) = obj.get_mut("mcpServers") else {
            return Ok(false);
        };
        let Some(servers_obj) = servers.as_object_mut() else {
            return Ok(false);
        };
        servers_obj.remove(server_name).is_some()
    };
    if removed {
        write_config_atomic(config_path, &root)?;
    }
    Ok(removed)
}

fn render_entry_value(entry: &McpServerEntry) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("command".into(), Value::String(entry.command.clone()));
    if !entry.args.is_empty() {
        map.insert(
            "args".into(),
            Value::Array(
                entry
                    .args
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !entry.env.is_empty() {
        let mut env_map = serde_json::Map::new();
        for (k, v) in &entry.env {
            env_map.insert(k.clone(), Value::String(v.clone()));
        }
        map.insert("env".into(), Value::Object(env_map));
    }
    Value::Object(map)
}

/// Read a JSON file as an object, returning an empty object when the
/// file doesn't exist or is empty. Errors when the file is present
/// but malformed: we'd rather refuse to mutate an unparseable config
/// than wipe the user's other servers.
fn read_or_default_object(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Default::default()));
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read existing config at {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let parsed: Value = serde_json::from_str(&raw).with_context(|| {
        format!(
            "parse existing config at {} as JSON (refusing to clobber a malformed file)",
            path.display()
        )
    })?;
    Ok(parsed)
}

/// Write a JSON value atomically: serialize to `<path>.tmp`, fsync,
/// rename. Inherits the rename's atomicity guarantee on the same
/// filesystem so a crash mid-write leaves either the old file or the
/// new file in place — never a half-written one.
fn write_config_atomic(path: &Path, value: &Value) -> Result<()> {
    let serialized = serde_json::to_string_pretty(value).context("serialize MCP config")?;
    write_config_text_atomic(path, &serialized, "mcp_config.json")
}

pub(crate) fn write_config_text_atomic(
    path: &Path,
    serialized: &str,
    fallback_name: &str,
) -> Result<()> {
    let mut tmp = path.to_path_buf();
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(fallback_name);
    tmp.set_file_name(format!("{file_name}.localmem.tmp"));

    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("create temp config at {}", tmp.display()))?;
        f.write_all(serialized.as_bytes())
            .with_context(|| format!("write temp config at {}", tmp.display()))?;
        if !serialized.ends_with('\n') {
            f.write_all(b"\n").context("write trailing newline")?;
        }
        f.sync_data().context("fsync temp config")?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename temp config {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

pub(crate) fn backup_path_for(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("mcp_config");
    p.set_file_name(format!("{file_name}.localmem.bak"));
    p
}

/// Dispatcher: turn a `ClientId` into a boxed adapter. Centralised so
/// new clients land in one place.
pub fn adapter(id: ClientId) -> Box<dyn McpClient> {
    match id {
        ClientId::ClaudeDesktop => Box::new(claude_desktop::ClaudeDesktop),
        ClientId::ClaudeCode => Box::new(claude_code::ClaudeCode),
        ClientId::Cursor => Box::new(cursor::Cursor),
        ClientId::Windsurf => Box::new(windsurf::Windsurf),
        ClientId::Cline => Box::new(cline::Cline),
        ClientId::Codex => Box::new(codex::Codex),
        ClientId::Aider => Box::new(AiderStub),
    }
}

/// Stub adapter for Aider — its core CLI does not yet ship MCP
/// support as of v0.2 design lock. Tracked for a future minor.
pub struct AiderStub;

impl McpClient for AiderStub {
    fn id(&self) -> ClientId {
        ClientId::Aider
    }
    fn config_path(&self, home: &Path) -> PathBuf {
        // Notional path so `mcp list` has something to show; never
        // mutated.
        home.join(".aider.conf.yml")
    }
    fn install(&self, _home: &Path, _entry: &McpServerEntry) -> Result<InstallReceipt> {
        bail!("{}", self.unsupported_msg().unwrap())
    }
    fn uninstall(&self, _home: &Path) -> Result<bool> {
        bail!("{}", self.unsupported_msg().unwrap())
    }
    fn unsupported_msg(&self) -> Option<&'static str> {
        Some(
            "aider does not ship MCP client support in its core CLI as of v0.2; \
             tracked for a future release.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn from_slug_round_trips_for_every_client() {
        for id in ClientId::all() {
            let back = ClientId::from_slug(id.slug()).expect("slug round-trips");
            assert_eq!(back, *id);
        }
    }

    #[test]
    fn from_slug_accepts_common_aliases() {
        assert_eq!(
            ClientId::from_slug("claude-desktop"),
            Some(ClientId::ClaudeDesktop)
        );
        assert_eq!(
            ClientId::from_slug("claudecode"),
            Some(ClientId::ClaudeCode)
        );
        assert!(ClientId::from_slug("not-a-client").is_none());
    }

    #[test]
    fn jsonshape_install_creates_dir_and_atomic_file() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nested").join("config.json");
        let entry = McpServerEntry {
            name: "localmem".into(),
            command: "/usr/local/bin/bun".into(),
            args: vec!["/srv/mcp-server/src/index.ts".into()],
            env: BTreeMap::from([("LOCALMEM_CORE_URL".into(), "http://127.0.0.1:7788".into())]),
        };
        let receipt = install_jsonshape_mcp_servers(&path, &entry).unwrap();
        assert_eq!(receipt.config_path, path);
        assert!(path.exists());
        let raw = fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v.pointer("/mcpServers/localmem/command")
                .and_then(Value::as_str),
            Some("/usr/local/bin/bun"),
        );
        assert_eq!(
            v.pointer("/mcpServers/localmem/args/0")
                .and_then(Value::as_str),
            Some("/srv/mcp-server/src/index.ts"),
        );
        assert_eq!(
            v.pointer("/mcpServers/localmem/env/LOCALMEM_CORE_URL")
                .and_then(Value::as_str),
            Some("http://127.0.0.1:7788"),
        );
    }

    #[test]
    fn jsonshape_install_preserves_existing_servers() {
        // The fixture has a peer MCP server; install must leave it
        // untouched. This is the highest-risk regression for the
        // viral install — wiping a user's existing config would be
        // unforgivable.
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(
            &path,
            r#"{ "mcpServers": { "other-tool": { "command": "/bin/echo" } } }"#,
        )
        .unwrap();
        let entry = McpServerEntry {
            name: "localmem".into(),
            command: "bun".into(),
            args: vec!["x.ts".into()],
            env: BTreeMap::new(),
        };
        install_jsonshape_mcp_servers(&path, &entry).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v.pointer("/mcpServers/other-tool/command")
                .and_then(Value::as_str),
            Some("/bin/echo"),
        );
        assert_eq!(
            v.pointer("/mcpServers/localmem/command")
                .and_then(Value::as_str),
            Some("bun"),
        );
    }

    #[test]
    fn jsonshape_install_is_idempotent() {
        // Re-running install replaces (not duplicates) the localmem
        // entry. The user can re-run after upgrading bun without
        // accidentally accumulating stale paths.
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("config.json");
        let mut entry = McpServerEntry {
            name: "localmem".into(),
            command: "/old/bun".into(),
            args: vec!["/old/index.ts".into()],
            env: BTreeMap::new(),
        };
        install_jsonshape_mcp_servers(&path, &entry).unwrap();
        entry.command = "/new/bun".into();
        install_jsonshape_mcp_servers(&path, &entry).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            v.pointer("/mcpServers/localmem/command")
                .and_then(Value::as_str),
            Some("/new/bun"),
        );
    }

    #[test]
    fn jsonshape_install_creates_backup_when_config_exists() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(&path, r#"{ "mcpServers": {} }"#).unwrap();
        let entry = McpServerEntry {
            name: "localmem".into(),
            command: "bun".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let receipt = install_jsonshape_mcp_servers(&path, &entry).unwrap();
        assert!(receipt.backup_path.exists(), "backup should be written");
        // Backup carries the pre-install content (empty mcpServers),
        // not the post-install content.
        let backup_raw = fs::read_to_string(&receipt.backup_path).unwrap();
        assert!(backup_raw.contains("\"mcpServers\""));
        assert!(!backup_raw.contains("localmem"));
    }

    #[test]
    fn jsonshape_install_refuses_to_clobber_malformed_config() {
        // A user with a broken config file should get a clear error,
        // not have their broken file silently overwritten. Backups
        // wouldn't save them here because they already lost data
        // before we ran.
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(&path, "not valid json {{{").unwrap();
        let entry = McpServerEntry {
            name: "localmem".into(),
            command: "bun".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let err = install_jsonshape_mcp_servers(&path, &entry).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parse existing config") || msg.contains("refusing"),
            "expected clear parse error, got: {msg}"
        );
    }

    #[test]
    fn jsonshape_uninstall_removes_only_localmem() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(
            &path,
            r#"{ "mcpServers": { "localmem": { "command": "bun" }, "other": { "command": "echo" } } }"#,
        )
        .unwrap();
        let removed = uninstall_jsonshape_mcp_servers(&path, "localmem").unwrap();
        assert!(removed);
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v.pointer("/mcpServers/localmem").is_none());
        assert!(v.pointer("/mcpServers/other").is_some());
    }

    #[test]
    fn jsonshape_uninstall_returns_false_when_absent() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(
            &path,
            r#"{ "mcpServers": { "other": { "command": "echo" } } }"#,
        )
        .unwrap();
        let removed = uninstall_jsonshape_mcp_servers(&path, "localmem").unwrap();
        assert!(!removed);
    }

    #[test]
    fn jsonshape_uninstall_on_missing_file_returns_false() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("does_not_exist.json");
        let removed = uninstall_jsonshape_mcp_servers(&path, "localmem").unwrap();
        assert!(!removed);
    }

    #[test]
    fn default_localmem_entry_uses_env_override() {
        std::env::set_var("LOCALMEM_MCP_SERVER_CMD", "/usr/bin/node; /opt/server.js");
        let entry = default_localmem_entry("127.0.0.1:7788").unwrap();
        std::env::remove_var("LOCALMEM_MCP_SERVER_CMD");
        assert_eq!(entry.command, "/usr/bin/node");
        assert_eq!(entry.args, vec!["/opt/server.js".to_string()]);
        assert_eq!(
            entry.env.get("LOCALMEM_CORE_URL").map(String::as_str),
            Some("http://127.0.0.1:7788"),
        );
    }
}
