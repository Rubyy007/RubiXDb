//! In-memory `(user_key, seq)`-ordered map with tombstone semantics, per
//! `RubixDB-LSM-Engine-Specification-v1.0.md` §1 ("Status: Final — ready
//! for implementation" — followed exactly; see `PHASE4A_MEMTABLE_
//! ARCHITECTURE.md` §2 for why no `SkipList` evaluation was performed).
//!
//! # Durability
//!
//! A `MemTable` is **not a durability mechanism**. It has no `fsync`, no
//! on-disk representation, and no crash-recovery logic of its own — the
//! WAL (`crate::wal`) remains the sole source of durability and sequence
//! assignment (`PHASE4A_ARCHITECTURE.md` §2/§5). `MemTable::insert` is
//! called only *after* a record's durability has already been confirmed
//! by the write path (or, during recovery, only for records the WAL
//! itself already proved durable) — never before, never as a substitute.
//!
//! # Concurrency
//!
//! Not internally synchronized — same single-writer principle as
//! `FileWal` (WAL Spec §9). Every mutating method takes `&mut self`;
//! the caller (`crate::lsm`) is responsible for serializing writers
//! (naturally satisfied by the Dedicated Batch Coordinator funneling
//! every write through one thread — `PHASE4A_MEMTABLE_ARCHITECTURE.md`
//! §3) and for exposing safe concurrent reads (a `RwLock<MemTable>` for
//! the mutable instance; no lock at all for a frozen `Arc<MemTable>`,
//! which the type system already guarantees cannot be mutated again).

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

#[cfg(test)]
mod property_tests;

/// A value stored in the memtable for one `(user_key, seq)` pair — either
/// a live value or a tombstone marking a deletion. Maps directly from
/// `crate::wal::WalOpOwned::{Put{value,..} -> Put(value), Delete{..} ->
/// Tombstone}` — deliberately not a duplicate representation of the same
/// WAL operation (`PHASE4A_MEMTABLE_ARCHITECTURE.md` §1): `op` byte
/// values `1 = PUT`, `2 = DELETE` are shared with the WAL by design
/// (LSM Engine Spec §0.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemtableValue {
    Put(Vec<u8>),
    Tombstone,
}

impl MemtableValue {
    /// The logical payload size this value contributes to memory
    /// accounting (§`ENTRY_OVERHEAD` below for the rest) — the value
    /// bytes themselves for `Put`, zero for `Tombstone` (LSM Engine Spec
    /// §1.3's `value_payload_len`).
    fn payload_len(&self) -> usize {
        match self {
            MemtableValue::Put(v) => v.len(),
            MemtableValue::Tombstone => 0,
        }
    }
}

/// A fixed, documented, conservative estimate for the `seq: u64` (8
/// bytes), the `MemtableValue` enum discriminant, and `BTreeMap` node
/// overhead per entry — LSM Engine Spec §1.3. **Not exact physical
/// allocation** (this project never claims it is — operating brief §17's
/// own caution): it only needs to be a stable, conservative trigger for
/// `is_full()`, not a byte-accurate accounting of the allocator's own
/// behavior.
pub const ENTRY_OVERHEAD: usize = 32;

/// The default `max_size_bytes` when a caller does not specify one — LSM
/// Engine Spec §4.1's own `LsmConfig::memtable_max_size_bytes` default.
/// **Not** the "~64 MiB" the Phase 4A operating brief described as "the
/// prior design" — that figure does not match the current, final spec
/// (which predates any actual MemTable implementation and therefore has
/// no "prior design" to remember); see `PHASE4A_ADR.md` for the full
/// account of this flagged discrepancy and why the spec's own value
/// governs.
pub const DEFAULT_MAX_SIZE_BYTES: usize = 4 * 1024 * 1024;

/// `entry_size(key, value) = key.len() + value_payload_len +
/// ENTRY_OVERHEAD` — LSM Engine Spec §1.3, exactly.
fn entry_size(key: &[u8], value: &MemtableValue) -> usize {
    key.len() + value.payload_len() + ENTRY_OVERHEAD
}

/// A single in-memory sorted structure, keyed by `(user_key, seq)`
/// ascending — LSM Engine Spec §1.1. Required, not a simplification:
/// until flushed, a memtable must be able to answer a snapshot read
/// (`as_of_seq` in the past) that needs an *older* version of a key
/// overwritten later in the same memtable's own lifetime.
#[derive(Debug, Default)]
pub struct MemTable {
    map: BTreeMap<(Vec<u8>, u64), MemtableValue>,
    size_bytes: usize,
    max_size_bytes: usize,
    min_seq: Option<u64>,
    max_seq: Option<u64>,
}

impl MemTable {
    pub fn new(max_size_bytes: usize) -> Self {
        MemTable {
            map: BTreeMap::new(),
            size_bytes: 0,
            max_size_bytes,
            min_seq: None,
            max_seq: None,
        }
    }

    /// Low-level, used both by normal writes and by WAL replay during
    /// recovery (LSM Engine Spec §1.2's own doc comment). `insert` is
    /// never called twice with an identical `(key, seq)` pair in normal
    /// operation — WAL `seq` values are unique per key by construction;
    /// recovery (streaming WAL replay, `PHASE4A_MEMTABLE_ARCHITECTURE.md`
    /// §10) guarantees this holds during replay too, since it never
    /// replays the same durable record twice.
    pub fn insert(&mut self, key: &[u8], seq: u64, value: MemtableValue) {
        self.size_bytes += entry_size(key, &value);
        self.min_seq = Some(self.min_seq.map_or(seq, |m| m.min(seq)));
        self.max_seq = Some(self.max_seq.map_or(seq, |m| m.max(seq)));
        self.map.insert((key.to_vec(), seq), value);
    }

    pub fn put(&mut self, key: &[u8], seq: u64, value: &[u8]) {
        self.insert(key, seq, MemtableValue::Put(value.to_vec()));
    }

    pub fn delete(&mut self, key: &[u8], seq: u64) {
        self.insert(key, seq, MemtableValue::Tombstone);
    }

    /// Latest version as of "now" — equivalent to
    /// `get_as_of(key, u64::MAX)` (LSM Engine Spec §1.2).
    pub fn get(&self, key: &[u8]) -> Option<(&u64, &MemtableValue)> {
        self.get_as_of(key, u64::MAX)
    }

    /// Highest `seq <= as_of_seq` for this key, or `None` if this
    /// memtable has no version of the key at or before `as_of_seq` — LSM
    /// Engine Spec §1.2, exactly. `range((key,0)..=(key,as_of_seq))`
    /// isolates exactly this key's versions with `seq <= as_of_seq`
    /// (`(key, seq)` tuples for a fixed `key` are contiguous and ordered
    /// by `seq`), and `.next_back()` — the last element of that
    /// sub-range — is by construction the *highest* qualifying `seq`. No
    /// special-casing, no manual binary search.
    pub fn get_as_of(&self, key: &[u8], as_of_seq: u64) -> Option<(&u64, &MemtableValue)> {
        self.map
            .range((key.to_vec(), 0)..=(key.to_vec(), as_of_seq))
            .next_back()
            .map(|((_, seq), value)| (seq, value))
    }

    /// Resolved lookup (`PHASE4A_MEMTABLE_ARCHITECTURE.md` §6): a
    /// visible tombstone means "not found," even if older values exist —
    /// operating brief §9/§14's own rule. Layered on top of the raw
    /// `get_as_of` above (which deliberately returns the tombstone
    /// itself — version resolution and tombstone filtering are the
    /// caller's responsibility per LSM Engine Spec §1.2's own doc
    /// comment on `range`, applied consistently here too).
    pub fn get_value(&self, key: &[u8], as_of_seq: u64) -> Option<&[u8]> {
        match self.get_as_of(key, as_of_seq) {
            Some((_, MemtableValue::Put(v))) => Some(v),
            Some((_, MemtableValue::Tombstone)) => None,
            None => None,
        }
    }

    /// Raw iteration, ALL versions, in `(key asc, seq asc)` order —
    /// version resolution and tombstone filtering are the caller's
    /// (future LSM facade's) responsibility, not the memtable's (LSM
    /// Engine Spec §1.2/§4.2). This is also the exact interface a future
    /// RUBIC SSTable writer consumes (LSM Engine Spec §2.7 step 1) — no
    /// internal tree node or block-layout detail is exposed.
    pub fn range(
        &self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> impl Iterator<Item = (&(Vec<u8>, u64), &MemtableValue)> {
        let start = bound_to_tuple_start(start);
        let end = bound_to_tuple_end(end);
        self.map.range((start, end))
    }

    pub fn size_bytes(&self) -> usize {
        self.size_bytes
    }

    pub fn max_size_bytes(&self) -> usize {
        self.max_size_bytes
    }

    /// Phase 4A addition (operating brief §17: "expose... remaining
    /// capacity"), not in the original spec text but directly implied by
    /// it — `size_bytes`/`max_size_bytes` already exist; this is a pure
    /// derived accessor over them, no new state.
    pub fn remaining_capacity(&self) -> usize {
        self.max_size_bytes.saturating_sub(self.size_bytes)
    }

    /// Phase 4A addition (operating brief §17: "expose... entry count").
    pub fn entry_count(&self) -> usize {
        self.map.len()
    }

    pub fn is_full(&self) -> bool {
        self.size_bytes >= self.max_size_bytes
    }

    pub fn seq_range(&self) -> Option<(u64, u64)> {
        match (self.min_seq, self.max_seq) {
            (Some(min), Some(max)) => Some((min, max)),
            _ => None,
        }
    }

    /// Consumes `self` and returns an immutably-shared handle. A
    /// compile-time guarantee, not a runtime flag: once frozen, there is
    /// no code path left in the type system that can mutate this
    /// memtable again (LSM Engine Spec §1.2).
    pub fn freeze(self) -> Arc<MemTable> {
        Arc::new(self)
    }
}

/// Maps a key-level *start* (lower) bound to a `(key, seq)` tuple bound.
/// `Included(k)` must admit every version of `k` (seq >= 0, the
/// minimum), so it maps to `Included((k, 0))`. `Excluded(k)` must
/// exclude every version of `k` -- not just the one at some arbitrary
/// sentinel seq -- so the tuple bound must sit *above* every real
/// version of `k`, i.e. `Excluded((k, u64::MAX))`.
///
/// Fixed 2026-09-20 (`ADR-RE-001`/`PHASE_READ_ENGINE_ARCHITECTURE_
/// REPORT.md` §14.2): the previous single-function `bound_to_tuple`
/// used the *same* sentinel (`seq_at_unbounded`) for both `Included`
/// and `Excluded`, which is only correct for `Included` -- `Excluded(k)`
/// was silently mapping to `Excluded((k, 0))`, and since every real
/// entry has `seq >= 1`, `(k, real_seq) > (k, 0)` always holds, so an
/// "excluded" boundary key's own entries were never actually excluded.
/// Caught by a regression test (`range_excluded_start_bound_excludes_
/// every_version_of_the_boundary_key`) added *before* this fix, per
/// this project's own "test first, then the smallest correct fix"
/// convention for a confirmed production bug.
fn bound_to_tuple_start(bound: Bound<&[u8]>) -> Bound<(Vec<u8>, u64)> {
    match bound {
        Bound::Included(k) => Bound::Included((k.to_vec(), 0)),
        Bound::Excluded(k) => Bound::Excluded((k.to_vec(), u64::MAX)),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// The *end* (upper) bound's mirror image of `bound_to_tuple_start`:
/// `Included(k)` must admit every version of `k`, so it maps to
/// `Included((k, u64::MAX))` (above every real version). `Excluded(k)`
/// must exclude every version of `k`, so it maps to `Excluded((k, 0))`
/// (at or below every real version, since real `seq` is always >= 1).
fn bound_to_tuple_end(bound: Bound<&[u8]>) -> Bound<(Vec<u8>, u64)> {
    match bound {
        Bound::Included(k) => Bound::Included((k.to_vec(), u64::MAX)),
        Bound::Excluded(k) => Bound::Excluded((k.to_vec(), 0)),
        Bound::Unbounded => Bound::Unbounded,
    }
}

// Compile-time-only guarantee (LSM Engine Spec §1.6 test 8): `Arc<MemTable>`
// must expose no `&mut self` method — if this ever became false, the
// commented-out call below would start compiling, which is the actual
// property under test. Left commented deliberately; do not "fix" this
// by adding a runtime check, which is not what this test verifies.
//
// fn _freeze_is_truly_immutable(m: Arc<MemTable>) {
//     m.put(b"k", 1, b"v"); // must NOT compile: `put` requires `&mut self`
// }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_n_keys_get_them_all_back_with_correct_values() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        for i in 0..50u64 {
            m.put(
                format!("k{i:03}").as_bytes(),
                i + 1,
                format!("v{i}").as_bytes(),
            );
        }
        for i in 0..50u64 {
            let value = m.get_value(format!("k{i:03}").as_bytes(), u64::MAX);
            assert_eq!(value, Some(format!("v{i}").as_bytes()));
        }
    }

    #[test]
    fn delete_returns_tombstone_not_silently_absent() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        m.put(b"k1", 1, b"v1");
        m.delete(b"k1", 2);
        assert_eq!(m.get_value(b"k1", u64::MAX), None);
        // The raw lookup must show the tombstone explicitly, not just "None".
        assert_eq!(m.get(b"k1"), Some((&2, &MemtableValue::Tombstone)));
    }

    #[test]
    fn multiple_versions_get_as_of_returns_the_version_current_at_that_point() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        m.put(b"k1", 1, b"v1");
        m.put(b"k1", 5, b"v5");
        m.put(b"k1", 10, b"v10");

        assert_eq!(m.get_value(b"k1", 1), Some(b"v1".as_slice()));
        assert_eq!(m.get_value(b"k1", 3), Some(b"v1".as_slice()));
        assert_eq!(m.get_value(b"k1", 5), Some(b"v5".as_slice()));
        assert_eq!(m.get_value(b"k1", 9), Some(b"v5".as_slice()));
        assert_eq!(m.get_value(b"k1", 10), Some(b"v10".as_slice()));
        assert_eq!(m.get_value(b"k1", 100), Some(b"v10".as_slice()));
    }

    #[test]
    fn as_of_seq_before_first_write_returns_none() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        m.put(b"k1", 10, b"v1");
        assert_eq!(m.get_value(b"k1", 9), None);
        assert_eq!(m.get_as_of(b"k1", 9), None);
    }

    #[test]
    fn put_delete_put_resolves_correctly_at_every_boundary() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        m.put(b"k1", 1, b"v1");
        m.delete(b"k1", 2);
        m.put(b"k1", 3, b"v3");

        assert_eq!(m.get_value(b"k1", 1), Some(b"v1".as_slice()));
        assert_eq!(
            m.get_value(b"k1", 2),
            None,
            "tombstone must be visible as not-found"
        );
        assert_eq!(m.get_value(b"k1", 3), Some(b"v3".as_slice()));
    }

    #[test]
    fn size_bytes_grows_by_exactly_entry_size_per_insert() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        let mut expected = 0usize;
        for i in 0..20u64 {
            let key = format!("key{i}");
            let value = format!("value{i}");
            expected += key.len() + value.len() + ENTRY_OVERHEAD;
            m.put(key.as_bytes(), i + 1, value.as_bytes());
            assert_eq!(m.size_bytes(), expected);
        }
    }

    #[test]
    fn tombstone_entry_size_has_zero_payload_len() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        m.delete(b"k1", 1);
        assert_eq!(m.size_bytes(), 2 + ENTRY_OVERHEAD);
    }

    #[test]
    fn is_full_flips_at_the_configured_threshold() {
        let mut m = MemTable::new(50);
        assert!(!m.is_full());
        m.put(b"k", 1, b"v"); // 1 + 1 + 32 = 34 bytes
        assert!(!m.is_full());
        m.put(b"k2", 2, b"v2"); // +2+2+32=36 -> total 70 >= 50
        assert!(m.is_full());
    }

    #[test]
    fn range_returns_strictly_sorted_key_seq_order() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        m.put(b"b", 1, b"v");
        m.put(b"a", 2, b"v");
        m.put(b"a", 1, b"v");
        m.put(b"c", 1, b"v");

        let collected: Vec<(Vec<u8>, u64)> = m
            .range(Bound::Unbounded, Bound::Unbounded)
            .map(|((k, s), _)| (k.clone(), *s))
            .collect();
        assert_eq!(
            collected,
            vec![
                (b"a".to_vec(), 1),
                (b"a".to_vec(), 2),
                (b"b".to_vec(), 1),
                (b"c".to_vec(), 1),
            ]
        );
    }

    /// `ADR-RE-001`/`PHASE_READ_ENGINE_ARCHITECTURE_REPORT.md` §14.2's
    /// recorded discrepancy, investigated and confirmed a real bug (not
    /// intentional behavior) before fixing: `std::ops::Bound::Excluded(x)`
    /// has one, universal, unambiguous meaning -- "up to but not
    /// including `x`." `range()` takes exactly this standard type, so it
    /// must honor that contract for both the start and end bound,
    /// regardless of how many distinct `seq` versions the boundary key
    /// has.
    #[test]
    fn range_excluded_end_bound_excludes_every_version_of_the_boundary_key() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        m.put(b"k0010", 1, b"v");
        m.put(b"k0020", 2, b"v"); // the boundary key -- multiple things could
        m.put(b"k0020", 3, b"v"); // go wrong if only one of its two versions
                                  // were (correctly or incorrectly) excluded.
        m.put(b"k0030", 4, b"v");

        let keys: Vec<Vec<u8>> = m
            .range(Bound::Included(b"k0010"), Bound::Excluded(b"k0020"))
            .map(|((k, _), _)| k.clone())
            .collect();
        assert_eq!(
            keys,
            vec![b"k0010".to_vec()],
            "Excluded(k0020) as an end bound must exclude ALL versions of k0020, not just \
             the version whose seq happens to be below some internal sentinel"
        );
    }

    /// Same investigation, the START-bound direction (also affected by
    /// the same underlying `bound_to_tuple` sentinel mismatch, though
    /// not the specific case the architecture report's own bounded-range
    /// test happened to surface first).
    #[test]
    fn range_excluded_start_bound_excludes_every_version_of_the_boundary_key() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        m.put(b"k0010", 1, b"v");
        m.put(b"k0010", 2, b"v");
        m.put(b"k0020", 3, b"v");

        let keys: Vec<Vec<u8>> = m
            .range(Bound::Excluded(b"k0010"), Bound::Unbounded)
            .map(|((k, _), _)| k.clone())
            .collect();
        assert_eq!(
            keys,
            vec![b"k0020".to_vec()],
            "Excluded(k0010) as a start bound must exclude ALL versions of k0010"
        );
    }

    #[test]
    fn empty_memtable_behaves_correctly_without_panicking() {
        let m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        assert_eq!(m.get(b"k"), None);
        assert_eq!(m.get_as_of(b"k", 100), None);
        assert_eq!(m.range(Bound::Unbounded, Bound::Unbounded).count(), 0);
        assert_eq!(m.size_bytes(), 0);
        assert!(!m.is_full());
        assert_eq!(m.seq_range(), None);
        assert_eq!(m.entry_count(), 0);
        assert_eq!(m.remaining_capacity(), m.max_size_bytes());
    }

    #[test]
    fn freeze_produces_a_shared_immutable_handle_preserving_state() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        m.put(b"k1", 1, b"v1");
        m.put(b"k2", 2, b"v2");
        let size_before = m.size_bytes();
        let seq_range_before = m.seq_range();

        let frozen: Arc<MemTable> = m.freeze();
        assert_eq!(frozen.size_bytes(), size_before);
        assert_eq!(frozen.seq_range(), seq_range_before);
        assert_eq!(frozen.get_value(b"k1", u64::MAX), Some(b"v1".as_slice()));

        // Multiple readers can share the frozen handle concurrently —
        // proven by construction (Arc + no &mut methods), exercised here
        // for real across real threads.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let frozen = Arc::clone(&frozen);
                std::thread::spawn(move || frozen.get_value(b"k2", u64::MAX).map(|v| v.to_vec()))
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), Some(b"v2".to_vec()));
        }
    }

    #[test]
    fn seq_range_tracks_min_and_max_across_out_of_order_inserts() {
        let mut m = MemTable::new(DEFAULT_MAX_SIZE_BYTES);
        m.put(b"a", 5, b"v");
        m.put(b"b", 2, b"v");
        m.put(b"c", 9, b"v");
        assert_eq!(m.seq_range(), Some((2, 9)));
    }

    #[test]
    fn remaining_capacity_and_entry_count_are_accurate() {
        let mut m = MemTable::new(100);
        assert_eq!(m.remaining_capacity(), 100);
        m.put(b"k", 1, b"v"); // 1+1+32=34
        assert_eq!(m.entry_count(), 1);
        assert_eq!(m.remaining_capacity(), 66);
        m.delete(b"k2", 2); // 2+0+32=34
        assert_eq!(m.entry_count(), 2);
        assert_eq!(m.remaining_capacity(), 32);
    }
}
