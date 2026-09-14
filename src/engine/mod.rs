//! LSM engine facade implementing the Storage Engine Contract (Architecture
//! Spec Section 4): put, get, delete, point_lookup, range_scan, flush,
//! checkpoint, snapshot, recover, export_iterator.
//!
//! Not yet implemented — Phase 0, Step 4. Built on top of `wal`, `memtable`,
//! and `sstable`.
