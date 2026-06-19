//! Onboarding: ONE shared status source for every entry point (SPEC §8).
//!
//! Principle (Vijay, 2026-06-17): `localmem setup` is THE install, everywhere
//! (curl, npm, mcp install all funnel into it); any new onboarding step is added
//! to `setup`, never as a second command. Onboarding is a STATUS picture, not a
//! list of chores: localmem does the dependency work itself (fetch model, start
//! service, wire the client) and reports each as a CHECK (ok / not-ok). When a
//! check is not ok, it carries the manual fallback command so the user (or we)
//! can finish it. Genuine decisions (import history, understanding backend) are
//! surfaced separately, not as failures.
//!
//! This module is pure + state-aware: callers gather the facts (model present?
//! service running? which clients wired? understanding on? importable history?)
//! and hand them in; this produces the checks + renders. The same struct backs
//! the CLI render AND the dashboard status strip AND the MCP welcome resource.

use serde::Serialize;
use std::path::Path;

/// One onboarding check. `required` checks gate "ready"; optional ones (e.g.
/// understanding) are shown as status/toggles, never as failures.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Check {
    pub key: String,
    pub label: String,
    pub ok: bool,
    pub required: bool,
    pub detail: String,
    /// Manual fallback to satisfy this check when it is not ok (or to flip an
    /// optional toggle on). `None` when nothing to do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// A client localmem can wire its MCP into, with its one-line command, so the
/// "wire another client" surface lists every option (Ollama-style).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClientWire {
    pub slug: String,
    pub label: String,
    pub wired: bool,
    pub command: String,
}

/// The shared onboarding snapshot every entry point renders from.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GettingStarted {
    pub dashboard_url: String,
    pub checks: Vec<Check>,
    /// Every supported MCP client + its wire command, marked wired/not.
    pub clients: Vec<ClientWire>,
    /// True when all REQUIRED checks pass (model + service + at least one client).
    pub ready: bool,
    /// Importable AI histories detected (a user choice, not a failure).
    pub import_candidates: usize,
}

/// Build the snapshot from gathered facts. `clients` is `(slug, label, wired)`
/// for EVERY supported client (the caller pulls the list from the client
/// registry and marks which are wired).
pub fn build(
    dashboard_url: impl Into<String>,
    model_present: bool,
    service_running: bool,
    clients: &[(String, String, bool)],
    understanding_enabled: bool,
    import_candidates: usize,
) -> GettingStarted {
    let dashboard_url = dashboard_url.into();
    let any_wired = clients.iter().any(|(_, _, w)| *w);
    let wired_labels: Vec<&str> = clients
        .iter()
        .filter(|(_, _, w)| *w)
        .map(|(_, l, _)| l.as_str())
        .collect();

    let checks = vec![
        Check {
            key: "model".into(),
            label: "Embedder model".into(),
            ok: model_present,
            required: true,
            detail: if model_present {
                "bge-small ready".into()
            } else {
                "not fetched — semantic search degrades to keyword-only".into()
            },
            fix: (!model_present).then(|| "localmem fetch-model".to_string()),
        },
        Check {
            key: "service".into(),
            label: "Core service".into(),
            ok: service_running,
            required: true,
            detail: if service_running {
                "running".into()
            } else {
                "not running — the memory_* tools return 'core unreachable'".into()
            },
            fix: (!service_running)
                .then(|| "localmem service install   (or: localmem serve)".to_string()),
        },
        Check {
            key: "mcp".into(),
            label: "AI client wired".into(),
            ok: any_wired,
            required: true,
            detail: if any_wired {
                format!("wired: {}", wired_labels.join(", "))
            } else {
                "no AI client wired yet".into()
            },
            fix: (!any_wired).then(|| "localmem mcp install <client>".to_string()),
        },
        Check {
            key: "understanding".into(),
            label: "Understanding (summaries + graph)".into(),
            ok: understanding_enabled,
            required: false,
            detail: if understanding_enabled {
                "on".into()
            } else {
                "off — raw capture + search still work; no summaries/entities/graph".into()
            },
            fix: (!understanding_enabled).then(|| "localmem understand".to_string()),
        },
    ];

    let ready = checks.iter().filter(|c| c.required).all(|c| c.ok);
    let clients = clients
        .iter()
        .map(|(slug, label, wired)| ClientWire {
            slug: slug.clone(),
            label: label.clone(),
            wired: *wired,
            command: format!("localmem mcp install {slug}"),
        })
        .collect();

    GettingStarted {
        dashboard_url,
        checks,
        clients,
        ready,
        import_candidates,
    }
}

/// Resolve the human dashboard URL from a `[server].addr` value.
pub fn dashboard_url(addr: &str) -> String {
    let a = addr.trim();
    if a.starts_with("http://") || a.starts_with("https://") {
        a.to_string()
    } else {
        format!("http://{a}")
    }
}

/// True when the BGE embedder model directory exists under `home`.
pub fn model_present(home: &Path) -> bool {
    home.join("models").join("bge-small-en-v1.5").is_dir()
}

/// True when the core HTTP server answers on `addr` (a quick localhost probe),
/// so the "Core service" check reflects reality whether it was started by the
/// always-on service OR a bare `localmem serve`. `addr` may be a bare
/// `host:port` or an `http(s)://` URL.
pub fn core_reachable(addr: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let target = addr
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    target
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .map(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(300)).is_ok())
        .unwrap_or(false)
}

impl GettingStarted {
    fn icon(c: &Check) -> &'static str {
        if c.ok {
            "✓"
        } else if c.required {
            "✗"
        } else {
            "○"
        }
    }

    /// Terminal render for the CLI installers (`setup`, `mcp install`).
    pub fn render_terminal(&self) -> String {
        let mut out = String::new();
        out.push_str(if self.ready {
            "localmem is set up. Status:\n"
        } else {
            "localmem setup — almost there. Status:\n"
        });
        for c in &self.checks {
            out.push_str(&format!("  {} {} — {}\n", Self::icon(c), c.label, c.detail));
        }
        // Manual fallbacks for anything not yet satisfied.
        let todos: Vec<&Check> = self.checks.iter().filter(|c| !c.ok).collect();
        if !todos.is_empty() {
            out.push('\n');
            out.push_str("To finish (or we can do it for you):\n");
            for c in todos {
                if let Some(fix) = &c.fix {
                    out.push_str(&format!("  {}: {}\n", c.label, fix.replace('`', "")));
                }
            }
        }
        if self.import_candidates > 0 {
            out.push_str(&format!(
                "\nBring your history: {} importable export(s) detected. Run: localmem import-wizard --apply\n",
                self.import_candidates
            ));
        }
        out.push_str(&format!("\nDashboard: {}\n", self.dashboard_url));
        // Additive: wire another client (Ollama-style full list).
        let unwired: Vec<&ClientWire> = self.clients.iter().filter(|c| !c.wired).collect();
        if !unwired.is_empty() {
            out.push_str("\nWire another AI client:\n");
            for c in unwired {
                out.push_str(&format!("  {:<14} {}\n", c.label, c.command));
            }
        }
        out
    }

    /// Agent-facing Markdown for the MCP `localmem://getting-started` resource.
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# localmem is set up — your memory across every AI tool\n\n");
        out.push_str(&format!("**Dashboard:** {}\n\n", self.dashboard_url));
        out.push_str("**Status:**\n");
        for c in &self.checks {
            out.push_str(&format!("- {} {} — {}\n", Self::icon(c), c.label, c.detail));
        }
        let todos: Vec<&Check> = self
            .checks
            .iter()
            .filter(|c| !c.ok && c.fix.is_some())
            .collect();
        if !todos.is_empty() {
            out.push_str("\n**To finish:**\n");
            for c in todos {
                out.push_str(&format!(
                    "- {}: `{}`\n",
                    c.label,
                    c.fix.as_deref().unwrap_or("")
                ));
            }
        }
        if self.import_candidates > 0 {
            out.push_str(&format!(
                "\n**Bring your history:** {} importable export(s) detected — `localmem import-wizard --apply`.\n",
                self.import_candidates
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clients(wired: &[&str]) -> Vec<(String, String, bool)> {
        [
            ("claude-code", "Claude Code"),
            ("cursor", "Cursor"),
            ("windsurf", "Windsurf"),
        ]
        .iter()
        .map(|(s, l)| (s.to_string(), l.to_string(), wired.contains(s)))
        .collect()
    }

    #[test]
    fn dashboard_url_adds_scheme_when_missing() {
        assert_eq!(dashboard_url("127.0.0.1:7788"), "http://127.0.0.1:7788");
        assert_eq!(dashboard_url("https://x.dev"), "https://x.dev");
    }

    #[test]
    fn ready_requires_model_service_and_a_client() {
        let full = build("http://h", true, true, &clients(&["cursor"]), true, 0);
        assert!(full.ready);
        let no_model = build("http://h", false, true, &clients(&["cursor"]), true, 0);
        assert!(!no_model.ready);
        let no_client = build("http://h", true, true, &clients(&[]), true, 0);
        assert!(!no_client.ready);
        // Understanding off does NOT block ready (it's optional).
        let no_und = build("http://h", true, true, &clients(&["cursor"]), false, 0);
        assert!(no_und.ready);
    }

    #[test]
    fn unmet_checks_carry_a_manual_fix() {
        let g = build("http://h", false, false, &clients(&[]), false, 0);
        let model = g.checks.iter().find(|c| c.key == "model").unwrap();
        assert_eq!(model.fix.as_deref(), Some("localmem fetch-model"));
        assert!(g
            .checks
            .iter()
            .find(|c| c.key == "service")
            .unwrap()
            .fix
            .is_some());
        // Render surfaces the fallbacks.
        let t = g.render_terminal();
        assert!(t.contains("To finish"));
        assert!(t.contains("localmem fetch-model"));
    }

    #[test]
    fn lists_unwired_clients_with_commands() {
        let g = build("http://h", true, true, &clients(&["cursor"]), true, 0);
        let t = g.render_terminal();
        assert!(t.contains("Wire another AI client"));
        assert!(t.contains("localmem mcp install claude-code"));
        assert!(t.contains("localmem mcp install windsurf"));
        // The wired one is not re-listed as an action.
        assert!(!t.contains("localmem mcp install cursor"));
    }

    #[test]
    fn ready_render_says_set_up_and_shows_checks() {
        let g = build(
            "http://127.0.0.1:7788",
            true,
            true,
            &clients(&["claude-code"]),
            true,
            1,
        );
        let t = g.render_terminal();
        assert!(t.contains("localmem is set up"));
        assert!(t.contains("✓ Embedder model"));
        assert!(t.contains("import-wizard --apply"));
        assert!(g.render_markdown().contains("# localmem is set up"));
    }
}
