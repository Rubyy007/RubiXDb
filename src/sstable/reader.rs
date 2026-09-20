//! RUBIC SSTable reader — `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.10.
//! Footer/bloom/index are validated and held for the table's lifetime;
//! data blocks are read on demand, one at a time, via positional reads
//! (never a shared file cursor — safe for concurrent readers sharing one
//! `Arc<SsTable>` without any lock, mirroring `wal::file_io`'s existing
//! `write_all_at` positional-write pattern for the read direction).

use std::fs::File;
use std::io;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{EngineError, Result};
use crate::sstable::bloom::BloomFilter;
use crate::sstable::format::{self, DecodedRecord, Footer, IndexEntry, FOOTER_SIZE, OP_PUT};
use crate::sstable::RecordValue;

fn corrupt(detail: impl Into<String>) -> EngineError {
    EngineError::Corruption {
        detail: detail.into(),
    }
}

/// Positional read, exact-length, no shared cursor mutation — safe to
/// call concurrently from multiple threads against the same `File`
/// (mirrors `wal::file_io::WalFile::write_all_at`'s doc comment on why
/// this holds on both platforms).
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        match FileExt::seek_read(file, &mut buf[read..], offset + read as u64) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "read_exact_at: seek_read returned 0 before buffer was full",
                ))
            }
            Ok(n) => read += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// An open, fully-validated RUBIC SSTable. Immutable for its entire
/// lifetime (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.4) — every field
/// describing the table's own on-disk-derived data (`footer`/`bloom`/
/// `index`) is set once, at `open()`, and never mutated again. The two
/// `AtomicU64` counters below are the one deliberate exception:
/// `ADR-RE-001` §12 read-path observability, incremented from `&self`
/// methods via interior mutability, never affecting what a lookup
/// returns — see `read_block`/`get_versioned` for the exact, single
/// counting points (one well-defined point per counter, not
/// instrumentation scattered through every call site).
#[derive(Debug)]
pub struct SsTable {
    id: u64,
    path: PathBuf,
    file: File,
    footer: Footer,
    bloom: BloomFilter,
    index: Vec<IndexEntry>,
    /// Cumulative count of `read_block` calls that returned `Ok` for
    /// this table — shared by `get_versioned` and `range_scan_raw`
    /// (both call the same `read_block`), so this one counter already
    /// covers both without separate instrumentation at each call site.
    blocks_read: AtomicU64,
    /// Cumulative count of `get_versioned` calls that returned early on
    /// `!bloom.might_contain(key)` — the fast, zero-I/O miss path.
    bloom_negative_count: AtomicU64,
}

impl SsTable {
    /// Opens and fully validates `path`: footer (magic, version,
    /// checksum), then bloom and index blocks (checksum + internal
    /// consistency, `format::decode_index_block`'s contiguity/ordering
    /// checks). Data blocks are **not** read here — bounded memory at
    /// `open()` regardless of table size (operating brief §20/§38).
    pub fn open(path: &Path, id: u64) -> Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < FOOTER_SIZE as u64 {
            return Err(corrupt(format!(
                "footer: file shorter than FOOTER_SIZE ({len} < {FOOTER_SIZE})"
            )));
        }
        let mut footer_buf = [0u8; FOOTER_SIZE];
        read_exact_at(&file, &mut footer_buf, len - FOOTER_SIZE as u64)?;
        let footer = Footer::decode(&footer_buf)?;

        let usable_len = len - FOOTER_SIZE as u64;
        let bloom_end = footer
            .bloom_offset
            .checked_add(footer.bloom_length)
            .ok_or_else(|| corrupt("footer: bloom_offset + bloom_length overflow"))?;
        if bloom_end > usable_len {
            return Err(corrupt("footer: bloom block extends past the file"));
        }
        let index_end = footer
            .index_offset
            .checked_add(footer.index_length)
            .ok_or_else(|| corrupt("footer: index_offset + index_length overflow"))?;
        if index_end > usable_len {
            return Err(corrupt("footer: index block extends past the file"));
        }
        if footer.index_offset != bloom_end {
            return Err(corrupt(
                "footer: index block does not immediately follow the bloom block",
            ));
        }
        if index_end != usable_len {
            return Err(corrupt(
                "footer: index block does not end exactly at the footer",
            ));
        }

        let bloom_len_usize: usize = footer
            .bloom_length
            .try_into()
            .map_err(|_| corrupt("footer: bloom_length implausibly large"))?;
        let mut bloom_buf = vec![0u8; bloom_len_usize];
        read_exact_at(&file, &mut bloom_buf, footer.bloom_offset)?;
        let decoded_bloom = format::decode_bloom_block(&bloom_buf)?;
        let bloom = BloomFilter::from_parts(
            decoded_bloom.num_bits,
            decoded_bloom.num_hash_functions,
            decoded_bloom.bits,
        );

        let index_len_usize: usize = footer
            .index_length
            .try_into()
            .map_err(|_| corrupt("footer: index_length implausibly large"))?;
        let mut index_buf = vec![0u8; index_len_usize];
        read_exact_at(&file, &mut index_buf, footer.index_offset)?;
        let index = format::decode_index_block(&index_buf, footer.bloom_offset)?;

        Ok(SsTable {
            id,
            path: path.to_path_buf(),
            file,
            footer,
            bloom,
            index,
            blocks_read: AtomicU64::new(0),
            bloom_negative_count: AtomicU64::new(0),
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn min_seq(&self) -> u64 {
        self.footer.min_seq
    }
    pub fn max_seq(&self) -> u64 {
        self.footer.max_seq
    }
    pub fn record_count(&self) -> u64 {
        self.footer.record_count
    }
    pub fn block_count(&self) -> usize {
        self.index.len()
    }

    /// `ADR-RE-001` §12 read-path observability — cumulative count of
    /// successful `read_block` calls against this table (shared by
    /// `get_versioned` and `range_scan_raw`). Purely observational,
    /// never consulted by any correctness decision.
    pub fn blocks_read(&self) -> u64 {
        self.blocks_read.load(Ordering::Relaxed)
    }

    /// `ADR-RE-001` §12 read-path observability — cumulative count of
    /// `get_versioned` calls that short-circuited on a bloom-filter
    /// negative (zero block reads). Purely observational.
    pub fn bloom_negative_count(&self) -> u64 {
        self.bloom_negative_count.load(Ordering::Relaxed)
    }

    fn read_block(&self, entry: &IndexEntry) -> Result<Vec<DecodedRecord>> {
        let len: usize = entry
            .block_length
            .try_into()
            .map_err(|_| corrupt("index: block_length implausibly large"))?;
        let mut buf = vec![0u8; len];
        read_exact_at(&self.file, &mut buf, entry.block_offset)?;
        let decoded = format::decode_block(&buf)?;
        self.blocks_read.fetch_add(1, Ordering::Relaxed);
        Ok(decoded)
    }

    /// LSM Engine Spec §2.8's `get_versioned` algorithm, extended to
    /// correctly handle a key whose version run spans a block boundary
    /// (the sparse index only records each block's *last* key, so a key
    /// equal to a block's last key might continue into the next block —
    /// see `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.6's discussion of
    /// this edge case, and the spec's own test-checklist item 7). This
    /// never changes which single block is consulted for the common
    /// case (a key entirely inside one block); it only extends the
    /// candidate set forward while consecutive blocks share the exact
    /// same last key.
    pub fn get_versioned(&self, key: &[u8], as_of_seq: u64) -> Result<Option<(u64, RecordValue)>> {
        if !self.bloom.might_contain(key) {
            self.bloom_negative_count.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        if self.index.is_empty() {
            return Ok(None);
        }
        // `partition_point` finds the *leftmost* index whose `last_key >=
        // key` in this non-decreasing sequence (`format::decode_index_
        // block` allows, and the boundary-extension loop below requires,
        // consecutive entries with an *identical* `last_key`). A plain
        // `binary_search_by` must NOT be used here: on a tie (multiple
        // blocks sharing the same `last_key`), it is permitted to return
        // any matching index, not necessarily the leftmost one -- which
        // would silently skip earlier blocks that also hold this key.
        let mut idx = self.index.partition_point(|e| e.last_key.as_slice() < key);
        if idx >= self.index.len() {
            // key is greater than every block's last_key: absent.
            return Ok(None);
        }

        let mut candidates = vec![idx];
        while self.index[idx].last_key.as_slice() == key && idx + 1 < self.index.len() {
            idx += 1;
            candidates.push(idx);
        }

        let mut best: Option<(u64, RecordValue)> = None;
        for &ci in &candidates {
            let records = self.read_block(&self.index[ci])?;
            for r in records {
                if r.key == key
                    && r.seq <= as_of_seq
                    && best.as_ref().is_none_or(|(bs, _)| r.seq > *bs)
                {
                    let value = if r.op == OP_PUT {
                        RecordValue::Put(r.value)
                    } else {
                        RecordValue::Tombstone
                    };
                    best = Some((r.seq, value));
                }
            }
        }
        Ok(best)
    }

    /// `ADR-RE-001` §10/§2's `contains` primitive: identical bloom +
    /// sparse-index + candidate-block walk as `get_versioned` above, but
    /// the per-record scan never constructs an owned `RecordValue` for
    /// the caller — it inspects `r.op` and discards `r.value` in place,
    /// so no value bytes are moved past this function's stack frame.
    /// Returns `Ok(None)` when this table has no visible record for
    /// `key` at `as_of_seq` at all (bloom-negative, index-absent, or no
    /// candidate block held a matching record); `Ok(Some(is_put))`
    /// otherwise, distinguishing a live value from a tombstone so the
    /// caller (`LsmEngine::contains`) can apply the merge-layer
    /// recency rule the exact same way `get_as_of` does.
    ///
    /// Note (measured, not assumed — see `PHASE_READ_ENGINE_PERFORMANCE.md`
    /// §"contains vs get_as_of"): `read_block`/`format::decode_block`
    /// eagerly decodes every record's key *and* value bytes for a whole
    /// block regardless of which method is called, so this cannot avoid
    /// that allocation — only the final per-record `RecordValue::Put`
    /// construction on the winning record, which in `get_versioned` is
    /// itself already just a move, not a clone. The benchmark reports
    /// whatever difference this actually produces rather than assuming
    /// one.
    pub fn contains_versioned(&self, key: &[u8], as_of_seq: u64) -> Result<Option<bool>> {
        if !self.bloom.might_contain(key) {
            self.bloom_negative_count.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        if self.index.is_empty() {
            return Ok(None);
        }
        let mut idx = self.index.partition_point(|e| e.last_key.as_slice() < key);
        if idx >= self.index.len() {
            return Ok(None);
        }

        let mut candidates = vec![idx];
        while self.index[idx].last_key.as_slice() == key && idx + 1 < self.index.len() {
            idx += 1;
            candidates.push(idx);
        }

        let mut best: Option<(u64, bool)> = None;
        for &ci in &candidates {
            let records = self.read_block(&self.index[ci])?;
            for r in &records {
                if r.key == key && r.seq <= as_of_seq && best.is_none_or(|(bs, _)| r.seq > bs) {
                    best = Some((r.seq, r.op == OP_PUT));
                }
            }
        }
        Ok(best.map(|(_, is_put)| is_put))
    }

    /// Ordered iteration over `[start, end)`, bounded memory (one block
    /// materialized at a time, never the whole table — operating brief
    /// §20/§23). Stops safely and yields exactly one `Err` on the first
    /// corrupted block it encounters, never continuing through
    /// potentially untrusted offsets afterward.
    pub fn range_scan_raw<'a>(
        &'a self,
        start: Bound<&'a [u8]>,
        end: Bound<&'a [u8]>,
    ) -> RangeScanRaw<'a> {
        let start_block = match start {
            Bound::Unbounded => 0,
            // Leftmost match, same reasoning as `get_versioned` above: a
            // key spanning several consecutive blocks must not have its
            // earlier blocks skipped just because a tied `last_key` was
            // matched further right.
            Bound::Included(k) | Bound::Excluded(k) => {
                self.index.partition_point(|e| e.last_key.as_slice() < k)
            }
        };
        RangeScanRaw {
            table: self,
            next_block_idx: start_block,
            current: Vec::new().into_iter(),
            start,
            end,
            done: start_block >= self.index.len(),
        }
    }
}

pub struct RangeScanRaw<'a> {
    table: &'a SsTable,
    next_block_idx: usize,
    current: std::vec::IntoIter<DecodedRecord>,
    start: Bound<&'a [u8]>,
    end: Bound<&'a [u8]>,
    done: bool,
}

impl<'a> RangeScanRaw<'a> {
    fn in_start_bound(&self, key: &[u8]) -> bool {
        match self.start {
            Bound::Unbounded => true,
            Bound::Included(k) => key >= k,
            Bound::Excluded(k) => key > k,
        }
    }
    fn in_end_bound(&self, key: &[u8]) -> bool {
        match self.end {
            Bound::Unbounded => true,
            Bound::Included(k) => key <= k,
            Bound::Excluded(k) => key < k,
        }
    }
}

impl<'a> Iterator for RangeScanRaw<'a> {
    type Item = Result<(Vec<u8>, u64, RecordValue)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            if let Some(r) = self.current.next() {
                if !self.in_start_bound(&r.key) {
                    continue;
                }
                if !self.in_end_bound(&r.key) {
                    self.done = true;
                    return None;
                }
                let value = if r.op == OP_PUT {
                    RecordValue::Put(r.value)
                } else {
                    RecordValue::Tombstone
                };
                return Some(Ok((r.key, r.seq, value)));
            }
            if self.next_block_idx >= self.table.index.len() {
                self.done = true;
                return None;
            }
            let entry = &self.table.index[self.next_block_idx];
            self.next_block_idx += 1;
            match self.table.read_block(entry) {
                Ok(records) => self.current = records.into_iter(),
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtable::MemTable;
    use crate::sstable::test_support::TempDir;
    use crate::sstable::writer::{write_from_memtable, SsTableWriterConfig};

    fn build_table(dir: &Path, id: u64, memtable: &MemTable) -> SsTable {
        let meta = write_from_memtable(memtable, id, dir, &SsTableWriterConfig::default()).unwrap();
        SsTable::open(&meta.path, id).unwrap()
    }

    #[test]
    fn point_lookup_across_versions() {
        let tmp = TempDir::new("point-lookup");
        let mut m = MemTable::new(64 * 1024 * 1024);
        m.put(b"k1", 1, b"v1");
        m.put(b"k1", 5, b"v5");
        m.delete(b"k1", 10);
        let table = build_table(tmp.path(), 1, &m);

        assert_eq!(
            table.get_versioned(b"k1", 3).unwrap(),
            Some((1, RecordValue::Put(b"v1".to_vec())))
        );
        assert_eq!(
            table.get_versioned(b"k1", 5).unwrap(),
            Some((5, RecordValue::Put(b"v5".to_vec())))
        );
        assert_eq!(
            table.get_versioned(b"k1", 10).unwrap(),
            Some((10, RecordValue::Tombstone))
        );
        assert_eq!(table.get_versioned(b"k1", 0).unwrap(), None);
        assert_eq!(table.get_versioned(b"missing", 100).unwrap(), None);
    }

    #[test]
    fn range_scan_matches_memtable_content() {
        let tmp = TempDir::new("range-scan");
        let mut m = MemTable::new(64 * 1024 * 1024);
        for i in 0..50u64 {
            m.put(
                format!("k{i:04}").as_bytes(),
                i + 1,
                format!("v{i}").as_bytes(),
            );
        }
        let table = build_table(tmp.path(), 1, &m);

        let results: Vec<_> = table
            .range_scan_raw(Bound::Unbounded, Bound::Unbounded)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 50);
        for (i, (key, seq, value)) in results.iter().enumerate() {
            assert_eq!(key, format!("k{i:04}").as_bytes());
            assert_eq!(*seq, i as u64 + 1);
            assert_eq!(value, &RecordValue::Put(format!("v{i}").into_bytes()));
        }
    }

    #[test]
    fn range_scan_respects_bounds() {
        let tmp = TempDir::new("range-scan-bounds");
        let mut m = MemTable::new(64 * 1024 * 1024);
        for i in 0..20u64 {
            m.put(format!("k{i:04}").as_bytes(), i + 1, b"v");
        }
        let table = build_table(tmp.path(), 1, &m);
        let results: Vec<_> = table
            .range_scan_raw(
                Bound::Included(b"k0005".as_slice()),
                Bound::Excluded(b"k0010".as_slice()),
            )
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].0, b"k0005");
        assert_eq!(results[4].0, b"k0009");
    }

    #[test]
    fn open_rejects_truncated_file() {
        let tmp = TempDir::new("truncated");
        let path = tmp.path().join("bad.sst");
        std::fs::write(&path, [0u8; 10]).unwrap();
        let err = SsTable::open(&path, 1).unwrap_err();
        assert!(matches!(err, EngineError::Corruption { .. }));
    }
}
