//! Build script: stamp the git short SHA into the binary so `localmem
//! --version` reports an exact commit, not just a semver. This is the
//! provenance fix for the divergence that let a 0.3.5 binary be debugged
//! against 0.3.3 source. Falls back to "unknown" for source-tarball builds
//! with no git checkout.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=LOCALMEM_GIT_SHA={sha}");

    // Re-run when HEAD moves so the stamped SHA stays current. The git dir is
    // at the repo root, one level above this crate.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
}
