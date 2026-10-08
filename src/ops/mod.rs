//! Production operations: logical backup, restore, integrity verification.
//!
//! Everything here is built on the engine's *public* API (`LsmEngine`
//! snapshots, range scans and atomic `write_batch`) and the catalog /
//! relational encodings. It does not touch the WAL, manifest, SSTable or
//! compaction implementations. Design record:
//! `PHASE_RUBIXDB_BACKUP_RESTORE_ARCHITECTURE.md`,
//! `PHASE_RUBIXDB_INTEGRITY_ARCHITECTURE.md`.

pub mod backup;
pub mod catalog_mirror;
pub mod check;
pub mod format;
pub mod maintenance;
pub mod open;
pub mod restore;
pub mod storage;
pub mod wal_tail;

#[cfg(test)]
mod fuzz_tests;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod physical_tests;
#[cfg(test)]
mod wal_tail_tests;

use std::fmt;

/// A classified operational failure. `code` is a small, closed set of
/// stable identifiers (safe as a metric label / API error code); `detail`
/// is human-readable and may name the object concerned but never a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpsError {
    pub code: &'static str,
    pub detail: String,
}

impl OpsError {
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        OpsError {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for OpsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for OpsError {}

impl From<std::io::Error> for OpsError {
    fn from(e: std::io::Error) -> Self {
        OpsError::new(codes::IO, e.to_string())
    }
}

impl From<crate::EngineError> for OpsError {
    fn from(e: crate::EngineError) -> Self {
        OpsError::new(codes::ENGINE, e.to_string())
    }
}

/// Stable error codes (closed set).
pub mod codes {
    pub const IO: &str = "IO";
    pub const WAL_CORRUPT: &str = "WAL_CORRUPT";
    /// ADR-WAL-01: acknowledged records recorded by the last graceful shutdown are missing from the WAL.
    pub const WAL_TAIL_DAMAGED: &str = "WAL_TAIL_DAMAGED";
    /// ADR-WAL-01: a damaged WAL tail that recovery would remove could not be preserved first.
    pub const WAL_TAIL_QUARANTINE_FAILED: &str = "WAL_TAIL_QUARANTINE_FAILED";
    pub const ENGINE: &str = "ENGINE";
    pub const BAD_MAGIC: &str = "BACKUP_BAD_MAGIC";
    pub const UNSUPPORTED_VERSION: &str = "BACKUP_UNSUPPORTED_VERSION";
    pub const HEADER_INVALID: &str = "BACKUP_HEADER_INVALID";
    pub const TRUNCATED: &str = "BACKUP_TRUNCATED";
    pub const CHUNK_CHECKSUM: &str = "BACKUP_CHUNK_CHECKSUM";
    pub const CHUNK_ORDER: &str = "BACKUP_CHUNK_ORDER";
    pub const CHUNK_INVALID: &str = "BACKUP_CHUNK_INVALID";
    pub const KEY_ORDER: &str = "BACKUP_KEY_ORDER";
    pub const FOOTER_MISMATCH: &str = "BACKUP_FOOTER_MISMATCH";
    pub const TRAILING_DATA: &str = "BACKUP_TRAILING_DATA";
    pub const CATALOG_INVALID: &str = "BACKUP_CATALOG_INVALID";
    pub const DEST_EXISTS: &str = "DESTINATION_EXISTS";
    pub const DEST_NOT_EMPTY: &str = "DESTINATION_NOT_EMPTY";
    pub const BAD_NAME: &str = "INVALID_NAME";
    pub const CANCELLED: &str = "CANCELLED";
    pub const RESTORE_VERIFY: &str = "RESTORE_VERIFICATION_FAILED";
    pub const CHECK_FAILED: &str = "INTEGRITY_ERRORS_FOUND";
    pub const NOT_CONFIRMED: &str = "CONFIRMATION_REQUIRED";
    pub const PRECONDITION: &str = "PRECONDITION_FAILED";
}

/// Backup / restore / check file-name validation: a *name*, never a path.
/// 1..=64 chars of `[A-Za-z0-9._-]`, not starting with `.` and not `..`.
/// This is what the HTTP API accepts, so a caller cannot choose a
/// directory, traverse, or hit a reserved Windows device name.
pub fn validate_simple_name(name: &str) -> Result<(), OpsError> {
    let ok_chars = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if !ok_chars || name.starts_with('.') || name.ends_with('.') {
        return Err(OpsError::new(
            codes::BAD_NAME,
            "name must be 1-64 characters of [A-Za-z0-9._-], not starting or ending with '.'",
        ));
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        return Err(OpsError::new(
            codes::BAD_NAME,
            "name is a reserved device name",
        ));
    }
    Ok(())
}

/// A process-unique 128-bit identifier rendered as 32 hex chars. Not a
/// secret and not security-relevant: it only labels a backup / staging
/// directory so two operations never collide.
pub(crate) fn unique_id_hex() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let local = 0u8;
    let addr = &local as *const u8 as usize as u64;
    let mut seed = Vec::with_capacity(40);
    seed.extend_from_slice(&nanos.to_le_bytes());
    seed.extend_from_slice(&n.to_le_bytes());
    seed.extend_from_slice(&u64::from(std::process::id()).to_le_bytes());
    seed.extend_from_slice(&addr.to_le_bytes());
    let a = xxhash_rust::xxh64::xxh64(&seed, 0x5242_5842);
    let b = xxhash_rust::xxh64::xxh64(&seed, 0x4B55_5042);
    format!("{a:016x}{b:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_accept_plain_and_reject_traversal() {
        for ok in ["backup1", "nightly-2026.10.03", "a_b-c.rbxbackup"] {
            assert!(validate_simple_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "C:\\x",
            "x:y",
            ".hidden",
            "trailingdot.",
            "nul",
            "NUL.txt",
            "com1",
            "a b",
            "é",
            &"x".repeat(65),
        ] {
            assert!(validate_simple_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn unique_ids_do_not_repeat() {
        let a = unique_id_hex();
        let b = unique_id_hex();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
    }
}
