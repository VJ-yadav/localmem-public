//! localmem core library.
//!
//! All domain logic (event log, storage, retrieval, policy) lives here so
//! that internal tests count as legitimate consumers of the public API.
//! The `localmem` binary in `src/main.rs` is the thin CLI wrapper.

// Foundation (shipped: T-01 through T-04).
pub mod config; //       T-46: `<home>/config.toml` loader
pub mod event;
pub mod event_id;
pub mod event_log;

// Phase 2 stubs (parallel groups fill these in via worktrees).
// See TASKS.md "Parallel groups" + ROADMAP.md.
pub mod cli; //          Groups A (search) + D (journal)
pub mod embed; //        Group B: ONNX embedder (T-08)
pub mod extractor; //    Group C: rule-based fact extractor (T-13)
pub mod facts; //        Group C: DuckDB bitemporal facts (T-11 to T-12)
pub mod import; //       Polish: T-32 (chatgpt), T-33 (claude) bulk importers
pub mod indexer; //      Group B + C: routes events to derived stores (T-10, T-14)
pub mod journal; //      Group D: policy decision log (T-17)
pub mod kind; //         Phase 5B: kind taxonomy (T-52)
pub mod lexical; //      Group A: Tantivy BM25 index (T-05 to T-06)
pub mod policy; //       Group D: write policy engine (T-15 to T-16)
pub mod reserved_tags; // Phase 5B: reserved-tag semantics (T-51c)
pub mod retriever; //    Integration: hybrid retriever (T-23)
pub mod rewriter; //     Phase 5C: context rewriting at ingest (T-55)
pub mod server; //       Group E: local axum HTTP server (T-19 to T-22)
pub mod tag_match; //    Phase 5B: shared tag subset-match (T-51b)
pub mod vectors; //      Group B: LanceDB ANN index (T-09)
