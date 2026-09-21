//! Module-level Compaction tests: the version/tombstone retention rule
//! (brief §7's worked truth table, `PHASE_COMPACTION_ARCHITECTURE_
//! REPORT.md` §11) and basic k-way merge mechanics, against real,
//! on-disk `SsTable`s (never mocked) but without a full `LsmEngine` --
//! `LsmEngine`-level integration (Manifest transition, live-list
//! splice, physical deletion, crash windows, concurrency, storage
//! pressure) is covered separately in `src/lsm/tests.rs`, since those
//! properties only exist at that layer.

use std::path::Path;
use std::sync::Arc;

use crate::compaction::{merge, retain_versions, MergeStats};
use crate::memtable::MemTable;
use crate::sstable::test_support::TempDir;
use crate::sstable::{write_from_memtable, RecordValue, SsTable, SsTableWriterConfig};

fn build_table(dir: &Path, id: u64, memtable: &MemTable) -> Arc<SsTable> {
    let meta = write_from_memtable(memtable, id, dir, &SsTableWriterConfig::default()).unwrap();
    Arc::new(SsTable::open(&meta.path, id).unwrap())
}

fn collect_merge(
    inputs: &[Arc<SsTable>],
    oldest_live_snapshot_seq: Option<u64>,
) -> Vec<(Vec<u8>, u64, RecordValue)> {
    let (iter, _stats) = merge(inputs, oldest_live_snapshot_seq);
    iter.collect::<crate::error::Result<Vec<_>>>().unwrap()
}

// ---------------------------------------------------------------------
// `retain_versions` -- the worked truth table, brief §7 / architecture
// report §11: PUT@1, PUT@2, DELETE@3, PUT@4.
// ---------------------------------------------------------------------

fn versions_413() -> Vec<(u64, RecordValue)> {
    vec![
        (1, RecordValue::Put(b"v1".to_vec())),
        (2, RecordValue::Put(b"v2".to_vec())),
        (3, RecordValue::Tombstone),
        (4, RecordValue::Put(b"v4".to_vec())),
    ]
}

#[test]
fn retention_no_live_snapshot_keeps_only_the_newest() {
    let mut stats = MergeStats::default();
    let kept = retain_versions(versions_413(), None, &mut stats);
    assert_eq!(kept, vec![(4, RecordValue::Put(b"v4".to_vec()))]);
    assert_eq!(stats.records_retained, 1);
    assert_eq!(stats.versions_dropped, 2); // v1, v2
    assert_eq!(stats.tombstones_dropped, 1); // the @3 tombstone
}

/// **Discovered during implementation, documented rather than silently
/// fixed**: `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §11's own worked
/// table listed this row's expected survivors as just `{v1, v4}` --
/// correct only under the unstated assumption that snapshot@1 is the
/// *only* live snapshot. `oldest_live_snapshot_seq()` exposes only the
/// *minimum* live snapshot seq (`ADR-COMPACTION-001` Decision 5's own
/// API choice), never the full set -- so `retain_versions` cannot
/// distinguish "exactly one live snapshot at seq=1" from "several live
/// snapshots, the oldest at seq=1, with others at seq=2 or seq=3 this
/// function has no visibility into." The only safe, correct behavior
/// given that limited information is to retain *everything* from the
/// floor (here, v1 itself, since no version's `seq` is `<= 1` until v1)
/// through the newest -- exactly what the implemented algorithm does.
/// This test asserts the actual, correct, conservative behavior, not
/// the architecture report's own under-specified row.
#[test]
fn retention_snapshot_at_1_keeps_everything_from_the_floor_forward() {
    let mut stats = MergeStats::default();
    let kept = retain_versions(versions_413(), Some(1), &mut stats);
    assert_eq!(
        kept,
        versions_413(),
        "the floor here is v1 itself (no earlier version exists) -- every version from the \
         floor through the newest must be conservatively retained, since retain_versions \
         cannot rule out an unseen live snapshot at seq 2 or 3"
    );
    assert_eq!(stats.records_retained, 4);
    assert_eq!(stats.versions_dropped, 0);
    assert_eq!(stats.tombstones_dropped, 0);
}

/// Same discovered-during-implementation correction as the `@1` test
/// above, for the `@2` row.
#[test]
fn retention_snapshot_at_2_keeps_everything_from_the_floor_forward() {
    let mut stats = MergeStats::default();
    let kept = retain_versions(versions_413(), Some(2), &mut stats);
    assert_eq!(
        kept,
        vec![
            (2, RecordValue::Put(b"v2".to_vec())),
            (3, RecordValue::Tombstone),
            (4, RecordValue::Put(b"v4".to_vec())),
        ],
        "the floor here is v2 (seq=2 <= 2) -- v1 (seq=1, strictly older than the floor) is the \
         only version unreachable by any live snapshot and may be dropped; the tombstone@3 must \
         be conservatively retained since retain_versions cannot rule out an unseen live \
         snapshot at seq=3"
    );
    assert_eq!(stats.versions_dropped, 1); // v1 only
    assert_eq!(stats.tombstones_dropped, 0);
}

#[test]
fn retention_snapshot_at_3_keeps_the_tombstone_and_v4() {
    let mut stats = MergeStats::default();
    let kept = retain_versions(versions_413(), Some(3), &mut stats);
    assert_eq!(
        kept,
        vec![
            (3, RecordValue::Tombstone),
            (4, RecordValue::Put(b"v4".to_vec())),
        ]
    );
}

#[test]
fn retention_snapshot_at_4_keeps_only_v4() {
    let mut stats = MergeStats::default();
    let kept = retain_versions(versions_413(), Some(4), &mut stats);
    assert_eq!(kept, vec![(4, RecordValue::Put(b"v4".to_vec()))]);
}

/// The boundary case `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §11's
/// own simplified worked examples did not exercise: a live snapshot
/// pinned *strictly between* two versions, not exactly at one's own
/// `seq`. Discovered during implementation (documented, not hidden) --
/// see `retain_versions`'s own doc comment for the full derivation.
#[test]
fn retention_snapshot_strictly_between_two_versions_keeps_the_floor() {
    // versions at seq 3, 5, 8, 12, 15; live snapshot at seq=10 (between
    // 8 and 12). The oldest live snapshot itself resolves to the
    // version at seq=8 (highest version <= 10) -- that version MUST
    // survive, even though 8 < 10. A naive "seq < oldest => drop" rule
    // would incorrectly drop it.
    let versions = vec![
        (3, RecordValue::Put(b"a".to_vec())),
        (5, RecordValue::Put(b"b".to_vec())),
        (8, RecordValue::Put(b"c".to_vec())),
        (12, RecordValue::Put(b"d".to_vec())),
        (15, RecordValue::Put(b"e".to_vec())),
    ];
    let mut stats = MergeStats::default();
    let kept = retain_versions(versions, Some(10), &mut stats);
    assert_eq!(
        kept,
        vec![
            (8, RecordValue::Put(b"c".to_vec())),
            (12, RecordValue::Put(b"d".to_vec())),
            (15, RecordValue::Put(b"e".to_vec())),
        ],
        "the floor version (seq=8, the oldest live snapshot's own answer) must survive, along \
         with everything newer; only seq 3 and 5 (unreachable by any live snapshot, since the \
         oldest live snapshot's as_of_seq is 10 > 8) may be dropped"
    );
}

#[test]
fn retention_snapshot_older_than_every_version_keeps_everything() {
    // The key didn't exist yet as of the oldest live snapshot -- every
    // version could, in the worst case, be some other live snapshot's
    // own answer, so nothing is safe to drop except via the "newest"
    // rule, which doesn't apply here since there's no supersession
    // below any of them relative to a snapshot this old.
    let versions = vec![
        (3, RecordValue::Put(b"a".to_vec())),
        (5, RecordValue::Put(b"b".to_vec())),
        (8, RecordValue::Put(b"c".to_vec())),
    ];
    let mut stats = MergeStats::default();
    let kept = retain_versions(versions.clone(), Some(1), &mut stats);
    assert_eq!(kept, versions);
    assert_eq!(stats.versions_dropped, 0);
    assert_eq!(stats.tombstones_dropped, 0);
}

#[test]
fn retention_empty_input_is_empty_output() {
    let mut stats = MergeStats::default();
    let kept = retain_versions(Vec::new(), Some(5), &mut stats);
    assert!(kept.is_empty());
}

// ---------------------------------------------------------------------
// Merge mechanics -- real on-disk SSTables.
// ---------------------------------------------------------------------

#[test]
fn single_table_merge_reproduces_its_own_live_content() {
    let tmp = TempDir::new("compaction-single");
    let mut m = MemTable::new(64 * 1024 * 1024);
    for i in 0..20u64 {
        m.put(
            format!("k{i:03}").as_bytes(),
            i + 1,
            format!("v{i}").as_bytes(),
        );
    }
    let table = build_table(tmp.path(), 1, &m);
    let merged = collect_merge(&[Arc::clone(&table)], None);
    assert_eq!(
        merged.len(),
        20,
        "no live snapshot -- every key's newest (only) version survives"
    );
    for (i, (key, seq, value)) in merged.iter().enumerate() {
        assert_eq!(key, format!("k{i:03}").as_bytes());
        assert_eq!(*seq, i as u64 + 1);
        assert_eq!(value, &RecordValue::Put(format!("v{i}").into_bytes()));
    }
}

#[test]
fn two_table_merge_overlapping_keys_newer_table_wins() {
    let tmp = TempDir::new("compaction-two-overlap");
    let mut older = MemTable::new(64 * 1024 * 1024);
    older.put(b"a", 1, b"old-a");
    older.put(b"b", 2, b"old-b");
    let mut newer = MemTable::new(64 * 1024 * 1024);
    newer.put(b"b", 5, b"new-b");
    newer.put(b"c", 6, b"new-c");
    let table_old = build_table(tmp.path(), 1, &older);
    let table_new = build_table(tmp.path(), 2, &newer);

    // Newest-first, matching `LsmEngine.sstables`' own convention --
    // though `merge`'s correctness does not depend on input order
    // (every version, from every source, for a key is gathered
    // regardless of which slot it came from).
    let merged = collect_merge(&[table_new, table_old], None);
    assert_eq!(
        merged,
        vec![
            (b"a".to_vec(), 1, RecordValue::Put(b"old-a".to_vec())),
            (b"b".to_vec(), 5, RecordValue::Put(b"new-b".to_vec())),
            (b"c".to_vec(), 6, RecordValue::Put(b"new-c".to_vec())),
        ]
    );
}

#[test]
fn two_table_merge_disjoint_ranges() {
    let tmp = TempDir::new("compaction-two-disjoint");
    let mut t1 = MemTable::new(64 * 1024 * 1024);
    t1.put(b"a", 1, b"va");
    let mut t2 = MemTable::new(64 * 1024 * 1024);
    t2.put(b"z", 2, b"vz");
    let table1 = build_table(tmp.path(), 1, &t1);
    let table2 = build_table(tmp.path(), 2, &t2);

    let merged = collect_merge(&[table2, table1], None);
    assert_eq!(
        merged,
        vec![
            (b"a".to_vec(), 1, RecordValue::Put(b"va".to_vec())),
            (b"z".to_vec(), 2, RecordValue::Put(b"vz".to_vec())),
        ]
    );
}

#[test]
fn many_table_merge_tombstone_across_tables_suppresses_older_put() {
    let tmp = TempDir::new("compaction-many-tombstone");
    let mut t1 = MemTable::new(64 * 1024 * 1024);
    t1.put(b"k", 1, b"v1");
    let mut t2 = MemTable::new(64 * 1024 * 1024);
    t2.delete(b"k", 2);
    let mut t3 = MemTable::new(64 * 1024 * 1024);
    t3.put(b"other", 3, b"vo");
    let table1 = build_table(tmp.path(), 1, &t1);
    let table2 = build_table(tmp.path(), 2, &t2);
    let table3 = build_table(tmp.path(), 3, &t3);

    // No live snapshot -- the tombstone (the newest version of `k`)
    // survives (tombstones are first-class versions, never silently
    // dropped just for being a delete), but must never be emitted as
    // a visible row by the Read Engine (that collapse happens at the
    // `LsmEngine`/`get`/`range_scan` layer, unchanged, not here).
    let merged = collect_merge(&[table3, table2, table1], None);
    assert_eq!(
        merged,
        vec![
            (b"k".to_vec(), 2, RecordValue::Tombstone),
            (b"other".to_vec(), 3, RecordValue::Put(b"vo".to_vec())),
        ],
        "the tombstone itself (the newest version of k) must survive as a real record in the \
         merge output -- it is compaction's job only to decide *retention*, never to also \
         perform the get()/range_scan()-level tombstone-to-None/absent collapse"
    );
}

#[test]
fn merge_propagates_corruption_from_an_input_and_stops() {
    let tmp = TempDir::new("compaction-corrupt-input");
    let mut m = MemTable::new(64 * 1024 * 1024);
    for i in 0..10u64 {
        m.put(format!("k{i:03}").as_bytes(), i + 1, b"v");
    }
    let meta = write_from_memtable(&m, 1, tmp.path(), &SsTableWriterConfig::default()).unwrap();
    // Flip a byte in the first data block, exactly like the existing
    // corruption tests elsewhere in this codebase.
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&meta.path)
            .unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[byte[0] ^ 0xFF]).unwrap();
    }
    let table = Arc::new(SsTable::open(&meta.path, 1).unwrap());
    let tables = [table];
    let (mut iter, _stats) = merge(&tables, None);
    let mut saw_err = false;
    for item in iter.by_ref() {
        if item.is_err() {
            saw_err = true;
            break;
        }
    }
    assert!(
        saw_err,
        "a corrupted input block must surface as a real Err, not be silently skipped"
    );
    assert!(
        iter.next().is_none(),
        "the merge must end after the first error -- no partial/silent continuation"
    );
}
