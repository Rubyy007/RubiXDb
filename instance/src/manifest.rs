//! `instance.json` -- the persistent, non-secret identity of one
//! instance. `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md` §3.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const FILE_NAME: &str = "instance.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceManifest {
    pub instance_id: Uuid,
    pub name: String,
    pub created_at_unix_secs: u64,
    pub api_port: u16,
}

impl InstanceManifest {
    pub fn new(name: String, api_port: u16) -> Self {
        let created_at_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        InstanceManifest {
            instance_id: Uuid::new_v4(),
            name,
            created_at_unix_secs,
            api_port,
        }
    }

    pub fn load(dir: &Path) -> std::io::Result<Option<Self>> {
        let path = dir.join(FILE_NAME);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let m: Self = serde_json::from_slice(&bytes)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                Ok(Some(m))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Loads the manifest of the instance living in `dir` and checks it against
    /// that directory: the file must parse, its `name` must satisfy the instance
    /// name rule and must be the directory's own name (ASCII case-insensitive:
    /// NTFS resolves `DEFAULT` and `default` to one directory). A manifest that
    /// names another instance would otherwise make the confirmation string of
    /// `POST /v1/admin/shutdown` differ from the name an operator types, and its
    /// name is printed by `rubixdb instance list`. Errors name the file.
    pub fn load_for(dir: &Path) -> std::io::Result<Option<Self>> {
        let path = dir.join(FILE_NAME);
        let bad = |why: String| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {why}", path.display()),
            )
        };
        let Some(m) = Self::load(dir).map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidData {
                bad(format!("not a valid instance manifest ({e})"))
            } else {
                e
            }
        })?
        else {
            return Ok(None);
        };
        crate::paths::validate_instance_name(&m.name).map_err(|why| {
            bad(format!(
                "name {:?} is not a valid instance name ({why})",
                m.name
            ))
        })?;
        let dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !dir_name.eq_ignore_ascii_case(&m.name) {
            return Err(bad(format!(
                "name {:?} does not match the instance directory name {:?}",
                m.name, dir_name
            )));
        }
        Ok(Some(m))
    }

    /// Atomic write-then-rename so a reader never observes a
    /// partially-written manifest (write-tmp-then-rename is atomic on
    /// the same filesystem on both Windows -- `MoveFileEx` w/o
    /// `MOVEFILE_COPY_ALLOWED` when src/dst share a volume, which they
    /// always do here -- and POSIX `rename(2)`).
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let tmp = dir.join(format!("{FILE_NAME}.tmp"));
        let bytes = serde_json::to_vec_pretty(self).expect("InstanceManifest always serializes");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("rubixdb_manifest_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = InstanceManifest::new("default".to_string(), 18080);
        m.save(&dir).unwrap();
        let loaded = InstanceManifest::load(&dir).unwrap().unwrap();
        assert_eq!(m, loaded);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn instance_dir(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rubixdb_manifest_for_{}", Uuid::new_v4()));
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_for_accepts_a_manifest_that_matches_its_directory() {
        let dir = instance_dir("alpha");
        InstanceManifest::new("alpha".to_string(), 1234)
            .save(&dir)
            .unwrap();
        assert!(InstanceManifest::load_for(&dir).unwrap().is_some());
        // NTFS aliases `ALPHA` and `alpha`; the ASCII-case-insensitive match keeps that working.
        let upper = dir.parent().unwrap().join("ALPHA");
        assert!(InstanceManifest::load_for(&upper).unwrap().is_some() || !upper.exists());
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn load_for_rejects_a_foreign_invalid_or_unparsable_name_and_names_the_file() {
        for (dir_name, manifest_name) in [
            ("alpha", "beta"),
            ("alpha", "../../x"),
            ("alpha", "bad name"),
            ("alpha", ""),
            ("alpha", "a\u{1b}[31mred"),
        ] {
            let dir = instance_dir(dir_name);
            let mut m = InstanceManifest::new("alpha".to_string(), 1234);
            m.name = manifest_name.to_string();
            m.save(&dir).unwrap();
            let err = InstanceManifest::load_for(&dir).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            let msg = err.to_string();
            assert!(msg.contains("instance.json"), "{msg}");
            assert!(
                !msg.contains('\u{1b}'),
                "control characters must not reach the terminal: {msg:?}"
            );
            std::fs::remove_dir_all(dir.parent().unwrap()).ok();
        }
        let dir = instance_dir("alpha");
        std::fs::write(dir.join(FILE_NAME), b"not json").unwrap();
        let msg = InstanceManifest::load_for(&dir).unwrap_err().to_string();
        assert!(
            msg.contains("instance.json") && msg.contains("not a valid"),
            "{msg}"
        );
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn missing_manifest_is_none_not_error() {
        let dir = std::env::temp_dir().join(format!("rubixdb_manifest_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(InstanceManifest::load(&dir).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_manifest_is_a_typed_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("rubixdb_manifest_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), b"not json").unwrap();
        let err = InstanceManifest::load(&dir).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(&dir).ok();
    }
}
