//! `credentials.json` -- the one local-mode admin API key, generated
//! once per instance and read by both the owning `gui` process and any
//! attaching `cli` process. `PHASE_RUBIXDB_INSTANCE_SECURITY.md` §2.
//!
//! This does not weaken the existing API auth model
//! (`api/src/config.rs::parse_api_keys` still requires a real key of
//! at least 16 characters) -- it only changes *how* the human obtains
//! that key: instead of typing/pasting one, local mode generates a
//! high-entropy key once and distributes it via the filesystem, scoped
//! by the OS's own per-user permissions, exactly like SSH host keys or
//! a local Postgres `.pgpass`. "No login ceremony" describes the UX;
//! the wire protocol underneath is unchanged.

use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

const FILE_NAME: &str = "credentials.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceCredentials {
    pub admin_key: String,
}

impl InstanceCredentials {
    /// 64 hex characters from two independent `Uuid::new_v4()` draws --
    /// 256 bits of CSPRNG entropy (the same `getrandom`-backed
    /// generator already used for SQL session ids,
    /// `PHASE_RELATIONAL_SQL_API_ARCHITECTURE.md` §3), comfortably
    /// above the API's own 16-character minimum and never derived from
    /// any guessable/enumerable source (never a table id, never a
    /// counter).
    pub fn generate() -> Self {
        let key = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        InstanceCredentials { admin_key: key }
    }

    pub fn load(dir: &Path) -> std::io::Result<Option<Self>> {
        let path = dir.join(FILE_NAME);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let c: Self = serde_json::from_slice(&bytes)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                Ok(Some(c))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let tmp = dir.join(format!("{FILE_NAME}.tmp"));
        let bytes = serde_json::to_vec(self).expect("InstanceCredentials always serializes");
        std::fs::write(&tmp, &bytes)?;
        restrict_permissions(&tmp)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Windows has no POSIX-mode-bit equivalent in `std`; the accepted
/// boundary (documented, not silently assumed) is the per-user ACL
/// `%LOCALAPPDATA%` already carries -- other OS-user accounts cannot
/// read it by default. See `PHASE_RUBIXDB_INSTANCE_SECURITY.md` §2 for
/// the full accepted-risk statement.
#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_high_entropy_and_unique() {
        let a = InstanceCredentials::generate();
        let b = InstanceCredentials::generate();
        assert_eq!(a.admin_key.len(), 64);
        assert!(a.admin_key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.admin_key, b.admin_key);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("rubixdb_creds_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = InstanceCredentials::generate();
        c.save(&dir).unwrap();
        let loaded = InstanceCredentials::load(&dir).unwrap().unwrap();
        assert_eq!(c.admin_key, loaded.admin_key);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rubixdb_creds_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        InstanceCredentials::generate().save(&dir).unwrap();
        let mode = std::fs::metadata(dir.join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }
}
