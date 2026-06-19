//! `localmem search` handler.
//!
//! Dispatches to lexical / vector / hybrid retrieval. See TASKS.md task
//! T-07 (lexical-only path) and T-24 (hybrid default).
//!
//! `Mode::Hybrid` is the default per SPEC.md "memory_search" surface; it
//! delegates to [`crate::retriever::HybridRetriever`]. `Mode::Vec` shares the
//! same code path (vector-only is implied by the embedder dominating the
//! merge when no lexical terms overlap; v0.1 does not ship a vector-only
//! short-circuit). `Mode::Lex` keeps the BM25-only path for users who
//! deliberately want exact-term-only results.

use crate::embed::{Embedder, EMBEDDING_DIM};
use crate::facts::FactsStore;
use crate::lexical::{LexicalIndex, LexicalResultExt};
use crate::retriever::{Filters, HybridHit, HybridRetriever};
use crate::vectors::VectorStore;
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::ValueEnum;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Retrieval mode for `localmem search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// BM25 over capture content (T-06). Use when only literal-term recall
    /// is acceptable (e.g. searching for a known ULID or error code).
    Lex,
    /// ANN over embeddings. v0.1 routes through the hybrid retriever; a
    /// pure vector-only mode is reserved for a later release.
    Vec,
    /// Reciprocal Rank Fusion of BM25 + ANN with optional bitemporal
    /// filter (T-23). Default.
    Hybrid,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Lex => "lex",
            Mode::Vec => "vec",
            Mode::Hybrid => "hybrid",
        }
    }
}

/// Unified search result, regardless of which mode produced it. We expose
/// a single shape to the CLI so JSON consumers do not have to branch on
/// `mode` to read fields. `sources` is empty for `Mode::Lex` (BM25-only)
/// and lists `lex`/`vec` for hybrid hits.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DisplayHit {
    pub event_id: String,
    pub snippet: String,
    pub score: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Valid-time of the hit (when the memory happened, RFC3339), surfaced so
    /// the CLI exposes the temporal envelope the write/import paths now record
    /// (T-113). `None` when the source carried no instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
}

impl DisplayHit {
    fn from_lexical(h: crate::lexical::LexicalHit) -> Self {
        // The lexical index stores the capture's valid-time (effective capture
        // instant) as `ts`, so it IS the valid_from to surface.
        let valid_from = Some(h.ts.to_rfc3339_opts(SecondsFormat::Millis, true));
        Self {
            event_id: h.event_id,
            snippet: h.snippet,
            score: h.score,
            sources: Vec::new(),
            valid_from,
        }
    }

    fn from_hybrid(h: HybridHit) -> Self {
        Self {
            event_id: h.event_id,
            snippet: h.content,
            score: h.score,
            sources: h.sources.into_iter().map(String::from).collect(),
            valid_from: h
                .valid_from
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true)),
        }
    }
}

/// Entry point for the `search` subcommand.
///
/// `home` is the optional override from the global `--home` flag (which
/// also picks up `LOCALMEM_HOME` via clap's `env` attribute). When `None`,
/// we fall back to `$HOME/.localmem`.
///
/// `at_time` is the bitemporal "valid as of" timestamp for the hybrid
/// retriever's facts filter (T-23). Lex-mode searches ignore it because
/// the lexical index has no fact lineage to filter on.
///
/// `tags` is a subset-match filter (T-51). An empty map disables tag
/// filtering and matches v0.1 behavior; a non-empty map applies the
/// AND-of-pairs predicate to every retrieval path.
// This is a CLI dispatch boundary: every parameter is a distinct `localmem
// search` flag forwarded straight from clap. Bundling them into a struct would
// add indirection without cohesion, so the arg count is inherent, not a smell.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    home: Option<&str>,
    query: &str,
    k: usize,
    mode: Mode,
    at_time: Option<DateTime<Utc>>,
    tags: BTreeMap<String, String>,
    project: Option<String>,
    kind_filter: Option<crate::kind::Kind>,
    done_filter: Option<bool>,
    as_json: bool,
) -> Result<()> {
    let home = resolve_home(home)?;
    // SPEC §2.8: `--project <label>` scopes to that project plus user-common
    // (global) memory, never another project. The collision-proof `project_path`
    // is what the MCP server uses; the CLI accepts the readable `project` label.
    let scope =
        project
            .as_ref()
            .filter(|p| !p.trim().is_empty())
            .map(|p| crate::retriever::Scope {
                key: "project".to_string(),
                value: p.clone(),
                include_global: true,
            });
    // The facts DuckDB lock is exclusive: while the always-on service holds it,
    // an in-process hybrid/vector search cannot open the store. Route those
    // modes through the running server (lex is reader-only and stays local).
    // Kind/done filters have no server-side equivalent yet, so they keep the
    // in-process path.
    if !matches!(mode, Mode::Lex) && kind_filter.is_none() && done_filter.is_none() {
        let mut body = serde_json::json!({ "query": query, "k": k, "browse": true });
        if let Some(t) = at_time {
            body["at_time"] = serde_json::Value::String(t.to_rfc3339());
        }
        if !tags.is_empty() {
            body["tags"] = serde_json::to_value(&tags).unwrap_or_default();
        }
        if let Some(s) = scope.as_ref() {
            body["scope"] = serde_json::json!({
                "key": s.key, "value": s.value, "include_global": s.include_global
            });
        }
        if let Some(v) = crate::cli::server_post(&home, "/search", body) {
            let hits: Vec<DisplayHit> = v
                .get("results")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|r| DisplayHit {
                            // The server response carries the event id inside
                            // `sources` (it builds sources = vec![event_id]),
                            // so pull the id from there. It does not expose the
                            // lexical/vector source labels, so the routed view
                            // omits those.
                            event_id: r
                                .get("sources")
                                .and_then(|x| x.as_array())
                                .and_then(|a| a.first())
                                .and_then(|s| s.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            snippet: r
                                .get("fact")
                                .and_then(|x| x.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            score: r.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32,
                            sources: Vec::new(),
                            valid_from: r
                                .get("valid_from")
                                .and_then(|x| x.as_str())
                                .map(String::from),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut out = io::stdout().lock();
            return write_output(&mut out, query, mode, &hits, as_json);
        }
    }

    let tag_filter = if tags.is_empty() { None } else { Some(&tags) };
    // Search is not the audit path. Per SPEC_V0_2 (T-51c),
    // `visibility=private` captures stay hidden; retention TTL
    // applies. The lex hit carries both `ts` and `tags`, so the
    // predicate runs without a second lookup.
    let now = Utc::now();
    let visibility = crate::reserved_tags::Visibility::Default;

    // `effective_mode` tracks the mode actually used, which can differ
    // from the requested `mode` when hybrid degrades to lex on a
    // missing embedder. The renderer below uses this so JSON output
    // honestly reports `mode = "lex"` after a degrade.
    let mut effective_mode = mode;
    let hits: Vec<DisplayHit> = match mode {
        Mode::Lex => {
            // Reader-only open: no Tantivy writer lock, so this coexists
            // with `localmem serve` (which holds the writer). See
            // CLAUDE.md "CLI and server must be peers".
            let idx = LexicalIndex::open_reader_only(&home)
                .lex_context("open lexical index for reading")?;
            let lex_hits = idx
                .search(query, k, tag_filter)
                .context("run lexical search")?;
            lex_hits
                .into_iter()
                .filter(|h| {
                    crate::reserved_tags::is_visible(&h.tags, h.ts, now, visibility)
                        && scope.as_ref().map_or(true, |s| {
                            h.tags
                                .get(&s.key)
                                .map_or(s.include_global, |v| v == &s.value)
                        })
                })
                .map(DisplayHit::from_lexical)
                .collect()
        }
        Mode::Vec | Mode::Hybrid => {
            let model_dir = resolve_model_dir(&home);
            // Field-feedback fix (P1, 2026-06-04): when the BGE model
            // is missing and the user did not explicitly ask for
            // vec-only, fall back to lex search instead of
            // dead-ending. Hybrid is the DEFAULT mode, so a
            // missing-model install previously broke `localmem
            // search` out of the box; now we degrade with a one-line
            // WARN and continue.
            //
            // `Mode::Vec` still errors when the embedder is missing
            // because the user asked for vector-only explicitly and
            // a silent downgrade would mislead them.
            let embedder_opt = match Embedder::load(&model_dir) {
                Ok(e) => Some(e),
                Err(err) if matches!(mode, Mode::Hybrid) => {
                    warn_embedder_missing_once(&model_dir, &err);
                    effective_mode = Mode::Lex;
                    None
                }
                Err(err) => {
                    return Err(err.context(format!(
                        "load embedder from {} (set LOCALMEM_MODEL_DIR to override, or pass --mode lex)",
                        model_dir.display()
                    )));
                }
            };
            match embedder_opt {
                None => {
                    // Hybrid degraded to lex. Reuse the lex code path
                    // so the visibility predicate stays consistent
                    // with `Mode::Lex` (single source of truth for
                    // reserved-tag filtering).
                    let idx = LexicalIndex::open_reader_only(&home)
                        .lex_context("open lexical index for reading")?;
                    let lex_hits = idx
                        .search(query, k, tag_filter)
                        .context("run lexical search (hybrid degraded; embedder unavailable)")?;
                    lex_hits
                        .into_iter()
                        .filter(|h| {
                            crate::reserved_tags::is_visible(&h.tags, h.ts, now, visibility)
                                && scope.as_ref().map_or(true, |s| {
                                    h.tags
                                        .get(&s.key)
                                        .map_or(s.include_global, |v| v == &s.value)
                                })
                        })
                        .map(DisplayHit::from_lexical)
                        .collect()
                }
                Some(embedder) => {
                    let vectors = VectorStore::open(&home, EMBEDDING_DIM)
                        .await
                        .context("open vector store")?;
                    // Hybrid retriever only reads from the lex index;
                    // reader-only open keeps the CLI from fighting the
                    // server for the writer.
                    let lexical = LexicalIndex::open_reader_only(&home)
                        .lex_context("open lexical index for reading")?;
                    let facts = crate::cli::open_facts(&home)?;
                    // T-57 + T-73: thread `[retriever].recency_weight`
                    // AND per-kind half-lives from config so the CLI
                    // search path matches the server's bias. A missing
                    // config file falls back to defaults via
                    // `Config::default`, which means a sensible set of
                    // half-lives (fact=90d, preference=180d, etc.) is
                    // always applied; disable by writing an empty
                    // `[retriever].decay_half_life` map in
                    // `config.toml`.
                    let retriever_cfg = crate::config::Config::load(&home)
                        .unwrap_or_default()
                        .retriever;
                    // Phase 2 / T-74b: load the cross-encoder reranker when
                    // enabled and present; degrade to no-rerank otherwise.
                    let reranker = if retriever_cfg.rerank {
                        std::sync::Arc::new(tokio::sync::Mutex::new(
                            crate::rerank::Reranker::load(home.join("models").join("reranker"))
                                .ok(),
                        ))
                    } else {
                        std::sync::Arc::new(tokio::sync::Mutex::new(None))
                    };
                    let retriever = HybridRetriever::new(embedder, vectors, lexical, facts)
                        .with_recency_weight(retriever_cfg.recency_weight)
                        .with_decay_half_lives(retriever_cfg.decay_half_lives_in_days())
                        .with_mmr_lambda(retriever_cfg.mmr_lambda)
                        .with_reranker(reranker);
                    let mut filters = Filters::with_tags(tags);
                    filters.scope = scope.clone();
                    let hybrid_hits = retriever
                        .search(query, k, at_time, &filters)
                        .await
                        .context("run hybrid search")?;
                    hybrid_hits
                        .into_iter()
                        .map(DisplayHit::from_hybrid)
                        .collect()
                }
            }
        }
    };

    // T-52b: kind + done post-filter. We look up each hit's stored
    // metadata via the lex index (one query per hit, bounded by
    // `k`). When neither filter is set we skip the lookup entirely
    // so the common search path stays untouched.
    let hits = if kind_filter.is_some() || done_filter.is_some() {
        let lex = LexicalIndex::open_reader_only(&home)
            .lex_context("open lexical index for meta filter")?;
        let mut kept: Vec<DisplayHit> = Vec::with_capacity(hits.len());
        for h in hits.into_iter() {
            let meta = lex
                .meta_for(&h.event_id)
                .context("meta_for hit during kind/done filter")?
                .unwrap_or_default();
            if let Some(k) = &kind_filter {
                let hit_kind = crate::kind::Kind::from(meta.kind.clone());
                if &hit_kind != k {
                    continue;
                }
            }
            if let Some(want) = done_filter {
                if meta.done != want {
                    continue;
                }
            }
            kept.push(h);
        }
        kept
    } else {
        hits
    };

    let mut out = io::stdout().lock();
    write_output(&mut out, query, effective_mode, &hits, as_json)
}

/// Emit the "embedder unavailable" WARN at most once per process.
///
/// Field-feedback fix (P1, 2026-06-04): without this, an agent
/// running `localmem search` repeatedly on a model-less install
/// floods stderr and contaminates `--json` callers. The miss itself
/// is real and worth reporting once, but every subsequent attempt
/// only logs at TRACE so JSON pipes stay clean.
fn warn_embedder_missing_once(model_dir: &Path, err: &anyhow::Error) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        tracing::trace!(
            model_dir = %model_dir.display(),
            error = %err,
            "embedder unavailable (already warned this process); search degraded to lex"
        );
        return;
    }
    tracing::warn!(
        model_dir = %model_dir.display(),
        error = %err,
        "embedder unavailable; search degraded to lex-only. \
         Run `localmem fetch-model` to enable hybrid search, then `localmem replay` to backfill vectors."
    );
}

#[derive(Debug, Serialize)]
struct JsonOutput<'a> {
    query: &'a str,
    mode: &'a str,
    hits: &'a [DisplayHit],
}

fn write_output<W: Write>(
    out: &mut W,
    query: &str,
    mode: Mode,
    hits: &[DisplayHit],
    as_json: bool,
) -> Result<()> {
    if as_json {
        let payload = JsonOutput {
            query,
            mode: mode.as_str(),
            hits,
        };
        serde_json::to_writer(&mut *out, &payload).context("serialize search output as JSON")?;
        out.write_all(b"\n").context("write JSON newline")?;
    } else {
        write_human(out, query, hits)?;
    }
    Ok(())
}

fn write_human<W: Write>(out: &mut W, query: &str, hits: &[DisplayHit]) -> Result<()> {
    if hits.is_empty() {
        writeln!(out, "no results for {query:?}").context("write empty result line")?;
        return Ok(());
    }
    for (i, h) in hits.iter().enumerate() {
        // Append valid_from when present so the temporal envelope is visible at
        // a glance; omit it cleanly for sources that carry no instant.
        let when = match &h.valid_from {
            Some(vf) => format!(" valid_from={vf}"),
            None => String::new(),
        };
        writeln!(
            out,
            "[{}] {} score={:.3} id={}{}",
            i + 1,
            h.snippet,
            h.score,
            h.event_id,
            when,
        )
        .context("write result line")?;
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

/// Resolve where the BGE-small ONNX assets live. `LOCALMEM_MODEL_DIR` wins
/// when set so packagers and CI can point at a system-managed copy; the
/// default `<home>/models/bge-small-en-v1.5/` matches the install layout
/// from T-31.
fn resolve_model_dir(home: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var("LOCALMEM_MODEL_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home.join("models").join("bge-small-en-v1.5")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CapturePayload, Event, EventKind, Source};
    use serde_json::{Map, Value};
    use tempfile::tempdir;

    fn capture(text: &str) -> Event {
        Event::new(
            EventKind::Capture(CapturePayload {
                time: None,
                text: text.into(),
                rewritten_text: None,
                kind: Default::default(),
                mime: None,
                attachments: vec![],
                tags: Default::default(),
                extra: Map::new(),
            }),
            Source {
                app: "test".into(),
                host: "test-host".into(),
                user: None,
            },
        )
    }

    #[test]
    fn resolve_home_uses_override() {
        let path = resolve_home(Some("/tmp/localmem-x")).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/localmem-x"));
    }

    #[test]
    fn resolve_home_falls_back_to_home_dot_localmem() {
        // We do not assume HOME is set in the test environment, so set it
        // explicitly. This isolates the test from the developer's shell.
        // Using a non-real path is safe: resolve_home never touches disk.
        std::env::set_var("HOME", "/tmp/fake-home-for-test");
        let path = resolve_home(None).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/fake-home-for-test/.localmem"));
    }

    #[test]
    fn resolve_model_dir_env_override_and_fallback() {
        // Combined into one test because cargo runs unit tests in parallel
        // by default and `std::env::set_var` is process-wide: two separate
        // tests would race on `LOCALMEM_MODEL_DIR`. The two assertions are
        // ordered so the env state is fully restored on exit.
        std::env::set_var("LOCALMEM_MODEL_DIR", "/opt/bge-small");
        let dir = resolve_model_dir(Path::new("/home/x/.localmem"));
        assert_eq!(dir, PathBuf::from("/opt/bge-small"));

        std::env::remove_var("LOCALMEM_MODEL_DIR");
        let dir = resolve_model_dir(Path::new("/home/x/.localmem"));
        assert_eq!(
            dir,
            PathBuf::from("/home/x/.localmem/models/bge-small-en-v1.5")
        );
    }

    #[test]
    fn write_human_empty_says_no_results() {
        let mut buf = Vec::new();
        write_human(&mut buf, "rust webhooks", &[]).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("no results"));
        assert!(s.contains("rust webhooks"));
    }

    #[test]
    fn write_human_formats_each_hit() {
        let hits = vec![
            DisplayHit {
                event_id: "01HX1".into(),
                snippet: "first hit".into(),
                score: 1.25,
                sources: vec![],
                valid_from: Some("2023-01-15T10:00:00.000Z".into()),
            },
            DisplayHit {
                event_id: "01HX2".into(),
                snippet: "second hit".into(),
                score: 0.5,
                sources: vec!["lex".to_string(), "vec".to_string()],
                valid_from: None,
            },
        ];
        let mut buf = Vec::new();
        write_human(&mut buf, "q", &hits).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("[1] first hit"));
        assert!(s.contains("score=1.250"));
        assert!(s.contains("id=01HX1"));
        assert!(s.contains("[2] second hit"));
        assert!(s.contains("id=01HX2"));
        // valid_from is appended when present and omitted otherwise.
        assert!(s.contains("valid_from=2023-01-15T10:00:00.000Z"));
        assert!(!s.contains("[2] second hit score=0.500 id=01HX2 valid_from"));
    }

    #[test]
    fn write_output_json_shape_matches_contract() {
        let hits = vec![DisplayHit {
            event_id: "01HX1".into(),
            snippet: "hit".into(),
            score: 0.7,
            sources: vec!["lex".to_string(), "vec".to_string()],
            valid_from: None,
        }];
        let mut buf = Vec::new();
        write_output(&mut buf, "q", Mode::Hybrid, &hits, true).unwrap();
        let json: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(json["query"], "q");
        assert_eq!(json["mode"], "hybrid");
        let hits_json = json["hits"].as_array().unwrap();
        assert_eq!(hits_json.len(), 1);
        assert_eq!(hits_json[0]["event_id"], "01HX1");
        assert_eq!(hits_json[0]["snippet"], "hit");
        let sources = hits_json[0]["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 2);
    }

    #[test]
    fn json_output_omits_empty_sources_for_lex_only_hit() {
        // Lex-only hits have no source list. The JSON shape should drop
        // the field rather than emit an empty array, so downstream callers
        // can tell at a glance that the hit came from the lex-only path.
        let hits = vec![DisplayHit {
            event_id: "01HX1".into(),
            snippet: "hit".into(),
            score: 0.7,
            sources: vec![],
            valid_from: None,
        }];
        let mut buf = Vec::new();
        write_output(&mut buf, "q", Mode::Lex, &hits, true).unwrap();
        let json: Value = serde_json::from_slice(&buf).unwrap();
        assert!(json["hits"][0].get("sources").is_none());
    }

    #[tokio::test]
    async fn run_lex_returns_hits_from_indexed_corpus() {
        // End-to-end through `run()`: build an index under a tempdir, drop
        // the writer lock, then invoke run() with --mode lex.
        let tmp = tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let ev = capture("stripe webhook signature verification");
        {
            let mut idx = LexicalIndex::open(&home).unwrap();
            idx.index_event(&ev).unwrap();
            idx.commit().unwrap();
        }
        {
            let idx = LexicalIndex::open(&home).unwrap();
            let hits = idx.search("stripe webhook", 10, None).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].event_id, ev.id.to_string());
        }
        run(
            home.to_str(),
            "stripe webhook",
            10,
            Mode::Lex,
            None,
            BTreeMap::new(),
            None,
            None,
            None,
            /* as_json = */ true,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_lex_with_tags_filters_results() {
        // T-51: --tags applied at the lex layer drops hits whose
        // captures don't carry the requested tag.
        let tmp = tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let mut tagged = capture("stripe webhook signature verification");
        let mut untagged = capture("stripe webhook signature verification");
        if let EventKind::Capture(ref mut p) = tagged.kind {
            p.tags.insert("project".into(), "localmem".into());
        }
        if let EventKind::Capture(ref mut p) = untagged.kind {
            // Different project; should be filtered out.
            p.tags.insert("project".into(), "other".into());
        }
        {
            let mut idx = LexicalIndex::open(&home).unwrap();
            idx.index_event(&tagged).unwrap();
            idx.index_event(&untagged).unwrap();
            idx.commit().unwrap();
        }
        let mut filter = BTreeMap::new();
        filter.insert("project".into(), "localmem".into());
        run(
            home.to_str(),
            "stripe webhook",
            10,
            Mode::Lex,
            None,
            filter,
            None,
            None,
            None,
            true,
        )
        .await
        .unwrap();
        // The integration test asserts the run completes; the filter
        // semantics themselves are covered by direct lex tests under
        // `lexical::tests::search_with_tag_filter_returns_only_matching_captures`.
    }

    #[tokio::test]
    async fn run_hybrid_mode_degrades_to_lex_when_model_missing() {
        // Field-feedback fix (P1, 2026-06-04): with no model present
        // and Mode::Hybrid (the default), search must NOT dead-end. It
        // degrades to lex with a one-line WARN and returns Ok. Hybrid
        // was previously erroring, breaking `localmem search` out of
        // the box on any install that hadn't fetched the model yet.
        let tmp = tempdir().unwrap();
        std::env::set_var("LOCALMEM_MODEL_DIR", tmp.path().join("definitely-not-here"));
        let result = run(
            tmp.path().to_str(),
            "anything",
            10,
            Mode::Hybrid,
            None,
            BTreeMap::new(),
            None,
            None,
            None,
            false,
        )
        .await;
        std::env::remove_var("LOCALMEM_MODEL_DIR");
        assert!(
            result.is_ok(),
            "Mode::Hybrid must degrade to lex on missing embedder, got error: {result:?}"
        );
    }

    #[tokio::test]
    async fn run_vec_mode_still_errors_when_model_missing() {
        // The companion contract: `--mode vec` is an explicit user
        // request for vector-only retrieval. Silently degrading would
        // be dishonest; we keep the error so the user knows their
        // explicit choice failed.
        let tmp = tempdir().unwrap();
        std::env::set_var("LOCALMEM_MODEL_DIR", tmp.path().join("definitely-not-here"));
        let err = run(
            tmp.path().to_str(),
            "anything",
            10,
            Mode::Vec,
            None,
            BTreeMap::new(),
            None,
            None,
            None,
            false,
        )
        .await
        .expect_err("Mode::Vec must surface the missing-model error");
        std::env::remove_var("LOCALMEM_MODEL_DIR");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("load embedder") || msg.contains("missing model"),
            "expected a clear embedder load error, got: {msg}"
        );
        // The new error message points at the lex escape hatch.
        assert!(
            msg.contains("--mode lex") || msg.contains("LOCALMEM_MODEL_DIR"),
            "vec error must suggest the lex fallback or env override; got: {msg}"
        );
    }
}
