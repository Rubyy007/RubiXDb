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
        // Also on the cached path: a staging folder left by a process that was killed
        // mid-unpack is only ever visited here once the folder for this build exists.
        prune_stale(
            &parent,
            hash,
            std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60),
        );
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
    prune_stale(
        &parent,
        hash,
        std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60),
    );
    Some(target)
}

/// Best effort: removes every entry of `parent` other than the folder named exactly
/// `hash` that was last modified before `cutoff` -- folders of other builds AND
/// `<hash>.tmp-<pid>` staging folders (of any build, including this one) that a killed
/// process left behind. Entries newer than `cutoff` are kept, so a still-running older
/// binary never loses the folder it is serving from and a process that is unpacking right
/// now keeps its staging folder.
pub(crate) fn prune_stale(parent: &Path, hash: &str, cutoff: std::time::SystemTime) {
    let Ok(rd) = fs::read_dir(parent) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == hash {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < cutoff);
        if stale {
            let _ = fs::remove_dir_all(e.path());
        }
    }
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
    fn prune_removes_stale_staging_folders_of_any_build_but_never_the_current_folder() {
        let root = tmp("prune");
        let parent = root.join("frontend");
        for d in ["cur", "cur.tmp-111", "old", "old.tmp-222"] {
            fs::create_dir_all(parent.join(d)).unwrap();
        }
        let day = std::time::Duration::from_secs(24 * 60 * 60);
        let now = std::time::SystemTime::now();
        // Everything is brand new: a one-day cutoff in the past keeps every entry.
        prune_stale(&parent, "cur", now - day);
        assert!(parent.join("cur.tmp-111").is_dir() && parent.join("old").is_dir());
        // A cutoff in the future makes everything stale: all but the current folder go.
        prune_stale(&parent, "cur", now + day);
        assert!(
            parent.join("cur").is_dir(),
            "the current build's folder is never pruned"
        );
        for d in ["cur.tmp-111", "old", "old.tmp-222"] {
            assert!(!parent.join(d).exists(), "{d} should have been pruned");
        }
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
