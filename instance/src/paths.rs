//! Instance directory layout and name validation.
//!
//! `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md` §2. Every instance name is
//! restricted to a small ASCII charset before it is ever joined onto a
//! filesystem path -- this is the entire path-traversal defense (item
//! 41's "database/schema/table identifiers must never become arbitrary
//! filesystem paths," applied here to instance names): `/`, `\`, `..`,
//! drive letters, and UNC prefixes are all excluded by construction,
//! not by a blocklist.

use std::env;
use std::path::PathBuf;

pub const APP_DIR_NAME: &str = "rubiXDb";
pub const DEFAULT_INSTANCE_NAME: &str = "default";
const MAX_NAME_LEN: usize = 64;

/// Per-OS application-data root -- the same convention every desktop
/// product on each platform uses, so the instance directory lands
/// somewhere a normal user/admin already expects local app state to
/// live, with the OS's own per-user ACLs already applied (no new
/// permission model invented here).
pub fn app_data_dir() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        let base = env::var_os("LOCALAPPDATA")
            .or_else(|| env::var_os("APPDATA"))
            .ok_or_else(|| "neither LOCALAPPDATA nor APPDATA is set".to_string())?;
        Ok(PathBuf::from(base).join(APP_DIR_NAME))
    }
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join(APP_DIR_NAME))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = env::var_os("XDG_DATA_HOME") {
            if !xdg.is_empty() {
                return Ok(PathBuf::from(xdg).join(APP_DIR_NAME));
            }
        }
        let home = env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join(APP_DIR_NAME))
    }
}

/// Test/override hook: `RUBIXDB_INSTANCES_ROOT` takes priority over the
/// platform default when set, so integration tests never touch the
/// real developer machine's app-data directory.
pub fn instances_root() -> Result<PathBuf, String> {
    instances_root_with_override(env::var_os("RUBIXDB_INSTANCES_ROOT"))
}

/// Pure form of [`instances_root`], taking the override value directly
/// instead of reading the process environment -- used by unit tests so
/// they never mutate shared process-wide env state (`std::env::set_var`
/// across parallel test threads is a real race; this sidesteps it
/// entirely rather than serializing tests around it).
fn instances_root_with_override(
    override_value: Option<std::ffi::OsString>,
) -> Result<PathBuf, String> {
    if let Some(root) = override_value {
        if !root.is_empty() {
            return Ok(PathBuf::from(root));
        }
    }
    Ok(app_data_dir()?.join("instances"))
}

pub fn validate_instance_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        return Err(format!("instance name must be 1-{MAX_NAME_LEN} characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("instance name may contain only ASCII letters, digits, '-', '_'".to_string());
    }
    Ok(())
}

pub fn instance_dir(name: &str) -> Result<PathBuf, String> {
    instance_dir_under(&instances_root()?, name)
}

/// Pure form of [`instance_dir`] taking the root explicitly -- see
/// [`instances_root_with_override`] for why unit tests use this instead
/// of `std::env::set_var`.
fn instance_dir_under(root: &std::path::Path, name: &str) -> Result<PathBuf, String> {
    validate_instance_name(name)?;
    Ok(root.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal_names() {
        for bad in ["..", ".", "../x", "a/../../b", "a\\b", "a/b", "", "C:\\x"] {
            assert!(
                validate_instance_name(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn accepts_normal_names() {
        for good in ["default", "test-1", "my_instance", "A1"] {
            assert!(
                validate_instance_name(good).is_ok(),
                "expected {good:?} to be accepted"
            );
        }
    }

    #[test]
    fn rejects_overlong_name() {
        let long = "a".repeat(65);
        assert!(validate_instance_name(&long).is_err());
    }

    #[test]
    fn instance_dir_is_confined_under_root() {
        let root = PathBuf::from("/tmp/rubixdb-test-root");
        let dir = instance_dir_under(&root, "default").unwrap();
        assert_eq!(dir, root.join("default"));
    }

    #[test]
    fn instance_dir_rejects_traversal_even_with_root_override() {
        let root = PathBuf::from("/tmp/rubixdb-test-root");
        assert!(instance_dir_under(&root, "../escape").is_err());
    }

    #[test]
    fn instances_root_with_override_prefers_override() {
        let root = instances_root_with_override(Some(std::ffi::OsString::from("/tmp/x"))).unwrap();
        assert_eq!(root, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn instances_root_with_override_ignores_empty_override() {
        // An empty env var (unset in effect) must fall through to the
        // real platform default rather than resolving to "".
        let root = instances_root_with_override(Some(std::ffi::OsString::new()));
        assert!(root.is_ok());
        assert_ne!(root.unwrap(), PathBuf::new());
    }
}
