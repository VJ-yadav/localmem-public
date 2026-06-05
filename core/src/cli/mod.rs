//! CLI subcommand handlers.
//!
//! Each clap subcommand in `main.rs` delegates to a handler here. Keeping
//! handlers out of `main.rs` lets us test them as ordinary library code.

pub mod doctor;
pub mod export;
pub mod forget;
pub mod init;
pub mod journal;
pub mod mcp;
pub mod mcp_clients;
pub mod profile;
pub mod recall;
pub mod reindex;
pub mod replay;
pub mod audit;
pub mod fetch_model;
pub mod import_wizard;
pub mod recent;
pub mod search;
pub mod subjects;
pub mod summarize;
pub mod tag_arg;
pub mod tags;
pub mod todo;
pub mod write;
