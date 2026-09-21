//! Size-tiered, full-merge compaction, per LSM Engine Spec Section 5
//! and `ADR-COMPACTION-001`. Increment 1 built the deterministic core
//! operation; Increment 2 wires it to a real, automatic background
//! trigger (`LsmEngine`'s own compaction worker thread, `src/lsm/
//! mod.rs`'s `spawn_compaction_thread`/`compact_once_impl`) — this
//! module itself is unchanged by that wiring: it owns only the merge
//! algorithm, the retention rule, and the output-record stream, none
//! of which know about `LsmEngine`, threads, or triggers at all.

use std::collections::VecDeque;
use std::ops::Bound;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::Result;
use crate::sstable::{RecordValue, SsTable, SsTableRangeCursor};

/// `ADR-COMPACTION-001` Decision 16 — one instance per compaction
/// cycle (not a running total like `ReadStats`). Every field is drawn
/// directly from the phase brief's own enumerated list; no speculative
/// metric is added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionStats {
    pub input_sstable_count: usize,
    pub output_sstable_count: usize,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub records_read: u64,
    pub records_retained: u64,
    /// Always exactly `records_read - records_retained` — asserted,
    /// not independently accumulated, so this invariant cannot drift
    /// out of sync with the other two counters.
    pub records_dropped: u64,
    /// Subset of `records_dropped` where the dropped record's `op` was
    /// a tombstone (`RecordValue::Tombstone`).
    pub tombstones_dropped: u64,
    /// Subset of `records_dropped` where the dropped record was a
    /// `RecordValue::Put` (a superseded, no-longer-needed value
    /// version). `tombstones_dropped + versions_dropped ==
    /// records_dropped` always.
    pub versions_dropped: u64,
    pub duration: Duration,
    /// `input_bytes + output_bytes` — the worst-case transient disk
    /// footprint while both the inputs and the new output coexist on
    /// disk (`ADR-COMPACTION-001` Decision 1's "~2x" figure, made
    /// concrete per cycle). Not a live OS-measured peak — a computed
    /// upper bound from the byte counts this function already has.
    pub peak_temp_disk_bytes: u64,
}

// As of Increment 2, `MergeStats`/`retain_versions`/
// `CompactionMergeIter`/`merge` all have a real, automatic production
// caller: `compact_once_impl` (`src/lsm/mod.rs`), driven by the
// background compaction worker thread.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct MergeStats {
    pub(crate) records_read: u64,
    pub(crate) records_retained: u64,
    pub(crate) tombstones_dropped: u64,
    pub(crate) versions_dropped: u64,
}

/// `ADR-COMPACTION-001` Decision 4 — the version/tombstone retention
/// rule, using only `oldest_live_snapshot_seq` (no new Snapshot API,
/// per Decision 5). `versions` must already be sorted ascending by
/// `seq`. Returns the surviving subset, still ascending by `seq`.
///
/// **Correctness derivation** (more precise than the simplified,
/// exact-snapshot-boundary worked examples in `PHASE_COMPACTION_
/// ARCHITECTURE_REPORT.md` §11 — those illustrated the rule's shape at
/// points where a live snapshot's `as_of_seq` exactly equals a
/// version's own `seq`; this implementation must also handle a live
/// snapshot pinned *strictly between* two versions, which the
/// architecture report's examples did not happen to exercise): walking
/// from the newest version backward (older), every version is retained
/// until, and including, the first one encountered whose `seq <=
/// oldest_live_snapshot_seq` — call this the **floor**. The floor is
/// exactly the version the *oldest* live snapshot itself resolves to
/// (the highest surviving version with `seq <= that snapshot's own
/// as_of_seq`). Every version strictly newer than the floor is
/// retained unconditionally, because this function only knows the
/// *minimum* live snapshot `seq` (`oldest_live_snapshot_seq()`'s own
/// contract), not the full set of live snapshots' individual `as_of_
/// seq`s — in the worst case, any `seq` at or above the floor could be
/// exactly some *other* live snapshot's own pinned answer. This is the
/// conservative, provably-correct choice the phase brief's own
/// "correctness before reduction ratio" instruction requires: it can
/// retain more than the theoretical minimum (if the live snapshot set
/// is sparse), but it never drops a version any live snapshot could
/// still observe. Everything strictly older than the floor is
/// unreachable by any live snapshot (every live snapshot's `as_of_seq`
/// is `>= oldest_live_snapshot_seq >=` the floor's own `seq`, so every
/// live snapshot resolves to the floor or something newer, never to
/// anything older than it) and is safe to drop. If `oldest_live_
/// snapshot_seq` is `None` (no live snapshot at all), only the newest
/// version survives — there is nothing to protect.
pub(crate) fn retain_versions(
    versions: Vec<(u64, RecordValue)>,
    oldest_live_snapshot_seq: Option<u64>,
    stats: &mut MergeStats,
) -> Vec<(u64, RecordValue)> {
    if versions.is_empty() {
        return versions;
    }
    let total = versions.len();
    let mut keep = vec![false; total];
    let mut floor_reached = false;
    for i in (0..total).rev() {
        let seq = versions[i].0;
        let is_newest = i == total - 1;
        if is_newest {
            keep[i] = true;
            if oldest_live_snapshot_seq.is_none_or(|oldest| seq <= oldest) {
                floor_reached = true;
            }
            continue;
        }
        if floor_reached {
            continue; // keep[i] stays false
        }
        keep[i] = true;
        if let Some(oldest) = oldest_live_snapshot_seq {
            if seq <= oldest {
                floor_reached = true;
            }
        }
    }

    let mut retained = Vec::with_capacity(total);
    for (i, entry) in versions.into_iter().enumerate() {
        if keep[i] {
            stats.records_retained += 1;
            retained.push(entry);
        } else {
            match entry.1 {
                RecordValue::Tombstone => stats.tombstones_dropped += 1,
                RecordValue::Put(_) => stats.versions_dropped += 1,
            }
        }
    }
    retained
}

/// The k-way merge over compaction's inputs — SSTable sources only
/// (compaction never reads the active/immutable MemTable layers, per
/// LSM Engine Spec §5: its data source is already-flushed SSTables).
/// One persistent [`SsTableRangeCursor`] per input (`ADR-RE-002`
/// Option A's own owned-`Arc` cursor design, reused directly — the
/// exact same type the certified Read Engine's `RangeScanIter` uses),
/// never reconstructed per key. Streams: at most one key's worth of
/// pending output records are buffered at a time (`pending`), never
/// the whole merge result — `ADR-COMPACTION-001` Decision 3/Decision
/// 12's bounded-memory requirement.
pub(crate) struct CompactionMergeIter {
    cursors: Vec<Option<std::iter::Peekable<SsTableRangeCursor>>>,
    pending: VecDeque<(Vec<u8>, u64, RecordValue)>,
    oldest_live_snapshot_seq: Option<u64>,
    errored: bool,
    /// Shared with the caller via [`merge`]'s return value — the
    /// caller cannot read this struct's own private fields after
    /// `write_from_sorted_records` (or any other consumer) has fully
    /// drained and dropped the iterator, so the running tally lives
    /// behind a shared handle instead, updated as this iterator is
    /// driven and read by the caller only after iteration completes.
    stats: Arc<Mutex<MergeStats>>,
}

impl CompactionMergeIter {
    fn new(
        inputs: &[Arc<SsTable>],
        oldest_live_snapshot_seq: Option<u64>,
        stats: Arc<Mutex<MergeStats>>,
    ) -> Self {
        let cursors = inputs
            .iter()
            .map(|table| {
                Some(
                    SsTable::range_scan_cursor(
                        Arc::clone(table),
                        Bound::Unbounded,
                        Bound::Unbounded,
                    )
                    .peekable(),
                )
            })
            .collect();
        CompactionMergeIter {
            cursors,
            pending: VecDeque::new(),
            oldest_live_snapshot_seq,
            errored: false,
            stats,
        }
    }
}

impl Iterator for CompactionMergeIter {
    type Item = Result<(Vec<u8>, u64, RecordValue)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored {
            return None;
        }
        loop {
            if let Some(rec) = self.pending.pop_front() {
                return Some(Ok(rec));
            }

            // Fail closed immediately on any corrupted/unreadable
            // input block, regardless of key ordering — never
            // continue past it, matching `ADR-RE-001` §7's established
            // range-scan contract, extended here to compaction inputs.
            for slot in self.cursors.iter_mut().flatten() {
                if matches!(slot.peek(), Some(Err(_))) {
                    self.errored = true;
                    let e = slot.next().expect("peek just confirmed Some").unwrap_err();
                    return Some(Err(e));
                }
            }

            let mut min_key: Option<Vec<u8>> = None;
            for slot in self.cursors.iter_mut().flatten() {
                if let Some(Ok((k, _, _))) = slot.peek() {
                    if min_key.as_ref().is_none_or(|mk| k < mk) {
                        min_key = Some(k.clone());
                    }
                }
            }
            let Some(winning_key) = min_key else {
                return None; // every source exhausted
            };

            let mut versions: Vec<(u64, RecordValue)> = Vec::new();
            let mut records_read_this_key = 0u64;
            for slot in self.cursors.iter_mut() {
                let Some(cursor) = slot.as_mut() else {
                    continue;
                };
                loop {
                    match cursor.peek() {
                        Some(Ok((k, _, _))) if *k == winning_key => {}
                        _ => break,
                    }
                    let (_, seq, value) = match cursor.next() {
                        Some(Ok(v)) => v,
                        _ => unreachable!("peek just confirmed a matching Ok entry"),
                    };
                    records_read_this_key += 1;
                    versions.push((seq, value));
                }
                if cursor.peek().is_none() {
                    // Exhausted — drop now, releasing this source's
                    // `Arc<SsTable>` clone and decoded-block buffer
                    // immediately rather than waiting for the whole
                    // merge to finish (mirrors `RangeScanIter::peek_
                    // sstable`'s identical early-drop discipline,
                    // Increment 6).
                    *slot = None;
                }
            }

            versions.sort_unstable_by_key(|(seq, _)| *seq);
            let mut guard = self.stats.lock().unwrap_or_else(|p| p.into_inner());
            guard.records_read += records_read_this_key;
            let retained = retain_versions(versions, self.oldest_live_snapshot_seq, &mut guard);
            drop(guard);
            for (seq, value) in retained {
                self.pending.push_back((winning_key.clone(), seq, value));
            }
            // Loop back: either `pending` now has records to yield, or
            // (a defensive case that should not occur, since the
            // newest version of every key is always retained) it is
            // still empty and we continue to the next key.
        }
    }
}

/// The deterministic core Compaction operation (`ADR-COMPACTION-001`
/// Decision 1/Decision 13): merges `inputs` (every currently-live
/// SSTable, captured once by the caller — this function does not
/// capture or re-read the live list itself) into exactly one output
/// record stream. Does **not** write the output SSTable, touch the
/// Manifest, or touch any live-list/engine state — see `LsmEngine::
/// compact_once` (`src/lsm/mod.rs`) for the full integration sequence
/// this is one step of. Kept separate and engine-agnostic so it can be
/// tested (and reasoned about) without a real `LsmEngine`.
pub(crate) fn merge(
    inputs: &[Arc<SsTable>],
    oldest_live_snapshot_seq: Option<u64>,
) -> (CompactionMergeIter, Arc<Mutex<MergeStats>>) {
    // `CompactionMergeIter::new` clones each input's `Arc<SsTable>` out
    // of `inputs` (never borrows `inputs` itself), so the returned
    // iterator is fully owned -- no lifetime tie to this call's `&[Arc
    // <SsTable>]` argument, and a concrete return type (not `impl
    // Iterator`) avoids an unnecessarily complex signature.
    let stats = Arc::new(Mutex::new(MergeStats::default()));
    let iter = CompactionMergeIter::new(inputs, oldest_live_snapshot_seq, Arc::clone(&stats));
    (iter, stats)
}

#[cfg(test)]
mod tests;
