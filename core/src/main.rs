// localmem core binary
//
// Thin CLI wrapper. All domain logic lives in the library (src/lib.rs).
// See ARCHITECTURE.md for the design and ROADMAP.md for the build plan.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use localmem::cli::{
    audit, brief, doctor, export, fetch_model, forget, hooks, import_wizard, init, journal, mcp,
    profile, recall, recent, reindex, replay,
    search::{self, Mode as SearchMode},
    service, setup, status, subjects, summarize, tag_arg, tags as tags_cmd, todo, understand,
    write,
};
use localmem::server;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "localmem", version, about = "Local-first AI memory layer")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Override the localmem home directory (default: ~/.localmem)
    #[arg(long, env = "LOCALMEM_HOME", global = true)]
    home: Option<String>,

    /// Suppress all log output on stderr. Useful when piping `--json`
    /// output to agents/scripts that can't filter INFO/WARN noise.
    /// Auto-enabled when stderr is not a terminal (so `localmem write
    /// ... 2>/dev/null` style works without the flag). Field-feedback
    /// fix (2026-06-04).
    #[arg(long, global = true)]
    quiet: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Initialize a new localmem directory
    Init {
        /// Emit a single JSON object on stdout instead of a human-readable line.
        #[arg(long)]
        json: bool,
    },

    /// Ingest a memory (text from stdin or --content)
    Write {
        #[arg(long)]
        content: Option<String>,

        #[arg(long)]
        source: Option<String>,

        /// Container tags as a comma-separated `key=value` list. Tags
        /// scope the capture for later filtering on `search`, `recall`,
        /// and `profile`. Example: `--tags project=localmem,topic=async`.
        /// See SPEC_V0_2 "container-tag model" for reserved keys.
        #[arg(long, value_name = "K=V[,K=V...]")]
        tags: Option<String>,

        /// Closed-core kind taxonomy (T-52). One of `fact`,
        /// `preference`, `decision`, `constraint`, `todo`, `note`.
        /// Any other value round-trips as an extension kind (treated
        /// as `note` for behavioral purposes; preserved for replay).
        /// Default: `note`.
        #[arg(long)]
        kind: Option<String>,

        /// Valid-time of the memory (RFC3339), i.e. WHEN it happened, not
        /// when you are recording it. Sets the capture's temporal envelope
        /// so bitemporal recall and `search --at-time` resolve it to the
        /// real instant. Omit to stamp now. Example:
        /// `--as-of 2023-01-15T10:00:00Z`.
        #[arg(long, value_name = "RFC3339")]
        as_of: Option<String>,

        /// Emit a single JSON object on stdout instead of a human-readable line.
        #[arg(long)]
        json: bool,
    },

    /// Hybrid search over your memory
    Search {
        /// Query text. Pass as the positional argument or via
        /// `--content` for parity with `localmem write --content`.
        /// Field-feedback fix (2026-06-04): inconsistent flag shapes
        /// between `search` and `write` cost agents a round-trip.
        query: Option<String>,

        /// Synonym for the positional QUERY argument. Mutually
        /// exclusive with the positional form.
        #[arg(long, conflicts_with = "query")]
        content: Option<String>,

        #[arg(long, default_value = "10")]
        k: usize,

        /// Retrieval mode. `hybrid` (default) blends BM25 + ANN via RRF
        /// with optional bitemporal filter (T-23). `lex` is BM25-only,
        /// useful for exact-term recall. `vec` shares the hybrid path in
        /// v0.1; a pure vector-only short-circuit is reserved for later.
        #[arg(long, value_enum, default_value_t = SearchMode::Hybrid)]
        mode: SearchMode,

        /// Bitemporal "as of" timestamp (RFC3339). When set, hides hits
        /// whose derived facts have all been retired by that time. Only
        /// affects hybrid/vec modes; lex mode ignores it.
        #[arg(long, value_name = "RFC3339")]
        at_time: Option<String>,

        /// Tag filter as a comma-separated `key=value` list (T-51). A
        /// hit passes when every pair matches the capture's tags.
        /// Example: `--tags project=localmem,topic=async`.
        #[arg(long, value_name = "K=V[,K=V...]")]
        tags: Option<String>,

        /// Project scope (SPEC §2.8). Restrict to this project plus
        /// user-common (global) memory, never another project. This is
        /// the isolation default an agent should use; cross-project
        /// search is omitting it. Example: `--project localmem`.
        #[arg(long)]
        project: Option<String>,

        /// Kind filter (T-52b). Restrict to one of the closed-core
        /// kinds (`fact`, `preference`, `decision`, `constraint`,
        /// `todo`, `note`) or an extension kind string. Drops hits
        /// whose stored kind doesn't match exactly.
        #[arg(long)]
        kind: Option<String>,

        /// Todo `done` filter (T-52b). Pairs with `--kind todo`
        /// (though it works on any capture). `--done false` returns
        /// only open todos; `--done true` returns only completed
        /// ones; unset disables the filter.
        #[arg(long)]
        done: Option<bool>,

        /// Emit results as a single JSON object on stdout.
        #[arg(long)]
        json: bool,
    },

    /// Recall facts about an entity (audit view by default; `--at-time` for bitemporal)
    Recall {
        entity: String,

        /// Bitemporal "as of" timestamp (RFC3339). Hides facts retired by then.
        #[arg(long, value_name = "RFC3339")]
        at_time: Option<String>,

        /// Tag filter as a comma-separated `key=value` list (T-51b). A
        /// fact passes when every pair matches the tags inherited from
        /// its source capture. Example: `--tags project=localmem`.
        #[arg(long, value_name = "K=V[,K=V...]")]
        tags: Option<String>,

        #[arg(long)]
        json: bool,
    },

    /// Generate a synthesized markdown profile from facts
    Profile {
        /// Restrict to a single subject (entity). Default: all subjects.
        #[arg(long)]
        scope: Option<String>,

        /// Tag filter as a comma-separated `key=value` list (T-51b).
        /// Composes with `--scope` via AND semantics. Example:
        /// `--tags project=localmem,topic=auth`.
        #[arg(long, value_name = "K=V[,K=V...]")]
        tags: Option<String>,

        #[arg(long)]
        json: bool,
    },

    /// Soft-delete by emitting a `forget` event and retiring matching facts
    Forget {
        /// Event id to forget. Accepts a capture id (retires every derived fact)
        /// or a fact id (retires that one fact).
        #[arg(long, conflicts_with = "criteria")]
        target: Option<String>,

        /// JSON criteria object. v0.1: `{"subject": "...", "predicate": "..."}`.
        #[arg(long, conflicts_with = "target")]
        criteria: Option<String>,

        /// Optional reason recorded on the `forget` event.
        #[arg(long)]
        reason: Option<String>,

        #[arg(long)]
        json: bool,
    },

    /// Export the event log to a portable single-file archive
    Export {
        /// Destination path for the archive file.
        path: String,

        #[arg(long)]
        json: bool,
    },

    /// Inspect the policy decision journal
    Journal {
        /// Time window: any of `45s`, `30m`, `1h`, `1d`, `2w`. Default 1d.
        #[arg(long, default_value = "1d")]
        since: String,

        /// Restrict to one action (COMMIT, UPDATE, DEDUP, SKIP, FORGET).
        #[arg(long)]
        action: Option<String>,

        /// Emit a single JSON object on stdout instead of human-readable lines.
        #[arg(long)]
        json: bool,
    },

    /// Rebuild derived stores from the event log
    Replay {
        /// Emit a single JSON object on stdout instead of a human-readable line.
        #[arg(long)]
        json: bool,
    },

    /// Re-embed all captures with the current embedder
    Reindex {
        #[arg(long)]
        json: bool,
        /// Stop after N signal-capture vectors: a fast smoke test that
        /// embedding + writing work on this machine (and the auto-tuned batch
        /// sizes are sane) before a full rebuild. Omit to reindex everything.
        #[arg(long)]
        sample: Option<u64>,
    },

    /// Run the local HTTP server (for the MCP server to talk to).
    /// If --addr is omitted, resolves from <home>/config.toml `[server].addr`,
    /// then the `LOCALMEM_SERVER_ADDR` env var, then 127.0.0.1:7788.
    Serve {
        #[arg(long)]
        addr: Option<String>,
        /// Also serve the local web dashboard at the same address (no separate
        /// dashboard/serve.py needed). Open http://<addr>/ in a browser.
        #[arg(long)]
        dashboard: bool,
    },

    /// One-command onboarding: init the home, fetch the embedder model, wire
    /// detected MCP clients, install the always-on service, and verify.
    /// Best-effort per step.
    Setup {
        /// Skip downloading the embedder model (search stays lexical-only).
        #[arg(long)]
        no_model: bool,
        /// Skip installing the always-on auto-launch service.
        #[arg(long)]
        no_service: bool,
        #[arg(long)]
        json: bool,
    },

    /// Concise health + memory summary (lighter than `doctor`).
    Status {
        #[arg(long)]
        json: bool,
    },

    /// Internal: Claude Code hook handler (auto-capture). Reads the hook event
    /// JSON on stdin. Wired into the agent by `localmem hooks install`; not
    /// meant to be run by hand. Always exits 0 so it never disrupts the agent.
    #[command(hide = true)]
    Hook {
        /// Event: `prompt-submit` | `post-tool` | `session-start` | `session-end`.
        event: String,
    },

    /// Readiness + setup for the local-LLM understanding layer (summary +
    /// entities + intent on top of raw captures). Detects Ollama, checks the
    /// model, and explains cost/privacy. Never auto-installs anything.
    /// With `--backfill`, instead enqueue captures that predate the worker so
    /// existing memories get understood (routes to the running server).
    Understand {
        /// Understand captures that have no understanding yet (idempotent).
        #[arg(long)]
        backfill: bool,
        /// Scope the backfill to a project tag.
        #[arg(long)]
        project: Option<String>,
        /// Max captures to enqueue (most-recent first).
        #[arg(long)]
        limit: Option<usize>,
        /// Rebuild the typed-graph NODE layer (entity_mentions) from existing
        /// Understanding events. Offline + idempotent; no server or model needed.
        #[arg(long)]
        rebuild_graph: bool,
    },

    /// Register (or remove) the always-on auto-launch service so the core runs
    /// at login: launchd on macOS, systemd --user on Linux.
    Service {
        /// `install` | `uninstall` | `status`.
        action: String,
    },

    /// Render the Session Boot Briefing: a synthesized, current-state-first
    /// digest of a project's memory (NOW / open loops / watch-outs / rules /
    /// preferences / pointers). Routes to the running server; needs
    /// understanding enabled.
    Brief {
        /// Scope to a project (matches the `project` tag). Omit for all projects.
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Import memories from an export. Format is auto-detected (ChatGPT or
    /// Claude conversation export, or a localmem `archive`); override with
    /// `--format`. Use `--dry-run` to preview what would be imported.
    Import {
        /// Path to the export file, or a Claude Code transcript directory
        /// (e.g. ~/.claude/projects).
        path: String,
        /// Force a specific format instead of auto-detecting: `chatgpt`,
        /// `claude` (claude.ai export), `claude-code` (session transcripts),
        /// or `archive`.
        #[arg(long)]
        format: Option<String>,
        /// Parse and report what would be imported, without writing anything.
        #[arg(long)]
        dry_run: bool,

        #[arg(long)]
        json: bool,
    },

    /// Wire localmem into MCP-compatible AI clients (T-50)
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    /// Wire localmem auto-capture hooks into an AI agent so it captures your
    /// coding sessions automatically (and knows localmem is its memory).
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },

    /// List distinct entity subjects in the facts table with row counts (T-53).
    Subjects {
        /// Emit results as a single JSON object on stdout.
        #[arg(long)]
        json: bool,
    },

    /// List container tags in use across captures with counts (T-53).
    Tags {
        /// Emit results as a single JSON object on stdout.
        #[arg(long)]
        json: bool,
    },

    /// Show the last N captures, newest first (T-53).
    Recent {
        /// Maximum number of captures to return. SPEC default is 20.
        #[arg(long, default_value_t = recent::DEFAULT_LIMIT)]
        limit: usize,

        /// Emit results as a single JSON object on stdout.
        #[arg(long)]
        json: bool,
    },

    /// Synthesized brief over the (optionally tag/kind-filtered) memory store (T-53).
    Summarize {
        /// Tag filter as a comma-separated `key=value` list.
        #[arg(long, value_name = "K=V[,K=V...]")]
        tags: Option<String>,

        /// Restrict to a single kind: fact|preference|decision|constraint|todo|note
        /// (other values round-trip as extension kinds).
        #[arg(long)]
        kind: Option<String>,

        /// Emit the result as a single JSON object on stdout.
        #[arg(long)]
        json: bool,
    },

    /// Download a registered ML model into `<home>/models/<slug>/`
    /// (T-62). Default is the BGE-small embedder that vector search
    /// needs. SHA-256 verified when the registry hash is armed.
    /// Idempotent: existing files that verify are skipped.
    /// `--dry-run` reports what would happen without downloading.
    FetchModel {
        /// Registered model slug. Run without `--model` to fetch the
        /// default (`bge-small-en-v1.5`). Use `--url` for a custom
        /// HuggingFace download instead of a registered name.
        #[arg(long)]
        model: Option<String>,

        /// Custom HTTPS download URL. Bypasses the registry; no SHA
        /// verification (the registry is the trust path). `--model`
        /// then becomes the destination subdirectory slug under
        /// `<home>/models/`. Defaults to `custom` if `--model` is
        /// also omitted.
        #[arg(long)]
        url: Option<String>,

        /// Report what would land + total size; do not download.
        #[arg(long)]
        dry_run: bool,

        /// Emit the result as a single JSON object on stdout.
        #[arg(long)]
        json: bool,
    },

    /// Scan common locations (~/Downloads, ~/Desktop, CWD) for ChatGPT
    /// or Claude memory exports and offer to import them (capability #5,
    /// "First-run import wizard"). Without `--apply`, only reports what
    /// it found; with `--apply`, runs the matching importer for every
    /// HIGH-confidence detection.
    ImportWizard {
        /// Run the matching importer for every HIGH-confidence
        /// detection. Without this flag, the wizard is read-only.
        #[arg(long)]
        apply: bool,

        /// Emit the result as a single JSON object on stdout.
        #[arg(long)]
        json: bool,
    },

    /// Trace a fact back to its source capture, journal entries, and any
    /// follow-up forget/update events (T-53).
    Audit {
        /// Fact event id (ULID).
        fact_id: String,

        /// Emit the result as a single JSON object on stdout.
        #[arg(long)]
        json: bool,
    },

    /// Flip the done state of a todo-kind capture (T-52b). Emits an
    /// `UpdateCapture` event so the event log stays append-only and
    /// `localmem replay` reconstructs the latest state. The CLI
    /// refuses non-todo captures so the flag never lands on a kind
    /// that doesn't render it.
    Todo {
        #[command(subcommand)]
        action: TodoAction,
    },

    /// Run a per-check diagnostic of the install (T-48).
    /// Reports PASS/WARN/FAIL for: binary on PATH, home initialised,
    /// embedder model, server reachable, macOS Gatekeeper, MCP wiring
    /// per client. `--fix` auto-applies safe fixes (currently only
    /// `localmem init` when the home dir is missing).
    Doctor {
        /// Apply safe fixes after printing the report.
        #[arg(long)]
        fix: bool,

        /// Emit a single JSON object on stdout instead of the human
        /// table.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum TodoAction {
    /// Mark a todo capture as done.
    Done {
        /// Capture event id (ULID).
        target_id: String,

        /// Optional reason recorded on the UpdateCapture event for
        /// future audit (e.g. "shipped" / "abandoned" / etc.).
        #[arg(long)]
        reason: Option<String>,

        #[arg(long)]
        json: bool,
    },
    /// Reopen a previously-done todo capture.
    Open {
        /// Capture event id (ULID).
        target_id: String,

        #[arg(long)]
        reason: Option<String>,

        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum McpAction {
    /// Auto-install localmem into the named client's MCP config.
    /// Supported clients: claude, claude-code, cursor, windsurf, cline.
    /// Codex and aider report a clear "not yet supported" message in v0.2.
    Install {
        /// Client slug (e.g. `claude`, `cursor`, `cline`).
        client: String,

        #[arg(long)]
        json: bool,
    },

    /// Show install status across every known MCP client.
    List {
        #[arg(long)]
        json: bool,
    },

    /// Remove the localmem entry from the named client's MCP config.
    Uninstall {
        /// Client slug (e.g. `claude`, `cursor`, `cline`).
        client: String,

        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum HooksAction {
    /// Wire auto-capture hooks + the memory pointer into a client (claude-code).
    Install {
        #[arg(default_value = "claude-code")]
        client: String,
    },
    /// Remove the localmem hooks + memory pointer from a client.
    Uninstall {
        #[arg(default_value = "claude-code")]
        client: String,
    },
    /// Show whether the hooks + pointer are installed.
    Status {
        #[arg(default_value = "claude-code")]
        client: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse Cli BEFORE tracing init so we can honor --quiet and the
    // non-TTY auto-suppress. Field-feedback fix (2026-06-04): agents
    // piping `--json` output otherwise see INFO/WARN noise on stderr
    // they cannot filter.
    let cli = Cli::parse();
    init_tracing(cli.quiet);
    info!(?cli.command, "localmem starting");

    match cli.command {
        Command::Init { json } => init::run(cli.home.as_deref(), json)?,
        Command::Write {
            content,
            source,
            tags,
            kind,
            as_of,
            json,
        } => {
            let tags_map = tag_arg::parse_tags_arg(tags.as_deref()).context("parse --tags")?;
            // Kind: parse the string via Kind::from (canonical or
            // Other fallback). None → Note default.
            let kind_value = kind.map(localmem::kind::Kind::from).unwrap_or_default();
            write::run(
                cli.home.as_deref(),
                content.as_deref(),
                source.as_deref(),
                tags_map,
                kind_value,
                as_of.as_deref(),
                json,
            )
            .await?;
        }
        Command::Search {
            query,
            content,
            k,
            mode,
            at_time,
            tags,
            project,
            kind,
            done,
            json,
        } => {
            let query = query
                .or(content)
                .context("search requires a query (pass as positional or via --content)")?;
            let at_time = at_time.as_deref().map(parse_at_time).transpose()?;
            let tags_map = tag_arg::parse_tags_arg(tags.as_deref()).context("parse --tags")?;
            let kind_filter = kind.map(localmem::kind::Kind::from);
            search::run(
                cli.home.as_deref(),
                &query,
                k,
                mode,
                at_time,
                tags_map,
                project,
                kind_filter,
                done,
                json,
            )
            .await?;
        }
        Command::Recall {
            entity,
            at_time,
            tags,
            json,
        } => {
            let at_time = at_time.as_deref().map(parse_at_time).transpose()?;
            let tags_map = tag_arg::parse_tags_arg(tags.as_deref()).context("parse --tags")?;
            recall::run(cli.home.as_deref(), &entity, at_time, tags_map, json)?;
        }
        Command::Profile { scope, tags, json } => {
            let tags_map = tag_arg::parse_tags_arg(tags.as_deref()).context("parse --tags")?;
            profile::run(cli.home.as_deref(), scope.as_deref(), tags_map, json)?;
        }
        Command::Forget {
            target,
            criteria,
            reason,
            json,
        } => {
            forget::run(
                cli.home.as_deref(),
                target.as_deref(),
                criteria.as_deref(),
                reason.as_deref(),
                json,
            )?;
        }
        Command::Export { path, json } => {
            export::run_export(cli.home.as_deref(), &path, json)?;
        }
        Command::Journal {
            since,
            action,
            json,
        } => journal::run(cli.home.as_deref(), &since, action.as_deref(), json)?,
        Command::Replay { json } => replay::run(cli.home.as_deref(), json).await?,
        Command::Reindex { json, sample } => {
            reindex::run(cli.home.as_deref(), json, sample).await?
        }
        Command::Serve { addr, dashboard } => {
            let home = resolve_home(cli.home.as_deref())?;
            let cfg = localmem::config::Config::load(&home).context("load config")?;
            let resolved = addr.unwrap_or_else(|| cfg.server.addr.clone());
            let parsed: SocketAddr = resolved
                .parse()
                .with_context(|| format!("parse server address {resolved}"))?;
            let state = server::AppState::open(&home)
                .await
                .context("open localmem home")?;
            if dashboard {
                info!(addr = %parsed, "dashboard available at http://{parsed}/");
            }
            server::serve(parsed, state, dashboard).await?;
        }
        Command::Setup {
            no_model,
            no_service,
            json,
        } => setup::run(cli.home.as_deref(), no_model, no_service, json)?,
        Command::Status { json } => {
            let core_addr = resolve_core_addr(cli.home.as_deref())?;
            status::run(cli.home.as_deref(), &core_addr, json)?
        }
        Command::Hook { event } => hooks::run(cli.home.as_deref(), &event)?,
        Command::Understand {
            backfill,
            project,
            limit,
            rebuild_graph,
        } => {
            if rebuild_graph {
                understand::run_rebuild_graph(cli.home.as_deref())?
            } else if backfill {
                understand::run_backfill(cli.home.as_deref(), project, limit).await?
            } else {
                understand::run_status(cli.home.as_deref())?
            }
        }
        Command::Brief { project, json } => brief::run(cli.home.as_deref(), project, json).await?,
        Command::Service { action } => service::run(&action, cli.home.as_deref())?,
        Command::Hooks { action } => match action {
            HooksAction::Install { client } => hooks::install(cli.home.as_deref(), &client)?,
            HooksAction::Uninstall { client } => hooks::uninstall(cli.home.as_deref(), &client)?,
            HooksAction::Status { client } => hooks::status(cli.home.as_deref(), &client)?,
        },
        Command::Import {
            path,
            format,
            dry_run,
            json,
        } => export::run_import(cli.home.as_deref(), &path, format.as_deref(), dry_run, json)?,
        Command::Mcp { action } => {
            // Resolve the localmem core HTTP addr the same way `serve`
            // does so the MCP entry points at the user's actual core.
            let core_addr = resolve_core_addr(cli.home.as_deref())?;
            match action {
                McpAction::Install { client, json } => {
                    // mcp install uses $HOME for the client config path,
                    // not the localmem home (those are different dirs).
                    mcp::run_install(None, &client, &core_addr, json)?;
                }
                McpAction::List { json } => {
                    mcp::run_list(None, json)?;
                }
                McpAction::Uninstall { client, json } => {
                    mcp::run_uninstall(None, &client, json)?;
                }
            }
        }
        Command::Subjects { json } => subjects::run(cli.home.as_deref(), json)?,
        Command::Tags { json } => tags_cmd::run(cli.home.as_deref(), json)?,
        Command::Recent { limit, json } => recent::run(cli.home.as_deref(), limit, json)?,
        Command::Summarize { tags, kind, json } => {
            let tags_map = tag_arg::parse_tags_arg(tags.as_deref()).context("parse --tags")?;
            let kind_value = kind.map(localmem::kind::Kind::from);
            summarize::run(cli.home.as_deref(), tags_map, kind_value, json)?;
        }
        Command::FetchModel {
            model,
            url,
            dry_run,
            json,
        } => fetch_model::run(
            cli.home.as_deref(),
            model.as_deref(),
            url.as_deref(),
            dry_run,
            json,
        )?,
        Command::ImportWizard { apply, json } => {
            import_wizard::run(cli.home.as_deref(), apply, json)?
        }
        Command::Audit { fact_id, json } => audit::run(cli.home.as_deref(), &fact_id, json)?,
        Command::Todo { action } => match action {
            TodoAction::Done {
                target_id,
                reason,
                json,
            } => todo::run(
                cli.home.as_deref(),
                todo::TodoAction::Done,
                &target_id,
                reason.as_deref(),
                json,
            )?,
            TodoAction::Open {
                target_id,
                reason,
                json,
            } => todo::run(
                cli.home.as_deref(),
                todo::TodoAction::Open,
                &target_id,
                reason.as_deref(),
                json,
            )?,
        },
        Command::Doctor { fix, json } => {
            let core_addr = resolve_core_addr(cli.home.as_deref())?;
            doctor::run(cli.home.as_deref(), &core_addr, fix, json)?;
        }
    }
    Ok(())
}

/// Resolve the localmem core HTTP server address the same way
/// `serve` does. Used by `mcp install` so the rendered MCP entry
/// targets the user's configured core, not a hardcoded default.
fn resolve_core_addr(home_override: Option<&str>) -> Result<String> {
    let home = resolve_home(home_override)?;
    let cfg = localmem::config::Config::load(&home).context("load config for core addr")?;
    Ok(cfg.server.addr)
}

/// Resolve the localmem home directory the same way `cli::search` does:
/// explicit `--home` (or `LOCALMEM_HOME`) wins, else `$HOME/.localmem`.
fn resolve_home(home: Option<&str>) -> Result<PathBuf> {
    if let Some(h) = home.filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var("HOME")
        .context("HOME environment variable is not set; pass --home explicitly")?;
    Ok(PathBuf::from(home).join(".localmem"))
}

/// Parse the `--at-time` CLI argument as an RFC3339 timestamp normalized
/// to UTC. Exposed at this layer so the search handler can stay in terms of
/// `DateTime<Utc>` and not re-derive parsing rules from a string flag.
fn parse_at_time(s: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("parse --at-time as RFC3339: {s:?}"))?;
    Ok(parsed.with_timezone(&Utc))
}

/// Install the `tracing` subscriber, honoring `--quiet` and the
/// non-TTY auto-suppress rule.
///
/// Field-feedback fix (2026-06-04): agents piping `--json` saw
/// INFO/WARN noise they could not filter. Resolution hierarchy
/// (highest priority first):
///
/// 1. `--quiet` → install no-op subscriber. Explicit user request
///    to silence; wins over everything.
/// 2. `RUST_LOG=...` set → install the full subscriber regardless of
///    TTY. Escape hatch for debugging non-TTY workflows.
/// 3. Stderr is not a TTY → install no-op subscriber. Agents and
///    scripts piping our output do not want log contamination.
/// 4. Otherwise → install the default subscriber (interactive use).
fn init_tracing(quiet_flag: bool) {
    use std::io::IsTerminal;

    if quiet_flag {
        return;
    }
    let rust_log_set = std::env::var_os("RUST_LOG").is_some();
    let stderr_is_tty = std::io::stderr().is_terminal();
    if !rust_log_set && !stderr_is_tty {
        return;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "localmem=info".into()),
        )
        .init();
}
