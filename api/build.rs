//! Records the git revision this binary was built from, when it can be known.
//!
//! `RUBIXDB_GIT_REVISION` is set only if `git` is available and the crate is inside a work
//! tree; otherwise it is left unset and `GET /v1/observability/version` reports `null`
//! (a revision is never invented). A work tree with uncommitted changes gets a `-dirty`
//! suffix. Only the abbreviated commit hash is embedded: no path, user or host name.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
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
    if let Some(rev) = git(&["rev-parse", "--short=12", "HEAD"]) {
        if !rev.is_empty() && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
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
