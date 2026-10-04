//! The console build that was compiled into this executable (see `build.rs`).
//! At startup it is unpacked once into a content-hashed folder under the
//! per-user app data directory (`<app data>/frontend/<hash>`, deliberately NOT
//! inside the instances root, which holds only instance folders) and served from there, so the UI always matches the binary
//! it ships in -- never a stale `frontend-dist` copy lying next to it.

use std::fs;
use std::path::{Component, Path, PathBuf};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/embedded_frontend.rs"));
}

/// Unpacks the embedded console under `<app_data>/frontend/<hash>/` and returns
/// that folder, or `None` when this binary was built without a frontend.
pub fn extract_under(app_data: &Path) -> Option<PathBuf> {
    extract_files(app_data, generated::HASH, generated::FILES)
}

pub(crate) fn extract_files(root: &Path, hash: &str, files: &[(&str, &[u8])]) -> Option<PathBuf> {
    if !files.iter().any(|(p, _)| *p == "index.html") {
        return None;
    }
    let parent = root.join("frontend");
    let target = parent.join(hash);
    if target.join("index.html").is_file() {
        return Some(target);
    }
    fs::create_dir_all(&parent).ok()?;
    let staging = parent.join(format!("{hash}.tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    for (rel, bytes) in files {
        let rel_path = Path::new(rel);
        // Names come from build.rs, but never write outside the staging folder regardless.
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            let _ = fs::remove_dir_all(&staging);
            return None;
        }
        let dest = staging.join(rel_path);
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir).ok()?;
        }
        fs::write(&dest, bytes).ok()?;
    }
    // Atomic publish. Losing the race to another process that unpacked the same build is fine.
    if fs::rename(&staging, &target).is_err() {
        let _ = fs::remove_dir_all(&staging);
        if !target.join("index.html").is_file() {
            return None;
        }
    }
    // Best effort: drop folders left by other builds -- only ones untouched for a day, so a
    // still-running older binary never loses the folder it is serving from.
    let day = std::time::Duration::from_secs(24 * 60 * 60);
    if let Ok(rd) = fs::read_dir(&parent) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > day);
            if !name.starts_with(hash) && stale {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rbx_embed_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    const FILES: &[(&str, &[u8])] = &[
        ("index.html", b"<html></html>"),
        ("assets/app.js", b"console.log(1)"),
    ];

    #[test]
    fn unpacks_every_file_once_and_reuses_the_folder() {
        let root = tmp("a");
        let dir = extract_files(&root, "h1", FILES).unwrap();
        assert_eq!(fs::read(dir.join("index.html")).unwrap(), b"<html></html>");
        assert_eq!(
            fs::read(dir.join("assets").join("app.js")).unwrap(),
            b"console.log(1)"
        );
        fs::write(dir.join("marker"), b"x").unwrap();
        let again = extract_files(&root, "h1", FILES).unwrap();
        assert_eq!(again, dir);
        assert!(
            again.join("marker").is_file(),
            "an existing complete folder is reused, not rewritten"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_new_build_gets_a_new_folder_and_a_recent_old_one_is_left_alone() {
        let root = tmp("b");
        let old = extract_files(&root, "h1", FILES).unwrap();
        let new = extract_files(&root, "h2", FILES).unwrap();
        assert_ne!(old, new);
        assert!(
            old.exists(),
            "an older build may still be running from its folder"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nothing_is_unpacked_without_an_index_or_with_a_path_that_escapes() {
        let root = tmp("c");
        assert!(extract_files(&root, "h", &[("a.js", b"x")]).is_none());
        assert!(extract_files(&root, "h", &[("index.html", b"x"), ("../evil.js", b"x")]).is_none());
        assert!(extract_files(&root, "h", &[("index.html", b"x"), ("/abs.js", b"x")]).is_none());
        assert!(!root.join("evil.js").exists());
        fs::remove_dir_all(&root).ok();
    }
}
