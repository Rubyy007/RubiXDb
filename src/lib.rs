//! RubixDB — Phase 0: WAL, Memtable, SSTable, Manifest, and the LSM
//! storage engine.
//!
//! Source of truth: `RubixDB-Architecture-Specification-v1.0.md`,
//! `RubixDB-WAL-Specification-v1.0.md`, and
//! `RubixDB-LSM-Engine-Specification-v1.0.md` at the repository root. Where
//! this code and those documents disagree, the documents win.

pub mod catalog;
pub mod compaction;
pub mod error;
pub mod execution;
pub mod lsm;
pub mod manifest;
pub mod memtable;
pub mod sstable;
pub mod wal;

pub use error::{EngineError, Result};
