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
use crate::sstable::{sstable_filename, sstable_tmp_filename, SstableMeta};

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
/// `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.8/§3.4:
///
/// 1. Write the complete table to `{id}.sst.tmp`.
/// 2. `fsync` the temp file.
/// 3. `rename` to `{id}.sst` (atomic w.r.t. observers on both platforms).
/// 4. `fsync` the containing directory (`wal::fsync_dir` — real on Unix,
///    a documented no-op on Windows, `PHASE4B_ADR.md` ADR-P4B-4).
///
/// On any failure, the `.sst.tmp` file (if created) is left on disk,
/// untouched — never partially trusted, swept unconditionally by the
/// next `sstable::discover` call (`PHASE4B_ARCHITECTURE.md` §6). This
/// function never calls `wal::purge_before` and never touches the WAL in
/// any way (`PHASE4B_ADR.md` ADR-P4B-1).
pub fn write_from_memtable(
    memtable: &MemTable,
    id: u64,
    sstables_dir: &Path,
    config: &SsTableWriterConfig,
) -> Result<SstableMeta> {
    fs::create_dir_all(sstables_dir)?;
    let tmp_path = sstables_dir.join(sstable_tmp_filename(id));
    let final_path = sstables_dir.join(sstable_filename(id));

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)?;

    let entry_count = memtable.entry_count() as u64;
    let mut bloom = BloomFilter::new_for_key_count(entry_count.max(1), config.bloom_bits_per_key);

    let mut offset: u64 = 0;
    let mut index_entries = Vec::new();
    let mut block_records_buf: Vec<u8> = Vec::new();
    let mut block_record_count: u32 = 0;
    let mut block_last_key: Vec<u8> = Vec::new();
    let mut total_records: u64 = 0;

    for ((key, seq), value) in
        memtable.range(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
    {
        let (op, value_bytes): (u8, &[u8]) = match value {
            MemtableValue::Put(v) => (OP_PUT, v.as_slice()),
            MemtableValue::Tombstone => (OP_DELETE, &[]),
        };
        format::encode_record(&mut block_records_buf, key, *seq, op, value_bytes)?;
        block_record_count += 1;
        block_last_key = key.clone();
        total_records += 1;
        bloom.insert(key);

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

    let (min_seq, max_seq) = memtable.seq_range().unwrap_or((0, 0));
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
