//! Bake a build identity into the daemon.
//!
//! `Version` reports the crate version, which is the same string for every
//! snapshot build between two releases — so it cannot answer "which build is
//! actually running?". That question matters here: a package upgrade replaces
//! the binary on disk but cannot restart a *user* service, so the running
//! daemon routinely predates the installed one, and nothing said so.
//!
//! Precedence, most trustworthy first:
//!
//! 1. `GAFFER_BUILD_ID` from the environment — what the RPM spec exports, so a
//!    packaged build reports exactly the NVR it was built as.
//! 2. The git commit, for builds from a checkout.
//! 3. `unknown`, for a source tarball with neither. Deliberately not an error:
//!    refusing to build because provenance is unclear would be worse than
//!    building something that admits it does not know.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GAFFER_BUILD_ID");

    let id = std::env::var("GAFFER_BUILD_ID")
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .or_else(from_git)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GAFFER_BUILD_ID={id}");
}

/// `git<short sha>`, with `-dirty` appended when the tree has uncommitted
/// changes — so a build made from edited sources cannot claim to be the commit
/// it was based on.
fn from_git() -> Option<String> {
    let sha = run(&["rev-parse", "--short=12", "HEAD"])?;

    // Re-run when HEAD moves. Only wired up once git has answered, so a build
    // outside a checkout does not declare a dependency on a path that is not
    // there.
    if let Some(dir) = run(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
    }

    // `--porcelain` is empty exactly when the tree is clean.
    let dirty = run(&["status", "--porcelain"]).is_some_and(|out| !out.is_empty());
    Some(if dirty { format!("git{sha}-dirty") } else { format!("git{sha}") })
}

/// Run a git command, or `None` if git is missing, fails, or is not in a repo.
fn run(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
