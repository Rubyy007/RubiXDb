//! Locates the built frontend (`frontend/dist` after `npm run build`)
//! relative to the running executable, so a packaged release can ship
//! the static build alongside `rubixdb.exe` without any install-time
//! configuration. `PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §3.

use std::path::PathBuf;

/// Checked in order; the first candidate containing a real
/// `index.html` wins. `RUBIXDB_FRONTEND_DIST` is an explicit override
/// (checked first) for non-standard layouts / development.
pub fn resolve() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("RUBIXDB_FRONTEND_DIST") {
        if !raw.is_empty() {
            let candidate = PathBuf::from(raw);
            if is_valid(&candidate) {
                return Some(candidate);
            }
        }
    }

    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?.to_path_buf();

    let relative_candidates: &[&str] = &[
        // Production packaging: dist copied next to the executable.
        "frontend-dist",
        // Development: running `target/debug/rubixdb.exe` straight out
        // of a workspace checkout, dist built in place under
        // `frontend/`.
        "../../frontend/dist",
        "../../../frontend/dist",
    ];
    for rel in relative_candidates {
        let candidate = exe_dir.join(rel);
        if is_valid(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_valid(dir: &std::path::Path) -> bool {
    dir.join("index.html").is_file()
}
