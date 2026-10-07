//! Records the git revision this binary was built from, when it can be known.
//!
//! `RUBIXDB_GIT_REVISION` is set only if `git` is available and the crate is inside a work
//! tree; otherwise it is left unset and `GET /v1/observability/version` reports `null`
//! (a revision is never invented). A work tree with any uncommitted change to a tracked file gets a
//! `-dirty` suffix, and the script is re-run whenever HEAD, the index or any tracked file changes, so a
//! stale clean revision cannot outlive a modification (see `watch_repository`). Only the
//! abbreviated commit hash is embedded: no path, user or host name.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim().to_string())
}

/// Tells cargo when this script must run again. A revision that is only correct when the script
/// happens to be re-run is the hazard this guards against: a binary built from a modified tree must
/// say `-dirty`, a binary built from a clean committed tree must say the bare hash, whichever way the
/// tree got there. So the script re-runs when
///
/// * `HEAD`, the ref it points at, `packed-refs` or the index change (a commit, a checkout, a stage), and
/// * any tracked file changes (a modification, or a revert of one).
///
/// Untracked files are not an input (`git status --untracked-files=no` below), so they are not watched.
fn watch_repository() {
    let Some(git_dir) = git(&["rev-parse", "--git-dir"]) else {
        return;
    };
    let git_dir = std::path::PathBuf::from(git_dir);
    for f in ["HEAD", "index", "packed-refs"] {
        println!("cargo:rerun-if-changed={}", git_dir.join(f).display());
    }
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed={}", git_dir.join(r).display());
    }
    // Tracked files, as paths relative to the work tree root; cargo resolves relative
    // `rerun-if-changed` paths against this crate's directory, so anchor them at the root.
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        return;
    };
    let top = std::path::PathBuf::from(top);
    let Ok(out) = Command::new("git").args(["ls-files", "-z"]).output() else {
        return;
    };
    if !out.status.success() {
        return;
    }
    for rel in out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        if let Ok(rel) = std::str::from_utf8(rel) {
            println!("cargo:rerun-if-changed={}", top.join(rel).display());
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUBIXDB_GIT_REVISION_OVERRIDE");
    // An explicit override (e.g. a source tarball build with no .git) wins; it must be a
    // plain hex hash, anything else is ignored.
    if let Ok(v) = std::env::var("RUBIXDB_GIT_REVISION_OVERRIDE") {
        if !v.is_empty() && v.len() <= 40 && v.bytes().all(|b| b.is_ascii_hexdigit()) {
            println!("cargo:rustc-env=RUBIXDB_GIT_REVISION={v}");
            emit_profile();
            return;
        }
    }
    watch_repository();
    if let Some(rev) = git(&["rev-parse", "--short=12", "HEAD"]) {
        if !rev.is_empty() && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
            // Any tracked modification, staged or not, makes the tree dirty.
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            let v = if dirty { format!("{rev}-dirty") } else { rev };
            println!("cargo:rustc-env=RUBIXDB_GIT_REVISION={v}");
        }
    }
    emit_profile();
}

fn emit_profile() {
    if let Ok(p) = std::env::var("PROFILE") {
        println!("cargo:rustc-env=RUBIXDB_BUILD_PROFILE={p}");
    }
}
