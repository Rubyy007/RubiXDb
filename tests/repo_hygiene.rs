//! Repository hygiene gate (Phase 7 security gap SG-1): no credential or key
//! material may be tracked by git. Reads `git ls-files` of the current
//! checkout; it inspects only what is committed/staged, never the working
//! tree's ignored files. See `PHASE_RUBIXDB_SECURITY_GAP_ANALYSIS.md` F-1.
//!
//! Skipped (loudly) only when the sources are not inside a git checkout
//! (e.g. a packaged source tarball), where there is nothing to inspect.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn tracked_files() -> Option<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root())
        .args(["ls-files", "-z"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

/// True if `text` contains `admin_key` followed by optional quote/space, a
/// colon or `=`, optional space/quote, then exactly 64 hex digits.
/// Hand-rolled so the root crate needs no regex dependency.
fn contains_admin_key_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = b"admin_key";
    let mut i = 0;
    while let Some(pos) = find(bytes, needle, i) {
        let mut j = pos + needle.len();
        let skip = |j: &mut usize, set: &[u8]| {
            while *j < bytes.len() && set.contains(&bytes[*j]) {
                *j += 1;
            }
        };
        skip(&mut j, b"\"' \t");
        if j < bytes.len() && (bytes[j] == b':' || bytes[j] == b'=') {
            j += 1;
            skip(&mut j, b"\"' \t");
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
                j += 1;
            }
            if j - start == 64 {
                return true;
            }
        }
        i = pos + needle.len();
    }
    false
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

#[test]
fn no_tracked_credentials_json() {
    let Some(files) = tracked_files() else {
        eprintln!("skipping: not a git checkout");
        return;
    };
    let bad: Vec<_> = files
        .iter()
        .filter(|f| Path::new(f).file_name().and_then(|n| n.to_str()) == Some("credentials.json"))
        .collect();
    assert!(bad.is_empty(), "tracked credentials.json: {bad:?}");
}

#[test]
fn no_tracked_key_or_certificate_files() {
    let Some(files) = tracked_files() else {
        eprintln!("skipping: not a git checkout");
        return;
    };
    let bad: Vec<_> = files
        .iter()
        .filter(|f| {
            let l = f.to_ascii_lowercase();
            [".pem", ".key", ".pfx", ".p12", ".jks"]
                .iter()
                .any(|ext| l.ends_with(ext))
        })
        .collect();
    assert!(bad.is_empty(), "tracked key/certificate files: {bad:?}");
}

#[test]
fn no_tracked_file_contains_a_64_hex_admin_key_literal() {
    let Some(files) = tracked_files() else {
        eprintln!("skipping: not a git checkout");
        return;
    };
    let root = repo_root();
    let mut bad = Vec::new();
    for f in &files {
        let p = root.join(f);
        // Files staged for deletion are listed by `ls-files --cached` only
        // while still in the index; a missing working copy cannot leak.
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        if bytes.len() > 8 * 1024 * 1024 {
            continue;
        }
        if contains_admin_key_literal(&String::from_utf8_lossy(&bytes)) {
            bad.push(f.clone());
        }
    }
    assert!(bad.is_empty(), "64-hex admin_key literal in: {bad:?}");
}

#[test]
fn detector_finds_the_leak_shape_and_ignores_lookalikes() {
    let k = "a".repeat(64);
    assert!(contains_admin_key_literal(&format!(
        "{{\"admin_key\":\"{k}\"}}"
    )));
    assert!(contains_admin_key_literal(&format!("admin_key = '{k}'")));
    // 63 and 65 hex digits are not a 64-hex key.
    assert!(!contains_admin_key_literal(&format!(
        "{{\"admin_key\":\"{}\"}}",
        "a".repeat(63)
    )));
    assert!(!contains_admin_key_literal(&format!(
        "{{\"admin_key\":\"{}\"}}",
        "a".repeat(65)
    )));
    // Field name only, or a non-literal value.
    assert!(!contains_admin_key_literal("pub admin_key: String,"));
    assert!(!contains_admin_key_literal("\"admin_key\": \"<redacted>\""));
}
