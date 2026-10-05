//! Locates the built frontend (`frontend/dist` after `npm run build`)
//! relative to the running executable, so a packaged release can ship
//! the static build alongside `rubixdb.exe` without any install-time
//! configuration. `PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §3.

use std::path::PathBuf;

/// Order: `RUBIXDB_FRONTEND_DIST` (explicit override, for development), then
/// the console compiled into this executable (`embedded_frontend`), then -- only
/// for a binary built without a frontend -- folders next to / above the
/// executable; the first containing a real `index.html` wins. The embedded copy
/// comes before the on-disk ones on purpose: a leftover `frontend-dist` folder
/// must never shadow the UI that ships inside the binary.
pub const ENV: &str = "RUBIXDB_FRONTEND_DIST";

/// `RUBIXDB_FRONTEND_DIST`, validated. Unset or empty = no override. A value
/// that is not a directory holding `index.html` is an error (it used to fall
/// through silently to the embedded console, hiding a typo in the override).
pub fn env_override() -> Result<Option<PathBuf>, String> {
    match std::env::var(ENV) {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{ENV} is not valid Unicode")),
        Ok(raw) if raw.is_empty() => Ok(None),
        Ok(raw) => check_override(&raw).map(Some),
    }
}

pub(crate) fn check_override(raw: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(raw);
    if is_valid(&candidate) {
        Ok(candidate)
    } else {
        Err(format!(
            "{ENV} {raw:?} is not a directory containing index.html (unset it to serve the console embedded in this executable)"
        ))
    }
}

pub fn resolve() -> Result<Option<PathBuf>, String> {
    if let Some(dir) = env_override()? {
        return Ok(Some(dir));
    }
    Ok(resolve_without_override())
}

fn resolve_without_override() -> Option<PathBuf> {
    if let Ok(app_data) = rubixdb_instance::paths::app_data_dir() {
        if let Some(dir) = crate::embedded_frontend::extract_under(&app_data) {
            return Some(dir);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_must_be_a_directory_with_index_html() {
        let dir = std::env::temp_dir().join(format!("rbx_fe_override_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = check_override(dir.to_str().unwrap()).unwrap_err();
        assert!(err.contains(ENV), "{err}");
        assert!(check_override(dir.join("missing").to_str().unwrap()).is_err());
        std::fs::write(dir.join("index.html"), b"<html></html>").unwrap();
        assert_eq!(check_override(dir.to_str().unwrap()).unwrap(), dir);
        // a file is not a directory holding index.html
        assert!(check_override(dir.join("index.html").to_str().unwrap()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
