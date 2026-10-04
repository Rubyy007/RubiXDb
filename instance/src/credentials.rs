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
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

const FILE_NAME: &str = "credentials.json";

#[derive(Clone, Serialize, Deserialize)]
pub struct InstanceCredentials {
    pub admin_key: String,
}

/// Phase 7 SG-3a: `{:?}` must never print the key (it appears in panic
/// messages, `assert!` output and any future log line).
impl std::fmt::Debug for InstanceCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceCredentials")
            .field("admin_key", &"<redacted>")
            .finish()
    }
}

impl InstanceCredentials {
    /// 64 hex characters from two independent `Uuid::new_v4()` draws --
    /// ~244 bits of CSPRNG entropy (a v4 UUID carries 122 random bits; the
    /// same `getrandom`-backed generator already used for SQL session ids,
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

    /// Atomically persists the credential: stage -> restrict -> write -> fsync
    /// -> read-back verify -> rename. The permission restriction is applied to
    /// the *empty* staging file, before the secret is written, so the key is
    /// never present in a file with a looser ACL. Fails closed: if the
    /// restriction cannot be applied (or the staged file does not read back
    /// as exactly this credential) nothing is renamed into place, the staging
    /// file is removed, and an existing `credentials.json` is left untouched.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        self.save_with(dir, |_| {})
    }

    /// `before_commit` runs after the staged file is fully written and before
    /// it is verified and renamed -- a test seam for "the staged file was
    /// damaged"; production passes a no-op.
    fn save_with(&self, dir: &Path, before_commit: impl FnOnce(&Path)) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let tmp = dir.join(format!("{FILE_NAME}.tmp"));
        let bytes = serde_json::to_vec(self).expect("InstanceCredentials always serializes");
        let result = (|| {
            // A stale staging file (a previous process killed mid-save) is
            // truncated and re-restricted, never trusted.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            restrict_permissions(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            drop(f);
            before_commit(&tmp);
            verify_staged(&tmp, self)?;
            std::fs::rename(&tmp, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

/// The staged file must parse and carry exactly the key being persisted.
fn verify_staged(tmp: &Path, expected: &InstanceCredentials) -> std::io::Result<()> {
    let bytes = std::fs::read(tmp)?;
    let staged: InstanceCredentials = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if staged.admin_key != expected.admin_key {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "staged credential file failed read-back verification",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Windows: replaces the DACL with owner + SYSTEM only and disables
/// inheritance (`winacl.rs`, SG-2). Supersedes the accepted-risk statement in
/// `PHASE_RUBIXDB_INSTANCE_SECURITY.md` §2 (reliance on the inherited
/// `%LOCALAPPDATA%` ACL) for files written by this version.
#[cfg(windows)]
fn restrict_permissions(path: &Path) -> std::io::Result<()> {
    crate::winacl::restrict_to_owner_and_system(path)
}

/// Platforms with neither POSIX modes nor the Windows ACL path are not
/// supported targets; fail closed rather than silently persist a secret.
#[cfg(not(any(unix, windows)))]
fn restrict_permissions(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "no credential-file permission mechanism on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rubixdb_creds_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn generated_keys_are_high_entropy_and_unique() {
        let a = InstanceCredentials::generate();
        let b = InstanceCredentials::generate();
        assert_eq!(a.admin_key.len(), 64);
        assert!(a.admin_key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.admin_key, b.admin_key);
    }

    #[test]
    fn debug_never_prints_the_key() {
        let c = InstanceCredentials::generate();
        for rendered in [format!("{c:?}"), format!("{c:#?}")] {
            assert!(!rendered.contains(&c.admin_key), "{rendered}");
            assert!(rendered.contains("<redacted>"));
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tmp_dir();
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
        let dir = tmp_dir();
        InstanceCredentials::generate().save(&dir).unwrap();
        let mode = std::fs::metadata(dir.join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn saved_file_dacl_is_owner_and_system_only_without_inheritance() {
        let dir = tmp_dir();
        InstanceCredentials::generate().save(&dir).unwrap();
        let sddl = crate::winacl::dacl_sddl(&dir.join(FILE_NAME)).unwrap();
        // Exactly two ACEs -- SYSTEM full, OWNER RIGHTS full -- in a protected
        // DACL (`P`). `AI` in the readback is the auto-inherited marker the OS
        // adds to the descriptor; no ACE is itself inherited (`ID`), and no
        // other principal (Users, Authenticated Users, Everyone, Administrators)
        // appears.
        assert_eq!(sddl, "D:PAI(A;;FA;;;SY)(A;;FA;;;OW)");
        assert!(!sddl.contains(";ID;"), "{sddl}");
        assert_eq!(sddl.matches("(A;").count(), 2, "{sddl}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Control: a plain file in the same directory inherits, so the
    /// assertion above is not vacuously true on this machine.
    #[cfg(windows)]
    #[test]
    fn control_unrestricted_file_inherits_its_dacl() {
        let dir = tmp_dir();
        let p = dir.join("plain.txt");
        std::fs::write(&p, b"x").unwrap();
        let sddl = crate::winacl::dacl_sddl(&p).unwrap();
        assert!(sddl.contains(";ID;"), "expected inherited ACEs, got {sddl}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Fail closed: when staging cannot happen (a directory squats on the
    /// staging name), no credential is persisted and the existing one is
    /// untouched.
    #[test]
    fn staging_failure_persists_nothing_and_leaves_existing_credential() {
        let dir = tmp_dir();
        let old = InstanceCredentials::generate();
        old.save(&dir).unwrap();
        std::fs::create_dir(dir.join(format!("{FILE_NAME}.tmp"))).unwrap();
        assert!(InstanceCredentials::generate().save(&dir).is_err());
        assert_eq!(
            InstanceCredentials::load(&dir).unwrap().unwrap().admin_key,
            old.admin_key
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Invalid rotation: the staged file is damaged before commit. The
    /// read-back check refuses to rename it; the existing credential is
    /// byte-for-byte untouched and the staging file is cleaned up.
    #[test]
    fn corrupt_staged_file_is_never_committed() {
        let dir = tmp_dir();
        let old = InstanceCredentials::generate();
        old.save(&dir).unwrap();
        let before = std::fs::read(dir.join(FILE_NAME)).unwrap();
        let next = InstanceCredentials::generate();
        let err = next
            .save_with(&dir, |tmp| std::fs::write(tmp, b"{ not json").unwrap())
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(dir.join(FILE_NAME)).unwrap(), before);
        assert!(!dir.join(format!("{FILE_NAME}.tmp")).exists());

        // Valid JSON carrying a different key is also refused.
        let other = serde_json::to_vec(&InstanceCredentials::generate()).unwrap();
        assert!(next
            .save_with(&dir, |tmp| std::fs::write(tmp, &other).unwrap())
            .is_err());
        assert_eq!(std::fs::read(dir.join(FILE_NAME)).unwrap(), before);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A process killed after staging leaves a partial staging file. It must
    /// not affect loading, and the next save must overwrite it cleanly.
    #[test]
    fn stale_partial_staging_file_is_ignored_and_replaced() {
        let dir = tmp_dir();
        let old = InstanceCredentials::generate();
        old.save(&dir).unwrap();
        std::fs::write(dir.join(format!("{FILE_NAME}.tmp")), br#"{"admin_key":"ab"#).unwrap();
        assert_eq!(
            InstanceCredentials::load(&dir).unwrap().unwrap().admin_key,
            old.admin_key
        );
        let next = InstanceCredentials::generate();
        next.save(&dir).unwrap();
        assert_eq!(
            InstanceCredentials::load(&dir).unwrap().unwrap().admin_key,
            next.admin_key
        );
        assert!(!dir.join(format!("{FILE_NAME}.tmp")).exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
