//! Size-tiered, full-merge compaction, per LSM Engine Spec Section 5:
//! triggers at `compaction_trigger_count` live SSTables, enforces the
//! tombstone-safety rule against outstanding `snapshot_refs`.
//!
//! Not yet implemented — Phase 0, Step 6.
