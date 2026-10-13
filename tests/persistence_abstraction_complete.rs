//! ADR-PERSISTENCE-TRAIT-01 section 5: completeness of the persistence layer is enforced, not asserted.
//!
//! Every durable byte and every namespace change of the engine and of `ops` must go through `crate::persist`, otherwise a
//! simulated power loss could silently skip it. This test reads the PRODUCTION part of every source file under `src/`
//! (everything before the first inline `#[cfg(test)] mod ...tests {`; comments stripped) and fails on any direct filesystem
//! access outside `src/persist/`:
//!
//! * the path `std::fs` (any item);
//! * `Path` methods that touch the disk: `.exists()`, `.canonicalize()`, and `.is_dir()` / `.is_file()` called on a path
//!   expression (`x.join(..)`, `Path::new(..)`, or one of the usual path variable names).
//!
//! It is a textual guard with the limits of one: it cannot see a path method called through an unusual variable name. The
//! stronger evidence is dynamic - the crash-injection cycle runs the engine on a virtual root that does not exist on the real
//! disk, so any leaked direct call fails there with `NotFound`.
//!
//! Runs in the default `cargo test` (no feature needed).

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn is_test_support(p: &Path) -> bool {
    let n = p.file_name().unwrap().to_string_lossy().to_string();
    n.ends_with("tests.rs")
        || n.ends_with("_tests.rs")
        || n == "testing.rs"
        || n.starts_with("fuzz")
        || n == "test_support.rs"
}

/// The production lines of a file: up to the first inline `#[cfg(test)] mod <...>tests {`, comments removed.
fn production_lines(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut cut = lines.len();
    for i in 0..lines.len() {
        if lines[i].trim().starts_with("#[cfg(test)]") {
            if let Some(next) = lines.get(i + 1) {
                let n = next.trim();
                let n = n
                    .strip_prefix("pub(crate) ")
                    .or_else(|| n.strip_prefix("pub "))
                    .unwrap_or(n);
                if let Some(rest) = n.strip_prefix("mod ") {
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if (name.ends_with("tests") || name.ends_with("test"))
                        && rest[name.len()..].trim_start().starts_with('{')
                    {
                        cut = i;
                        break;
                    }
                }
            }
        }
    }
    lines[..cut]
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let code = match l.find("//") {
                Some(ix) => &l[..ix],
                None => l,
            };
            (i + 1, code.to_string())
        })
        .collect()
}

const PATH_NAMES: [&str; 14] = [
    "dir", "path", "dest", "p", "data_dir", "sst_dir", "parent", "marker", "src", "dst", "to",
    "from", "root", "wal_dir",
];

fn path_predicate_violation(code: &str, call: &str) -> bool {
    let mut from = 0;
    while let Some(ix) = code[from..].find(call) {
        let at = from + ix;
        let before = code[..at].trim_end();
        // `x.join(..)` / `Path::new(..)` / `f(..)` ending in `)` whose callee is join/new: a path expression
        if before.ends_with(')') {
            if let Some(open) = before.rfind('(') {
                let callee = before[..open].trim_end();
                if callee.ends_with(".join")
                    || callee.ends_with("Path::new")
                    || callee.ends_with("PathBuf::from")
                {
                    return true;
                }
            }
        }
        let ident: String = before
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if PATH_NAMES.contains(&ident.as_str()) {
            return true;
        }
        from = at + call.len();
    }
    false
}

#[test]
fn no_production_code_outside_persist_touches_the_filesystem_directly() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(
        files.len() > 50,
        "the scan found suspiciously few source files: {}",
        files.len()
    );
    let persist = src.join("persist");
    let mut violations: Vec<String> = Vec::new();
    let mut scanned = 0;
    for f in files {
        // `faults.rs` and `startup_probe.rs` write the TEST HARNESS's own files (execution log, ready marker, probe marker),
        // which must stay on the real disk while the database under test is simulated; they hold no database data.
        let harness_output = f.ends_with("faults.rs") || f.ends_with("startup_probe.rs");
        if f.starts_with(&persist) || is_test_support(&f) || harness_output {
            continue;
        }
        let text = fs::read_to_string(&f).unwrap();
        scanned += 1;
        for (n, code) in production_lines(&text) {
            let rel = f.strip_prefix(&src).unwrap().display().to_string();
            if code.contains("std::fs") {
                violations.push(format!(
                    "{rel}:{n}: `std::fs` outside src/persist: {}",
                    code.trim()
                ));
            }
            if code.contains(".exists()") {
                violations.push(format!(
                    "{rel}:{n}: `.exists()` (use persist::exists): {}",
                    code.trim()
                ));
            }
            if code.contains(".canonicalize()") {
                violations.push(format!(
                    "{rel}:{n}: `.canonicalize()` (use persist::canonicalize): {}",
                    code.trim()
                ));
            }
            for call in [".is_dir()", ".is_file()"] {
                if path_predicate_violation(&code, call) {
                    violations.push(format!(
                        "{rel}:{n}: `{call}` on a path (use persist::is_dir / is_file): {}",
                        code.trim()
                    ));
                }
            }
        }
    }
    assert!(scanned > 40, "scanned only {scanned} production files");
    assert!(
        violations.is_empty(),
        "{} direct filesystem access(es) outside src/persist:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

/// The scan itself must be able to fail: each of these lines is a violation, each of those is allowed.
#[test]
fn the_scan_flags_what_it_must_and_allows_what_it_must() {
    for bad in [
        "if dir.join(\"wal\").is_dir() {",
        "if !data_dir.is_dir() {",
        "if Path::new(\"x\").is_file() {",
        "let ok = dest.exists();",
        "let c = dir.canonicalize()?;",
    ] {
        let hit = bad.contains(".exists()")
            || bad.contains(".canonicalize()")
            || path_predicate_violation(bad, ".is_dir()")
            || path_predicate_violation(bad, ".is_file()");
        assert!(hit, "not flagged: {bad}");
    }
    for ok in [
        "if entry.file_type()?.is_file() {",
        "if meta.is_dir() {",
        "let m = crate::persist::metadata(&p)?;",
    ] {
        let hit = ok.contains(".exists()")
            || ok.contains(".canonicalize()")
            || path_predicate_violation(ok, ".is_dir()")
            || path_predicate_violation(ok, ".is_file()");
        assert!(!hit, "wrongly flagged: {ok}");
    }
}
