//! LSM engine facade implementing the Storage Engine Contract (Architecture
//! Spec Section 4), per LSM Engine Spec Section 4: the `ReadView` read
//! pattern, the write path, freeze-and-flush, and — since recovery is
//! cross-cutting rather than a bounded component of its own (LSM Engine
//! Spec Section 7) — end-to-end crash recovery.
//!
//! Not yet implemented — Phase 0, Step 4 (facade) and Step 7 (recovery).
//! Built on top of `wal`, `memtable`, `sstable`, and `manifest`.
