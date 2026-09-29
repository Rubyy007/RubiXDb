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
