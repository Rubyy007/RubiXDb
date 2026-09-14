//! Shared error taxonomy, per Architecture Spec §4.2. Every Phase 0
//! component (`wal`, `manifest`, `sstable`, `lsm`, `compaction`) returns
//! `Result<T>` over this one enum rather than defining its own — see
//! `ARCHITECTURE.md`'s "Error type" entry for why this is effectively a
//! Tier 1 constraint, not a free choice.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Errors any storage-engine-layer operation can return.
///
/// Every variant that can be reached by reading corrupted or otherwise
/// untrusted on-disk bytes is a `Result` path, never a panic — see the
/// build prompt's Non-Negotiable Security bar.
#[derive(Debug)]
pub enum EngineError {
    /// The requested key/record does not exist.
    NotFound,
    /// On-disk bytes failed a checksum, length, or structural check and
    /// cannot be trusted. Carries enough detail to locate the problem
    /// without including the corrupted payload bytes themselves (payload
    /// contents are never logged, per the Non-Negotiable Security bar).
    Corruption { detail: String },
    /// Underlying OS I/O failure (permissions, disk full, device error).
    Io(io::Error),
    /// The WAL could not be opened/used in its current state.
    WalUnavailable { detail: String },
    /// An operation is not supported by this configuration/engine.
    Unsupported { operation: String },
    /// A length-prefixed field (a payload on `append`, a declared record
    /// length during recovery) exceeded a configured maximum.
    CapacityExceeded { requested: u64, max: u64 },
    /// An in-progress multi-step operation was deliberately abandoned
    /// (e.g., a migration or compaction cycle) rather than left partially
    /// applied.
    Aborted { detail: String },
    /// A path constructed from configuration resolved outside the
    /// configured data directory. Always a configuration error, never
    /// something a caller should retry.
    InvalidPath { detail: String, path: PathBuf },
    /// A caller-visible wait bound was exceeded before the awaited
    /// condition became true (e.g. `wal::group_commit::GroupCommitter::
    /// await_durable`'s follower timeout). Distinct from `Io`: no I/O
    /// necessarily failed — the wait itself simply ran out of time, and a
    /// caller may reasonably retry the wait.
    Timeout { detail: String },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::NotFound => write!(f, "not found"),
            EngineError::Corruption { detail } => write!(f, "corruption: {detail}"),
            EngineError::Io(e) => write!(f, "I/O error: {e}"),
            EngineError::WalUnavailable { detail } => write!(f, "WAL unavailable: {detail}"),
            EngineError::Unsupported { operation } => write!(f, "unsupported: {operation}"),
            EngineError::CapacityExceeded { requested, max } => {
                write!(f, "capacity exceeded: requested {requested}, max {max}")
            }
            EngineError::Aborted { detail } => write!(f, "aborted: {detail}"),
            EngineError::InvalidPath { detail, path } => {
                write!(f, "invalid path {}: {detail}", path.display())
            }
            EngineError::Timeout { detail } => write!(f, "timeout: {detail}"),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EngineError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for EngineError {
    fn from(e: io::Error) -> Self {
        EngineError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;
