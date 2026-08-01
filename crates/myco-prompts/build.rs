//! Stamps the build's git commit into `MYCO_GIT_COMMIT`.
//!
//! The commit keys the on-disk manual directory
//! (`~/.myco/manual/<version>/<commit>/`, see `src/manual`), so two builds of
//! the same package version cannot overwrite each other's articles. Runs
//! `git` locally and nothing else: builds stay offline, and a source tree
//! without git history (a crates.io tarball) stamps `unknown` rather than
//! failing the build.

use std::path::Path;
use std::process::Command;

fn main() {
    let commit = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo::rustc-env=MYCO_GIT_COMMIT={commit}");

    // Re-stamp when HEAD moves. `--git-path` resolves both files through the
    // real git dir, so a linked worktree (feature work lives in one) watches
    // its own HEAD rather than the main checkout's.
    rerun_if_exists(git(&["rev-parse", "--git-path", "HEAD"]).as_deref());
    let head_ref = git(&["symbolic-ref", "--quiet", "HEAD"]);
    if let Some(head_ref) = head_ref {
        rerun_if_exists(git(&["rev-parse", "--git-path", &head_ref]).as_deref());
    }
}

/// Trimmed stdout of a successful `git` run; `None` when git is missing, the
/// tree has no history, or the command fails.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Watch `path` for changes. Skips missing paths: a packed ref has no file of
/// its own, and naming one would rebuild the crate on every invocation.
fn rerun_if_exists(path: Option<&str>) {
    if let Some(path) = path
        && Path::new(path).exists()
    {
        println!("cargo::rerun-if-changed={path}");
    }
}
