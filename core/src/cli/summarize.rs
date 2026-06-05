//! `localmem summarize` handler (T-53).
//!
//! Returns a synthesized markdown brief of the user's memory. Same
//! rendering pipeline as `localmem profile` (T-39) but with optional
//! tag + kind filters that scope the brief to a slice of the memory
//! store. Behaviorally a thin wrapper around the existing profile
//! synthesis so the two commands stay in sync; the spec-mandated
//! split is preserved (summarize is the discovery surface, profile
//! is the per-subject synthesis) but they share rendering rules.

use crate::cli::profile;
use crate::kind::Kind;
use anyhow::Result;
use std::collections::BTreeMap;

/// Entry point for the `summarize` subcommand.
///
/// `tags` is the T-51b container-tag filter; an empty map disables
/// it. `kind` restricts the rendered brief to a single closed-core
/// kind (T-52). When both are provided they compose with AND.
pub fn run(
    home: Option<&str>,
    tags: BTreeMap<String, String>,
    kind: Option<Kind>,
    as_json: bool,
) -> Result<()> {
    // Subject scope is intentionally not exposed here: summarize is
    // the "brief on a slice of memory" surface, profile is the
    // "deep-dive on one subject" surface. Spec: `summarize [tag=X]`.
    profile::run_with_kind(home, None, tags, kind, as_json)
}
