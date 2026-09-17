//! Engine-local, crash-safe record of the live SSTable set and the
//! durable checkpoint boundary — `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md`,
//! `PHASE5_MANIFEST_ARCHITECTURE.md`. Reuses the WAL's exact frame shape
//! (`length || crc32c || body`, independently implemented — see
//! `format.rs`'s own doc comment for why) and the WAL's own directory
//! lock file (no Manifest-specific lock).
//!
//! # Two-phase recovery, not one
//!
//! Manifest recovery has a read-only phase (`replay_readonly`, shared
//! lock — must run, and release its lock, before `FileWal::open_for_
//! recovery`'s exclusive lock, exactly like `wal::replay_streaming`
//! already must, `PHASE4A_ADR.md` ADR-P4A-3) and a write-capable phase
//! (`open_after_exclusive_lock`, run only once the caller already holds
//! the exclusive lock via `FileWal::open_for_recovery`) that re-replays
//! (cheap; the file is small) and returns a `Manifest` handle ready for
//! `append_sync`. See `PHASE5_MANIFEST_ARCHITECTURE.md` §4 for the full
//! ordering derivation and why this split exists.

mod format;
mod recovery;
mod state;

pub use format::ManifestEdit;
pub use recovery::ManifestReplayResult;
pub use state::{CheckpointState, ManifestState, SstableManifestEntry};

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::Result;
use crate::wal::{acquire_shared_lock_if_present, LOCK_FILE_NAME};

const MANIFEST_FILE_NAME: &str = "MANIFEST";

/// Read-only Manifest replay under a **shared** lock — safe to call
/// before an exclusive lock is held elsewhere in the same process
/// (`PHASE4A_ADR.md` ADR-P4A-3's same-process lock-ordering
/// constraint). Returns `Ok(default state)` if the Manifest file
/// doesn't exist yet (a fresh engine — no live SSTables, no
/// checkpoint, `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §9).
pub fn replay_readonly(dir: &Path) -> Result<ManifestReplayResult> {
    let lock_path = dir.join(LOCK_FILE_NAME);
    let _lock = acquire_shared_lock_if_present(&lock_path)?; // dropped (released) at function end
    let manifest_path = dir.join(MANIFEST_FILE_NAME);
    let mut file = match File::open(&manifest_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManifestReplayResult::default())
        }
        Err(e) => return Err(e.into()),
    };
    recovery::replay(&mut file)
}

/// An open Manifest, ready for `append_sync`. Only ever constructed
/// while the caller already holds the data directory's exclusive lock
/// (`FileWal::open_for_recovery`'s own lock) — this type does no
/// locking of its own beyond that inherited guarantee, matching
/// `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §10.
#[derive(Debug)]
pub struct Manifest {
    file: File,
    /// Total valid edits ever appended or replayed this open session —
    /// inspection/observability only (`PHASE5_ARCHITECTURE.md` §7).
    record_count: u64,
    last_edit: Option<ManifestEdit>,
}

impl Manifest {
    /// Opens (creating if absent) `<dir>/MANIFEST`, re-replays it
    /// (cheap — the file is small; the first, shared-lock pass already
    /// validated it, this pass runs under exclusive protection so its
    /// result is authoritative for building the live SSTable set and
    /// driving the directory sweep) and physically truncates any torn
    /// tail before returning — mirroring `FileWal::open_for_recovery`'s
    /// own truncate-on-torn-tail behavior exactly, not a new pattern.
    ///
    /// **Caller contract**: must already hold the exclusive directory
    /// lock. Not enforced by this function (the lock is a `File` held
    /// by the caller, e.g. via `FileWal::open_for_recovery`, not
    /// something this function can observe) — documented, not silently
    /// assumed.
    pub fn open_after_exclusive_lock(dir: &Path) -> Result<(Manifest, ManifestReplayResult)> {
        std::fs::create_dir_all(dir)?;
        let manifest_path = dir.join(MANIFEST_FILE_NAME);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&manifest_path)?;

        let replay_result = recovery::replay(&mut file)?;
        if replay_result.truncated {
            file.set_len(replay_result.valid_length)?;
        }
        file.seek(SeekFrom::Start(replay_result.valid_length))?;

        let manifest = Manifest {
            file,
            record_count: replay_result.edit_count,
            last_edit: replay_result.last_edit,
        };
        Ok((manifest, replay_result))
    }

    /// Appends one edit and durably fsyncs it before returning — every
    /// `Manifest` write is `append_sync`, never a bare `append` a
    /// caller might forget to sync (`RUBIC_MANIFEST_FORMAT_
    /// SPECIFICATION.md` §3.2's "append... and fsync it," applied
    /// identically to every edit type, not only `ADD_SSTABLE`).
    pub fn append_sync(&mut self, edit: ManifestEdit) -> Result<()> {
        let frame = format::encode_frame(&edit);
        self.file.write_all(&frame)?;
        self.file.sync_all()?;
        self.record_count += 1;
        self.last_edit = Some(edit);
        Ok(())
    }

    /// Total valid edits appended or replayed so far this session —
    /// inspection tool / `PHASE5_ARCHITECTURE.md` §7. Never mutates
    /// storage.
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// The last successfully-applied edit, if any — inspection only.
    pub fn last_edit(&self) -> Option<ManifestEdit> {
        self.last_edit
    }

    /// Current on-disk file size, in bytes — inspection only.
    pub fn size_bytes(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

#[cfg(test)]
mod tests;
