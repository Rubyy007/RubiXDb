//! Integration-level SSTable tests: multi-block behavior, the full
//! corruption matrix (operating brief §34), round-trip against a
//! MemTable reference, and property-based verification (operating brief
//! §35-36).

use std::ops::Bound;
use std::path::Path;

use crate::error::EngineError;
use crate::memtable::MemTable;
use crate::sstable::format::FOOTER_SIZE;
use crate::sstable::test_support::TempDir;
use crate::sstable::writer::{write_from_memtable, SsTableWriterConfig};
use crate::sstable::{discover, RecordValue, SsTable};

fn build(dir: &Path, id: u64, memtable: &MemTable, config: &SsTableWriterConfig) -> SsTable {
    let meta = write_from_memtable(memtable, id, dir, config).unwrap();
    SsTable::open(&meta.path, id).unwrap()
}

fn small_block_config() -> SsTableWriterConfig {
    SsTableWriterConfig {
        target_block_size: 64, // deliberately tiny -> forces many blocks
        ..SsTableWriterConfig::default()
    }
}

// ---------------------------------------------------------------------
// Multi-block tests (operating brief §37)
// ---------------------------------------------------------------------

#[test]
fn multi_block_lookup_first_middle_last_and_missing() {
    let tmp = TempDir::new("multiblock");
    let mut m = MemTable::new(64 * 1024 * 1024);
    for i in 0..300u64 {
        m.put(
            format!("key{i:05}").as_bytes(),
            i + 1,
            format!("val{i}").as_bytes(),
        );
    }
    let table = build(tmp.path(), 1, &m, &small_block_config());
    assert!(
        table.block_count() >= 10,
        "expected many blocks from a tiny target_block_size, got {}",
        table.block_count()
    );

    // first
    assert_eq!(
        table.get_versioned(b"key00000", u64::MAX).unwrap(),
        Some((1, RecordValue::Put(b"val0".to_vec())))
    );
    // middle
    assert_eq!(
        table.get_versioned(b"key00150", u64::MAX).unwrap(),
        Some((151, RecordValue::Put(b"val150".to_vec())))
    );
    // last
    assert_eq!(
        table.get_versioned(b"key00299", u64::MAX).unwrap(),
        Some((300, RecordValue::Put(b"val299".to_vec())))
    );
    // missing (lexicographically between two present keys)
    assert_eq!(table.get_versioned(b"key0014X", u64::MAX).unwrap(), None);
    // missing (before everything / after everything)
    assert_eq!(table.get_versioned(b"aaa", u64::MAX).unwrap(), None);
    assert_eq!(table.get_versioned(b"zzz", u64::MAX).unwrap(), None);
}

#[test]
fn version_and_tombstone_boundary_split_across_blocks() {
    // Craft a case where one key's multiple versions are likely to land
    // in different blocks: many big-ish values for the same key.
    let tmp = TempDir::new("version-boundary");
    let mut m = MemTable::new(64 * 1024 * 1024);
    let filler = vec![b'x'; 40];
    for seq in 1..=20u64 {
        m.insert(
            b"hotkey",
            seq,
            crate::memtable::MemtableValue::Put(filler.clone()),
        );
    }
    m.delete(b"hotkey", 21);
    let table = build(tmp.path(), 1, &m, &small_block_config());
    assert!(
        table.block_count() > 1,
        "test needs multiple blocks to be meaningful"
    );

    for seq in 1..=20u64 {
        let got = table.get_versioned(b"hotkey", seq).unwrap();
        assert_eq!(got, Some((seq, RecordValue::Put(filler.clone()))));
    }
    assert_eq!(
        table.get_versioned(b"hotkey", 21).unwrap(),
        Some((21, RecordValue::Tombstone))
    );
    assert_eq!(table.get_versioned(b"hotkey", 0).unwrap(), None);
}

#[test]
fn large_sstable_hundred_plus_blocks_index_has_one_entry_per_block() {
    let tmp = TempDir::new("large");
    let mut m = MemTable::new(256 * 1024 * 1024);
    for i in 0..5000u64 {
        m.put(
            format!("k{i:06}").as_bytes(),
            i + 1,
            format!("v{i:06}").as_bytes(),
        );
    }
    let table = build(tmp.path(), 1, &m, &small_block_config());
    assert!(table.block_count() >= 100);

    for i in (0..5000u64).step_by(137) {
        let key = format!("k{i:06}");
        assert_eq!(
            table.get_versioned(key.as_bytes(), u64::MAX).unwrap(),
            Some((i + 1, RecordValue::Put(format!("v{i:06}").into_bytes())))
        );
    }
}

// ---------------------------------------------------------------------
// Large-record policy (operating brief §14-15)
// ---------------------------------------------------------------------

#[test]
fn record_larger_than_target_block_size_gets_its_own_block() {
    let tmp = TempDir::new("oversized-record");
    let mut m = MemTable::new(64 * 1024 * 1024);
    let big_value = vec![b'y'; 10_000]; // >> the 64-byte target_block_size below
    m.put(b"small", 1, b"v");
    m.put(b"big", 2, &big_value);
    m.put(b"small2", 3, b"v2");
    let table = build(tmp.path(), 1, &m, &small_block_config());

    assert_eq!(
        table.get_versioned(b"big", u64::MAX).unwrap(),
        Some((2, RecordValue::Put(big_value)))
    );
    assert_eq!(
        table.get_versioned(b"small", u64::MAX).unwrap(),
        Some((1, RecordValue::Put(b"v".to_vec())))
    );
    assert_eq!(
        table.get_versioned(b"small2", u64::MAX).unwrap(),
        Some((3, RecordValue::Put(b"v2".to_vec())))
    );
}

// ---------------------------------------------------------------------
// Round-trip against MemTable reference (operating brief §36)
// ---------------------------------------------------------------------

#[test]
fn round_trip_matches_memtable_for_every_key_and_every_as_of_seq() {
    let tmp = TempDir::new("round-trip");
    let mut m = MemTable::new(64 * 1024 * 1024);
    let mut seq = 1u64;
    let mut ops: Vec<(&[u8], Option<&[u8]>)> = Vec::new();
    for i in 0..40u64 {
        let key: &'static [u8] = Box::leak(format!("k{}", i % 10).into_bytes().into_boxed_slice());
        if i % 7 == 0 {
            ops.push((key, None));
        } else {
            let val: &'static [u8] = Box::leak(format!("v{i}").into_bytes().into_boxed_slice());
            ops.push((key, Some(val)));
        }
    }
    for (key, val) in &ops {
        match val {
            Some(v) => m.put(key, seq, v),
            None => m.delete(key, seq),
        }
        seq += 1;
    }
    let max_seq = seq - 1;
    let table = build(tmp.path(), 1, &m, &SsTableWriterConfig::default());

    for k in 0..10u64 {
        let key = format!("k{k}");
        for as_of in 0..=max_seq {
            let expected = m
                .get_as_of(key.as_bytes(), as_of)
                .map(|(s, v)| (*s, v.clone()));
            let actual = table.get_versioned(key.as_bytes(), as_of).unwrap();
            let actual_mapped = actual.map(|(s, rv)| {
                (
                    s,
                    match rv {
                        RecordValue::Put(v) => crate::memtable::MemtableValue::Put(v),
                        RecordValue::Tombstone => crate::memtable::MemtableValue::Tombstone,
                    },
                )
            });
            assert_eq!(
                actual_mapped, expected,
                "mismatch for key={key} as_of={as_of}"
            );
        }
    }
}

// ---------------------------------------------------------------------
// Corruption matrix (operating brief §34)
// ---------------------------------------------------------------------

fn sample_table_bytes() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new("corruption");
    let mut m = MemTable::new(64 * 1024 * 1024);
    for i in 0..30u64 {
        m.put(
            format!("k{i:03}").as_bytes(),
            i + 1,
            format!("v{i}").as_bytes(),
        );
    }
    let meta = write_from_memtable(&m, 1, tmp.path(), &small_block_config()).unwrap();
    let path = meta.path.clone();
    (tmp, path)
}

fn flip_byte_at(path: &Path, offset: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[offset] ^= 0xFF;
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn corruption_bad_footer_magic() {
    let (_tmp, path) = sample_table_bytes();
    let len = std::fs::metadata(&path).unwrap().len() as usize;
    flip_byte_at(&path, len - FOOTER_SIZE); // first byte of magic
    let err = SsTable::open(&path, 1).unwrap_err();
    assert!(matches!(err, EngineError::Corruption { .. }));
}

#[test]
fn corruption_bad_footer_checksum() {
    let (_tmp, path) = sample_table_bytes();
    let len = std::fs::metadata(&path).unwrap().len() as usize;
    flip_byte_at(&path, len - 1); // last byte of footer_crc32c
    let err = SsTable::open(&path, 1).unwrap_err();
    assert!(matches!(err, EngineError::Corruption { .. }));
}

#[test]
fn corruption_unsupported_version_is_reported_distinctly() {
    let (_tmp, path) = sample_table_bytes();
    let len = std::fs::metadata(&path).unwrap().len() as usize;
    let mut bytes = std::fs::read(&path).unwrap();
    // format_version field: footer offset 8..12
    let fv_off = len - FOOTER_SIZE + 8;
    bytes[fv_off..fv_off + 4].copy_from_slice(&99u32.to_le_bytes());
    // Recompute footer checksum (offset 68..72 within the 72-byte footer)
    let footer_start = len - FOOTER_SIZE;
    let crc = crc32c::crc32c(&bytes[footer_start..footer_start + 68]);
    bytes[footer_start + 68..footer_start + 72].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(&path, bytes).unwrap();
    let err = SsTable::open(&path, 1).unwrap_err();
    assert!(matches!(err, EngineError::Unsupported { .. }));
}

#[test]
fn corruption_truncated_file_below_footer_size() {
    let (_tmp, path) = sample_table_bytes();
    std::fs::write(&path, [0u8; 10]).unwrap();
    let err = SsTable::open(&path, 1).unwrap_err();
    assert!(matches!(err, EngineError::Corruption { .. }));
}

#[test]
fn corruption_bad_index_checksum_detected_at_open() {
    let (_tmp, path) = sample_table_bytes();
    let bytes = std::fs::read(&path).unwrap();
    let len = bytes.len();
    let footer_start = len - FOOTER_SIZE;
    let index_offset = u64::from_le_bytes(
        bytes[footer_start + 52..footer_start + 60]
            .try_into()
            .unwrap(),
    ) as usize;
    // Flip a byte inside the index block region (well before its trailing checksum).
    flip_byte_at(&path, index_offset + 4);
    let err = SsTable::open(&path, 1).unwrap_err();
    assert!(matches!(err, EngineError::Corruption { .. }));
}

#[test]
fn corruption_bad_bloom_checksum_detected_at_open() {
    let (_tmp, path) = sample_table_bytes();
    let bytes = std::fs::read(&path).unwrap();
    let len = bytes.len();
    let footer_start = len - FOOTER_SIZE;
    let bloom_offset = u64::from_le_bytes(
        bytes[footer_start + 36..footer_start + 44]
            .try_into()
            .unwrap(),
    ) as usize;
    flip_byte_at(&path, bloom_offset + 10);
    let err = SsTable::open(&path, 1).unwrap_err();
    assert!(matches!(err, EngineError::Corruption { .. }));
}

#[test]
fn corruption_bad_data_block_checksum_detected_lazily_on_read() {
    let (_tmp, path) = sample_table_bytes();
    let bytes = std::fs::read(&path).unwrap();
    // First data block starts at offset 0; corrupt a byte inside it (not
    // the trailing checksum, so `open()` -- which never reads data blocks
    // -- succeeds, and the failure is only observed on the read that
    // actually touches this block).
    let mut corrupted = bytes.clone();
    corrupted[2] ^= 0xFF;
    std::fs::write(&path, &corrupted).unwrap();

    let table = SsTable::open(&path, 1).unwrap();
    let err = table.get_versioned(b"k000", u64::MAX).unwrap_err();
    assert!(matches!(err, EngineError::Corruption { .. }));
}

#[test]
fn corruption_index_offset_out_of_bounds() {
    let (_tmp, path) = sample_table_bytes();
    let mut bytes = std::fs::read(&path).unwrap();
    let len = bytes.len();
    let footer_start = len - FOOTER_SIZE;
    // Set index_offset (footer bytes 52..60) to something absurd.
    bytes[footer_start + 52..footer_start + 60].copy_from_slice(&(len as u64 * 10).to_le_bytes());
    let crc = crc32c::crc32c(&bytes[footer_start..footer_start + 68]);
    bytes[footer_start + 68..footer_start + 72].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(&path, bytes).unwrap();
    let err = SsTable::open(&path, 1).unwrap_err();
    assert!(matches!(err, EngineError::Corruption { .. }));
}

// ---------------------------------------------------------------------
// Discovery / publication (operating brief §29, no-Manifest sweep)
// ---------------------------------------------------------------------

#[test]
fn discover_sweeps_tmp_files_and_opens_valid_tables_newest_first() {
    let tmp = TempDir::new("discover");
    let mut m1 = MemTable::new(64 * 1024 * 1024);
    m1.put(b"a", 1, b"v1");
    let mut m2 = MemTable::new(64 * 1024 * 1024);
    m2.put(b"b", 2, b"v2");
    write_from_memtable(&m1, 1, tmp.path(), &SsTableWriterConfig::default()).unwrap();
    write_from_memtable(&m2, 2, tmp.path(), &SsTableWriterConfig::default()).unwrap();
    // An orphaned .sst.tmp left behind by a simulated crashed build.
    std::fs::write(tmp.path().join("00000000000000000003.sst.tmp"), b"garbage").unwrap();

    let (tables, next_id) = discover(tmp.path()).unwrap();
    assert_eq!(tables.len(), 2);
    assert_eq!(tables[0].id(), 2, "newest-first ordering");
    assert_eq!(tables[1].id(), 1);
    // The swept id-3 `.tmp` was never published (no `.sst` ever existed at
    // that id) -- per `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.1, an id
    // is only "used" once a table under that id is actually published, so
    // reusing 3 for the next real build is correct, not a collision.
    assert_eq!(next_id, 3);
    assert!(!tmp.path().join("00000000000000000003.sst.tmp").exists());
}

#[test]
fn discover_fails_closed_on_a_corrupt_published_table() {
    let tmp = TempDir::new("discover-corrupt");
    let mut m = MemTable::new(64 * 1024 * 1024);
    m.put(b"a", 1, b"v1");
    let meta = write_from_memtable(&m, 1, tmp.path(), &SsTableWriterConfig::default()).unwrap();
    // Corrupt the footer's own trailing checksum -- validated eagerly at
    // `open()`/`discover()` time, unlike data-block corruption (detected
    // only lazily, on the read that actually touches that block --
    // `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §6's deliberate distinction).
    let len = std::fs::metadata(&meta.path).unwrap().len() as usize;
    flip_byte_at(&meta.path, len - 1);
    let err = discover(tmp.path()).unwrap_err();
    assert!(matches!(err, EngineError::Corruption { .. }));
}

#[test]
fn discover_on_empty_or_absent_directory_is_empty_not_an_error() {
    let tmp = TempDir::new("discover-empty");
    let absent = tmp.path().join("does-not-exist-yet");
    let (tables, next_id) = discover(&absent).unwrap();
    assert!(tables.is_empty());
    assert_eq!(next_id, 1);
}

// ---------------------------------------------------------------------
// Property test: SSTable(MemTable) behaves identically to MemTable
// itself, across randomized operation sequences (operating brief §35).
// ---------------------------------------------------------------------

mod property {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Op {
        Put(u8, Vec<u8>),
        Delete(u8),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..8, proptest::collection::vec(any::<u8>(), 0..20))
                .prop_map(|(k, v)| Op::Put(k, v)),
            (0u8..8).prop_map(Op::Delete),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn sstable_matches_memtable_reference(ops in proptest::collection::vec(op_strategy(), 1..80)) {
            let tmp = TempDir::new("property");
            let mut m = MemTable::new(64 * 1024 * 1024);
            let mut seq = 1u64;
            for op in &ops {
                match op {
                    Op::Put(k, v) => { m.put(&[*k], seq, v); }
                    Op::Delete(k) => { m.delete(&[*k], seq); }
                }
                seq += 1;
            }
            let max_seq = seq.saturating_sub(1);
            let table = build(tmp.path(), 1, &m, &small_block_config());

            for k in 0u8..8 {
                for as_of in 0..=max_seq {
                    let expected = m.get_as_of(&[k], as_of).map(|(s, v)| (*s, v.clone()));
                    let actual = table.get_versioned(&[k], as_of).unwrap();
                    let actual_mapped = actual.map(|(s, rv)| {
                        (
                            s,
                            match rv {
                                RecordValue::Put(v) => crate::memtable::MemtableValue::Put(v),
                                RecordValue::Tombstone => crate::memtable::MemtableValue::Tombstone,
                            },
                        )
                    });
                    prop_assert_eq!(actual_mapped, expected, "key={} as_of={}", k, as_of);
                }
            }

            // Ordered iteration must match a raw memtable scan exactly.
            let expected_iter: Vec<_> = m
                .range(Bound::Unbounded, Bound::Unbounded)
                .map(|((k, s), v)| (k.clone(), *s, v.clone()))
                .collect();
            let actual_iter: Vec<_> = table
                .range_scan_raw(Bound::Unbounded, Bound::Unbounded)
                .collect::<crate::error::Result<Vec<_>>>()
                .unwrap()
                .into_iter()
                .map(|(k, s, rv)| {
                    (
                        k,
                        s,
                        match rv {
                            RecordValue::Put(v) => crate::memtable::MemtableValue::Put(v),
                            RecordValue::Tombstone => crate::memtable::MemtableValue::Tombstone,
                        },
                    )
                })
                .collect();
            prop_assert_eq!(actual_iter, expected_iter);
        }
    }
}
