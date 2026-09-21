//! RUBIC SSTable writer — `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.8
//! (build process) and §3.4 (atomic publication). Never writes directly
//! to a path a reader might treat as live; the `.sst.tmp` -> `.sst`
//! rename is the sole publication point.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::error::Result;
use crate::memtable::{MemTable, MemtableValue};
use crate::sstable::bloom::BloomFilter;
use crate::sstable::format::{
    self, DEFAULT_BLOOM_BITS_PER_KEY, DEFAULT_TARGET_BLOCK_SIZE, OP_DELETE, OP_PUT,
};
use crate::sstable::{sstable_filename, sstable_tmp_filename, RecordValue, SstableMeta};

#[derive(Debug, Clone)]
pub struct SsTableWriterConfig {
    pub target_block_size: usize,
    pub bloom_bits_per_key: u32,
}

impl Default for SsTableWriterConfig {
    fn default() -> Self {
        SsTableWriterConfig {
            target_block_size: DEFAULT_TARGET_BLOCK_SIZE,
            bloom_bits_per_key: DEFAULT_BLOOM_BITS_PER_KEY,
        }
    }
}

/// Builds a complete RUBIC SSTable from `memtable` and atomically
/// publishes it as `<sstables_dir>/{id:020}.sst`, per
/// `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.8/§3.4. A thin adapter
/// over [`write_from_sorted_records`] — see that function's own doc
/// comment for the full construction/publication discipline, which is
/// shared, unmodified, between this and every other producer of a
/// `.sst` file (`ADR-COMPACTION-001` Decision 3: generalizing the
/// writer's *input shape* only, never the on-disk format). `memtable.
/// range(Unbounded, Unbounded)` already yields `(key, seq)` ascending
/// (LSM Engine Spec §1.1's sorted `BTreeMap` backing), so no extra
/// sort is needed here.
pub fn write_from_memtable(
    memtable: &MemTable,
    id: u64,
    sstables_dir: &Path,
    config: &SsTableWriterConfig,
) -> Result<SstableMeta> {
    let entry_count_hint = memtable.entry_count() as u64;
    let records = memtable
        .range(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
        .map(|((key, seq), value)| {
            let rv = match value {
                MemtableValue::Put(v) => RecordValue::Put(v.clone()),
                MemtableValue::Tombstone => RecordValue::Tombstone,
            };
            Ok((key.clone(), *seq, rv))
        });
    write_from_sorted_records(records, id, sstables_dir, config, entry_count_hint)
}

/// `ADR-COMPACTION-001` Decision 3: the generalized writer entry point
/// Compaction's k-way merge uses (via `SsTableRangeCursor`-driven
/// output, `src/compaction/mod.rs`), sharing 100% of the block/bloom/
/// index/footer construction and atomic `.tmp`-then-rename publication
/// discipline with [`write_from_memtable`] — no on-disk format change,
/// only an additional, input-shape-generic entry point.
///
/// `records` must already be in `(key asc, seq asc)` order (the same
/// order `range_scan_raw`/`SsTableRangeCursor`/`MemTable::range` all
/// already produce) — this function does not sort; it streams. Each
/// item may be `Err` (propagating a real I/O/corruption error from an
/// upstream source, e.g. a corrupted compaction input) — the first
/// `Err` aborts the build immediately, leaving only an untrusted
/// `.sst.tmp` behind (identical fail-closed shape to every other
/// error path in this function; the file is a partial-build under
/// interruption either way, and `sstable::discover`'s existing `.tmp`
/// sweep already treats every remaining `.tmp` file, regardless of
/// which caller left it, as untrusted and reclaims it unconditionally
/// at the next startup — `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.3).
///
/// `entry_count_hint` sizes the bloom filter up front (`BloomFilter::
/// new_for_key_count`) — it need not be exact (a compaction merge's
/// true surviving-record count is only known after the fact); an
/// overestimate (e.g. the sum of input SSTables' `record_count()`,
/// which is always `>=` the true surviving count since compaction only
/// ever drops records) is safe and merely yields a marginally lower
/// false-positive rate than requested, never an incorrect result.
///
/// `min_seq`/`max_seq` for the output footer are computed from the
/// actual records streamed through, not passed in — for `write_from_
/// memtable` this is provably identical to `memtable.seq_range()`
/// (`MemTable`'s own `min_seq`/`max_seq` fields are updated on every
/// insert, i.e. already "min/max seq across every record," the same
/// quantity this function derives by observing every record it writes).
pub fn write_from_sorted_records<I>(
    records: I,
    id: u64,
    sstables_dir: &Path,
    config: &SsTableWriterConfig,
    entry_count_hint: u64,
) -> Result<SstableMeta>
where
    I: Iterator<Item = Result<(Vec<u8>, u64, RecordValue)>>,
{
    fs::create_dir_all(sstables_dir)?;
    let tmp_path = sstables_dir.join(sstable_tmp_filename(id));
    let final_path = sstables_dir.join(sstable_filename(id));

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)?;

    let mut bloom =
        BloomFilter::new_for_key_count(entry_count_hint.max(1), config.bloom_bits_per_key);

    let mut offset: u64 = 0;
    let mut index_entries = Vec::new();
    let mut block_records_buf: Vec<u8> = Vec::new();
    let mut block_record_count: u32 = 0;
    let mut block_last_key: Vec<u8> = Vec::new();
    let mut total_records: u64 = 0;
    let mut min_seq: Option<u64> = None;
    let mut max_seq: Option<u64> = None;

    for item in records {
        let (key, seq, value) = item?;
        let (op, value_bytes): (u8, Vec<u8>) = match value {
            RecordValue::Put(v) => (OP_PUT, v),
            RecordValue::Tombstone => (OP_DELETE, Vec::new()),
        };
        format::encode_record(&mut block_records_buf, &key, seq, op, &value_bytes)?;
        block_record_count += 1;
        block_last_key = key.clone();
        total_records += 1;
        min_seq = Some(min_seq.map_or(seq, |m| m.min(seq)));
        max_seq = Some(max_seq.map_or(seq, |m| m.max(seq)));
        bloom.insert(&key);

        if block_records_buf.len() + 4 >= config.target_block_size {
            offset = finalize_and_write_block(
                &mut file,
                &mut index_entries,
                offset,
                block_record_count,
                &block_records_buf,
                std::mem::take(&mut block_last_key),
            )?;
            block_records_buf.clear();
            block_record_count = 0;
        }
    }
    if block_record_count > 0 {
        offset = finalize_and_write_block(
            &mut file,
            &mut index_entries,
            offset,
            block_record_count,
            &block_records_buf,
            block_last_key,
        )?;
    }
    let bloom_offset = offset;
    let bloom_bytes =
        format::encode_bloom_block(bloom.num_bits(), bloom.num_hash_functions(), bloom.bits());
    file.write_all(&bloom_bytes)?;
    offset += bloom_bytes.len() as u64;
    let bloom_length = bloom_bytes.len() as u64;

    let index_offset = offset;
    let index_bytes = format::encode_index_block(&index_entries)?;
    file.write_all(&index_bytes)?;
    let index_length = index_bytes.len() as u64;

    let min_seq = min_seq.unwrap_or(0);
    let max_seq = max_seq.unwrap_or(0);
    let footer = format::Footer {
        format_version: format::FORMAT_VERSION,
        min_seq,
        max_seq,
        record_count: total_records,
        bloom_offset,
        bloom_length,
        index_offset,
        index_length,
    };
    file.write_all(&footer.encode())?;

    file.sync_all()?;
    drop(file);

    fs::rename(&tmp_path, &final_path)?;
    crate::wal::fsync_dir(sstables_dir)?;

    Ok(SstableMeta {
        id,
        min_seq,
        max_seq,
        record_count: total_records,
        path: final_path,
    })
}

/// Writes one finalized data block to `file` at the current logical
/// `offset`, records its `IndexEntry`, and returns the new offset.
fn finalize_and_write_block(
    file: &mut File,
    index_entries: &mut Vec<format::IndexEntry>,
    offset: u64,
    record_count: u32,
    records_buf: &[u8],
    last_key: Vec<u8>,
) -> Result<u64> {
    let block = format::finalize_block(record_count, records_buf);
    file.write_all(&block)?;
    let block_length: u32 =
        block
            .len()
            .try_into()
            .map_err(|_| crate::error::EngineError::CapacityExceeded {
                requested: block.len() as u64,
                max: u32::MAX as u64,
            })?;
    index_entries.push(format::IndexEntry {
        last_key,
        block_offset: offset,
        block_length,
    });
    offset
        .checked_add(block_length as u64)
        .ok_or(crate::error::EngineError::CapacityExceeded {
            requested: u64::MAX,
            max: u64::MAX - 1,
        })
}
