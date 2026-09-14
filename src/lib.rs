//! RubixDB — Phase 0: WAL, Memtable, SSTable, and the LSM storage engine.
//!
//! Source of truth: `RubixDB-Architecture-Specification-v1.0.md` and
//! `RubixDB-WAL-Specification-v1.0.md` at the repository root. Where this
//! code and those documents disagree, the documents win.

pub mod compaction;
pub mod engine;
pub mod memtable;
pub mod sstable;
pub mod wal;
