//! RUBIC SSTable: immutable on-disk sorted structure — data blocks, bloom
//! filter block, sparse index block, fixed 72-byte footer — per
//! `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` (Phase 4B). Built via the
//! atomic tmp-file-then-rename discipline in that document's §3.4.
//!
//! # Durability / Manifest boundary (read this before touching this module)
//!
//! Per `PHASE4B_ADR.md` ADR-P4B-1 (explicit, user-approved decision):
//! this module introduces **no Manifest and no WAL interaction of any
//! kind**. An `SsTable` is a purely additional, purely derived read-path
//! source over data the WAL already holds durably. Nothing in this
//! module ever calls `wal::purge_before`, and nothing in this module is
//! ever the sole durable copy of any record it contains — see
//! `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.5 for why this makes
//! SSTable corruption a pure *availability* concern, never a *data-loss*
//! one, in this phase.
//!
//! # Liveness without a Manifest
//!
//! "Which SSTables are live" reduces to "which `.sst` files exist under
//! `<data_dir>/sstables/` and pass full footer/index/bloom validation" —
//! `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.3. `discover` below is that
//! sweep; a validation failure is a hard `Err`, not a silent exclusion
//! (`PHASE4B_ADR.md` ADR-P4B-2).

mod bloom;
pub mod format;
mod reader;
#[cfg(test)]
mod test_support;
mod writer;

pub use bloom::BloomFilter;
pub use reader::SsTable;
pub use writer::{write_from_memtable, SsTableWriterConfig};

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{EngineError, Result};

/// A resolved SSTable-sourced value — deliberately not collapsed to
/// `Option<Vec<u8>>` at this layer (LSM Engine Spec §2.8's own
/// `RecordValue`): the caller (the `LsmEngine` multi-source merge) is
/// responsible for the tombstone-hides-older-value collapse, exactly
/// once, across every source — never per-source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordValue {
    Put(Vec<u8>),
    Tombstone,
}

/// Metadata about one published SSTable — `RubixDB-LSM-Engine-
/// Specification-v1.0.md` §2.8, Phase-4B naming (`SstableMeta` there,
/// `id`/`min_seq`/`max_seq`/`path` unchanged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstableMeta {
    pub id: u64,
    pub min_seq: u64,
    pub max_seq: u64,
    pub record_count: u64,
    pub path: PathBuf,
}

/// Zero-padded, 20-digit decimal filename for a published table —
/// `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.2, exactly.
pub fn sstable_filename(id: u64) -> String {
    format!("{id:020}.sst")
}

/// Same, for the in-progress build path.
pub fn sstable_tmp_filename(id: u64) -> String {
    format!("{id:020}.sst.tmp")
}

/// Parses a *published* SSTable's numeric id from a filename, requiring
/// the exact `{20 digits}.sst` shape (never matches `.sst.tmp`, never
/// matches a non-numeric or wrong-width name) — used both by directory-
/// scan ID recovery (§3.1) and by the discovery sweep below.
pub(crate) fn parse_sstable_id(filename: &str) -> Option<u64> {
    let digits = filename.strip_suffix(".sst")?;
    if digits.len() != 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// The full startup sweep, `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.3:
/// 1. Delete every `*.sst.tmp` unconditionally (always an interrupted
///    build; never trusted, Manifest or not).
/// 2. Open and fully validate every remaining `*.sst` file. A validation
///    failure is a hard `Err` naming the exact path (`PHASE4B_ADR.md`
///    ADR-P4B-2's fail-closed choice) — never a silent exclusion.
///
/// Returns the live set, sorted **newest-first by id** (matching the
/// `sstables: Vec<Arc<SsTable>>` "newest-first" convention `LsmEngine`
/// uses for `immutables` too), plus the next unused id
/// (`max(id found) + 1`, or `1` for an empty/absent directory —
/// `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.1).
pub fn discover(sstables_dir: &Path) -> Result<(Vec<Arc<SsTable>>, u64)> {
    if !sstables_dir.exists() {
        return Ok((Vec::new(), 1));
    }
    let mut sst_ids: Vec<u64> = Vec::new();
    for entry in fs::read_dir(sstables_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.ends_with(".sst.tmp") {
            // Always an interrupted build -- never trusted, swept
            // unconditionally, regardless of how far the build got.
            fs::remove_file(entry.path())?;
            continue;
        }
        if let Some(id) = parse_sstable_id(name) {
            sst_ids.push(id);
        }
        // Any other filename under sstables/ is simply not this format's
        // concern -- ignored, not an error (a foreign file here is an
        // operator/deployment question, not a corruption question).
    }
    sst_ids.sort_unstable();
    let next_id = sst_ids.last().map_or(1, |max| max + 1);

    let mut tables = Vec::with_capacity(sst_ids.len());
    for id in sst_ids.iter().rev() {
        // newest-first
        let path = sstables_dir.join(sstable_filename(*id));
        let table = SsTable::open(&path, *id).map_err(|e| match e {
            EngineError::Corruption { detail } => EngineError::Corruption {
                detail: format!("sstable {}: {detail}", path.display()),
            },
            EngineError::Unsupported { operation } => EngineError::Unsupported {
                operation: format!("sstable {}: {operation}", path.display()),
            },
            other => other,
        })?;
        tables.push(Arc::new(table));
    }
    Ok((tables, next_id))
}

#[cfg(test)]
mod tests;
