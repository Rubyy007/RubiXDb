//! Randomized property tests (operating brief §25/§30): apply the same
//! sequence of operations to a `MemTable` and to a deliberately naive,
//! obviously-correct reference model, then assert every query produces
//! identical answers. Mirrors `wal::fuzz_tests`'s own established
//! pattern (`proptest`, dev-dependency only, ≥1,000 cases by default,
//! `ProptestConfig::with_cases`) — reused, not reinvented.
//!
//! **Reproducibility**: `proptest` itself persists a failing input's
//! seed to `proptest-regressions/memtable/property_tests.txt` on any
//! failure and replays it automatically on the next run — this project's
//! existing, already-relied-upon mechanism (`wal::fuzz_tests`'s own doc
//! comment references the same guarantee), not a new one built for this
//! module.

use proptest::collection::vec as pvec;
use proptest::prelude::*;

use super::{MemTable, MemtableValue};

/// A small, `proptest`-generatable operation — deliberately simpler than
/// `WalOpOwned` (no need for `CheckpointMarker` here; this module tests
/// the memtable in isolation, not WAL integration).
#[derive(Debug, Clone)]
enum FuzzOp {
    Put { key_idx: u8, value: Vec<u8> },
    Delete { key_idx: u8 },
}

fn fuzz_op_strategy() -> impl Strategy<Value = FuzzOp> {
    prop_oneof![
        (0u8..8, pvec(any::<u8>(), 0..16))
            .prop_map(|(key_idx, value)| FuzzOp::Put { key_idx, value }),
        (0u8..8).prop_map(|key_idx| FuzzOp::Delete { key_idx }),
    ]
}

fn key_for(idx: u8) -> Vec<u8> {
    format!("key-{idx}").into_bytes()
}

/// The reference model: every `(seq, MemtableValue)` ever applied to a
/// key, in application order — queried by the most obviously-correct
/// possible algorithm (a full linear scan + max), not the memtable's own
/// range-query trick, so the two implementations cannot share a bug.
#[derive(Debug, Default)]
struct ReferenceModel {
    history: std::collections::HashMap<Vec<u8>, Vec<(u64, MemtableValue)>>,
}

impl ReferenceModel {
    fn apply(&mut self, key: &[u8], seq: u64, value: MemtableValue) {
        self.history
            .entry(key.to_vec())
            .or_default()
            .push((seq, value));
    }

    /// Deliberately naive: scan every version of this key, keep the one
    /// with the highest `seq <= as_of_seq`. O(versions), not O(log n) —
    /// that's the point; it must be obviously correct, not fast.
    fn get_as_of(&self, key: &[u8], as_of_seq: u64) -> Option<(u64, MemtableValue)> {
        self.history
            .get(key)?
            .iter()
            .filter(|(seq, _)| *seq <= as_of_seq)
            .max_by_key(|(seq, _)| *seq)
            .cloned()
    }

    fn get_value(&self, key: &[u8], as_of_seq: u64) -> Option<Vec<u8>> {
        match self.get_as_of(key, as_of_seq) {
            Some((_, MemtableValue::Put(v))) => Some(v),
            Some((_, MemtableValue::Tombstone)) => None,
            None => None,
        }
    }

    /// All (key, seq) pairs ever applied, sorted — the same ordering
    /// contract `MemTable::range` promises.
    fn all_sorted(&self) -> Vec<(Vec<u8>, u64, MemtableValue)> {
        let mut all: Vec<(Vec<u8>, u64, MemtableValue)> = self
            .history
            .iter()
            .flat_map(|(k, versions)| {
                versions
                    .iter()
                    .map(move |(seq, v)| (k.clone(), *seq, v.clone()))
            })
            .collect();
        all.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        all
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Operating brief §25/§30: Put/Get, Delete/Get, Put/Delete/Put,
    /// multiple versions, ordering — all folded into one property, since
    /// they are all instances of "apply operations with increasing seq,
    /// then every get_as_of query must match the naive reference model."
    #[test]
    fn memtable_matches_naive_reference_model(ops in pvec(fuzz_op_strategy(), 0..200)) {
        let mut memtable = MemTable::new(usize::MAX); // no size limit — this test is about correctness, not capacity
        let mut reference = ReferenceModel::default();

        for (i, op) in ops.iter().enumerate() {
            let seq = (i as u64) + 1; // WAL-style: strictly increasing, matching real usage
            match op {
                FuzzOp::Put { key_idx, value } => {
                    let key = key_for(*key_idx);
                    memtable.put(&key, seq, value);
                    reference.apply(&key, seq, MemtableValue::Put(value.clone()));
                }
                FuzzOp::Delete { key_idx } => {
                    let key = key_for(*key_idx);
                    memtable.delete(&key, seq);
                    reference.apply(&key, seq, MemtableValue::Tombstone);
                }
            }
        }

        let max_seq = ops.len() as u64;
        // Query every key at every seq boundary actually exercised, plus
        // 0 (before anything) and max_seq+1 (after everything) — this
        // covers every interesting snapshot boundary without needing a
        // second randomized dimension.
        for key_idx in 0u8..8 {
            let key = key_for(key_idx);
            for as_of_seq in 0..=(max_seq + 1) {
                let actual = memtable.get_value(&key, as_of_seq);
                let expected = reference.get_value(&key, as_of_seq);
                prop_assert_eq!(
                    actual.map(|v| v.to_vec()),
                    expected,
                    "mismatch for key={:?} as_of_seq={}",
                    key,
                    as_of_seq
                );

                // Also check the raw (unresolved) lookup, so a tombstone-
                // handling bug inside get_value can't hide behind a
                // coincidentally-matching resolved answer.
                let actual_raw = memtable.get_as_of(&key, as_of_seq).map(|(s, v)| (*s, v.clone()));
                let expected_raw = reference.get_as_of(&key, as_of_seq);
                prop_assert_eq!(
                    actual_raw,
                    expected_raw,
                    "raw mismatch for key={:?} as_of_seq={}",
                    key,
                    as_of_seq
                );
            }
        }

        // Ordering (operating brief §15/§30): range() must yield exactly
        // the reference model's own (key asc, seq asc) sorted order.
        let actual_range: Vec<(Vec<u8>, u64, MemtableValue)> = memtable
            .range(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
            .map(|((k, s), v)| (k.clone(), *s, v.clone()))
            .collect();
        prop_assert_eq!(actual_range, reference.all_sorted());

        // size_bytes / entry_count must be internally consistent
        // (operating brief §17) regardless of the random operation mix.
        prop_assert_eq!(memtable.entry_count(), reference.history.values().map(|v| v.len()).sum::<usize>());
    }

    /// Freeze (operating brief §30): a frozen memtable's answers to every
    /// query must be identical to the mutable memtable's answers
    /// immediately before freezing — freezing changes mutability, never
    /// the logical content.
    #[test]
    fn freeze_preserves_every_query_answer(ops in pvec(fuzz_op_strategy(), 0..100)) {
        let mut memtable = MemTable::new(usize::MAX);
        for (i, op) in ops.iter().enumerate() {
            let seq = (i as u64) + 1;
            match op {
                FuzzOp::Put { key_idx, value } => memtable.put(&key_for(*key_idx), seq, value),
                FuzzOp::Delete { key_idx } => memtable.delete(&key_for(*key_idx), seq),
            }
        }

        let mut before: Vec<Option<Vec<u8>>> = Vec::new();
        let max_seq = ops.len() as u64;
        for key_idx in 0u8..8 {
            for as_of_seq in 0..=(max_seq + 1) {
                before.push(memtable.get_value(&key_for(key_idx), as_of_seq).map(|v| v.to_vec()));
            }
        }
        let size_before = memtable.size_bytes();
        let seq_range_before = memtable.seq_range();

        let frozen = memtable.freeze();

        let mut after: Vec<Option<Vec<u8>>> = Vec::new();
        for key_idx in 0u8..8 {
            for as_of_seq in 0..=(max_seq + 1) {
                after.push(frozen.get_value(&key_for(key_idx), as_of_seq).map(|v| v.to_vec()));
            }
        }
        prop_assert_eq!(before, after);
        prop_assert_eq!(frozen.size_bytes(), size_before);
        prop_assert_eq!(frozen.seq_range(), seq_range_before);
    }
}
