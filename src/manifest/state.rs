//! In-memory Manifest recovery/runtime state — `RUBIC_MANIFEST_FORMAT_
//! SPECIFICATION.md` §8. Built by sequential replay, one edit at a
//! time; bounded by the current live-SSTable count and the total
//! number of SSTables ever created, never by the number of edits in
//! the Manifest file.

use std::collections::{BTreeMap, HashSet};

use crate::error::{EngineError, Result};
use crate::manifest::format::ManifestEdit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SstableManifestEntry {
    pub min_seq: u64,
    pub max_seq: u64,
    pub file_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointState {
    pub flushed_through_seq: u64,
    pub wal_segment_id: u64,
    pub wal_offset: u64,
}

/// The full reconstructed Manifest-authoritative state —
/// `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §8.
#[derive(Debug, Default)]
pub struct ManifestState {
    pub live_sstables: BTreeMap<u64, SstableManifestEntry>,
    /// Every id ever `ADD_SSTABLE`'d, live or since removed — needed to
    /// distinguish "already removed" (idempotent, §7) from "never
    /// added" (corruption, §3.2).
    pub ever_added: HashSet<u64>,
    pub checkpoint: Option<CheckpointState>,
}

impl ManifestState {
    pub fn new() -> Self {
        ManifestState::default()
    }

    /// `flushed_through_seq`, or `0` if no checkpoint has ever been
    /// recorded — matching Phase 4A/4B's existing "no prior state means
    /// replay everything" behavior exactly (a `0` boundary discards
    /// nothing, since WAL sequences start at `1`).
    pub fn checkpoint_seq(&self) -> u64 {
        self.checkpoint.map_or(0, |c| c.flushed_through_seq)
    }

    /// Applies one already-frame-validated edit (`format::decode_body`
    /// already checked structural validity), enforcing the semantic,
    /// cross-edit rules `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §3.2
    /// assigns to this layer: invalid SSTable references, checkpoint
    /// regression. Idempotent per §7: a duplicate `ADD_SSTABLE` for an
    /// already-live id, or a `REMOVE_SSTABLE` for an already-removed
    /// (but previously-added) id, or a `SET_CHECKPOINT` repeating the
    /// current value, are all no-ops, not errors.
    pub fn apply(&mut self, edit: ManifestEdit) -> Result<()> {
        match edit {
            ManifestEdit::AddSstable {
                id,
                min_seq,
                max_seq,
                file_size,
            } => {
                self.ever_added.insert(id);
                self.live_sstables
                    .entry(id)
                    .or_insert(SstableManifestEntry {
                        min_seq,
                        max_seq,
                        file_size,
                    });
                Ok(())
            }
            ManifestEdit::RemoveSstable { id } => {
                if !self.ever_added.contains(&id) {
                    return Err(EngineError::Corruption {
                        detail: format!(
                            "manifest: REMOVE_SSTABLE for id {id} that was never ADD_SSTABLE'd"
                        ),
                    });
                }
                self.live_sstables.remove(&id);
                Ok(())
            }
            ManifestEdit::SetCheckpoint {
                flushed_through_seq,
                wal_segment_id,
                wal_offset,
            } => {
                if let Some(current) = self.checkpoint {
                    if flushed_through_seq < current.flushed_through_seq {
                        return Err(EngineError::Corruption {
                            detail: format!(
                                "manifest: SET_CHECKPOINT regression: {flushed_through_seq} < \
                                 current checkpoint {}",
                                current.flushed_through_seq
                            ),
                        });
                    }
                }
                self.checkpoint = Some(CheckpointState {
                    flushed_through_seq,
                    wal_segment_id,
                    wal_offset,
                });
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_then_remove_removes_from_live_set() {
        let mut s = ManifestState::new();
        s.apply(ManifestEdit::AddSstable {
            id: 1,
            min_seq: 1,
            max_seq: 10,
            file_size: 100,
        })
        .unwrap();
        assert!(s.live_sstables.contains_key(&1));
        s.apply(ManifestEdit::RemoveSstable { id: 1 }).unwrap();
        assert!(!s.live_sstables.contains_key(&1));
        assert!(s.ever_added.contains(&1));
    }

    #[test]
    fn duplicate_add_is_idempotent() {
        let mut s = ManifestState::new();
        let edit = ManifestEdit::AddSstable {
            id: 1,
            min_seq: 1,
            max_seq: 10,
            file_size: 100,
        };
        s.apply(edit).unwrap();
        s.apply(edit).unwrap();
        assert_eq!(s.live_sstables.len(), 1);
    }

    #[test]
    fn duplicate_remove_is_idempotent() {
        let mut s = ManifestState::new();
        s.apply(ManifestEdit::AddSstable {
            id: 1,
            min_seq: 1,
            max_seq: 10,
            file_size: 100,
        })
        .unwrap();
        s.apply(ManifestEdit::RemoveSstable { id: 1 }).unwrap();
        s.apply(ManifestEdit::RemoveSstable { id: 1 }).unwrap();
        assert!(!s.live_sstables.contains_key(&1));
    }

    #[test]
    fn remove_never_added_is_corruption() {
        let mut s = ManifestState::new();
        let err = s
            .apply(ManifestEdit::RemoveSstable { id: 999 })
            .unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn checkpoint_regression_is_corruption() {
        let mut s = ManifestState::new();
        s.apply(ManifestEdit::SetCheckpoint {
            flushed_through_seq: 100,
            wal_segment_id: 1,
            wal_offset: 0,
        })
        .unwrap();
        let err = s
            .apply(ManifestEdit::SetCheckpoint {
                flushed_through_seq: 50,
                wal_segment_id: 1,
                wal_offset: 0,
            })
            .unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }

    #[test]
    fn checkpoint_repeat_is_idempotent() {
        let mut s = ManifestState::new();
        let edit = ManifestEdit::SetCheckpoint {
            flushed_through_seq: 100,
            wal_segment_id: 1,
            wal_offset: 0,
        };
        s.apply(edit).unwrap();
        s.apply(edit).unwrap();
        assert_eq!(s.checkpoint_seq(), 100);
    }

    #[test]
    fn checkpoint_advance_is_accepted() {
        let mut s = ManifestState::new();
        s.apply(ManifestEdit::SetCheckpoint {
            flushed_through_seq: 100,
            wal_segment_id: 1,
            wal_offset: 0,
        })
        .unwrap();
        s.apply(ManifestEdit::SetCheckpoint {
            flushed_through_seq: 200,
            wal_segment_id: 2,
            wal_offset: 0,
        })
        .unwrap();
        assert_eq!(s.checkpoint_seq(), 200);
    }

    #[test]
    fn no_checkpoint_yet_defaults_to_zero() {
        let s = ManifestState::new();
        assert_eq!(s.checkpoint_seq(), 0);
    }
}
