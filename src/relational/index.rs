//! `IndexBuilder` — online secondary-index creation, maintenance-set
//! transitions, bounded resumable drop, crash recovery, and index-then-
//! fetch lookup/range scan. `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` is
//! the full decision record this module implements; see its §4–§9 for
//! the online-build protocol's timeline (T0–T5) and correctness proofs
//! this file's doc comments reference by name.
//!
//! Physical entry maintenance for ordinary DML (`INSERT`/`DELETE`) lives
//! in `TableStore` (`put_row`/`put_rows`/`delete_row`), not here — this
//! module owns *DDL* (`CREATE`/`DROP INDEX`), recovery, and read access
//! paths, all built on the same `TableStore::epoch_lock` primitive
//! `TableStore`'s own write path already uses.

use std::ops::Bound;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::catalog::schema::{IndexKind, IndexRow, IndexState};
use crate::catalog::CatalogService;
use crate::lsm::{LsmEngine, WriteOp};
use crate::relational::error::{RelationalError, Result};
use crate::relational::index_key::{
    decode_indexed_columns, encode_indexed_columns, index_entry_prefix_range, index_entry_range,
    index_scan_range,
};
use crate::relational::key::{decode_composite_key, table_row_key, table_row_range};
use crate::relational::table_store::{
    decode_full_row, indexed_entry_key, relational_type_from_column, Row, TableStore,
};
use crate::relational::value::{RelationalType, RelationalValue};

/// `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §14 (resource limits, item
/// 22/23 of the governing directive): backfill/sweep never materialize
/// the whole table/index in memory — bounded, streaming chunks.
pub const BACKFILL_CHUNK_ROWS: usize = 500;

/// Outcome of a (cancellable) startup recovery pass -- ADR-LIFECYCLE-001.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverySummary {
    /// Indexes that reached their final state (`Ready` for builds, removed for
    /// drops) during this pass.
    pub recovered: Vec<u32>,
    /// `true` when the pass stopped early because cancellation was requested:
    /// the remaining indexes are untouched (still `Building` / `Dropping`) and
    /// the next start restarts them from scratch.
    pub cancelled: bool,
}

#[inline]
fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|c| c.load(Ordering::SeqCst))
}
pub const SWEEP_CHUNK_ROWS: usize = 1000;
/// Item 22/36: bounds the number of `CREATE INDEX` builds this process
/// runs at once (a genuinely expensive, potentially adversarial
/// operation — item 36) — a bounded resource, not an unbounded
/// background queue.
pub const MAX_CONCURRENT_INDEX_BUILDS: usize = 4;

/// Bounded-cardinality counters only (item 37: never a per-table/per-
/// index/raw-key label) — a point-in-time copy via `IndexBuilder::stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStatsSnapshot {
    pub index_builds: u64,
    pub index_build_failures: u64,
    pub index_build_duration_ms_total: u64,
    pub index_entries_written: u64,
    pub index_entries_deleted: u64,
    pub index_lookups: u64,
    pub index_range_scans: u64,
    pub index_entries_examined: u64,
    /// Increment 16: rows actually returned by the index-then-fetch step
    /// (entries examined minus entries whose row was not visible).
    pub index_rows_fetched: u64,
    /// Increment 16: cumulative wall-clock microseconds spent inside
    /// index scans (a counter, no labels -- never SQL text/values).
    pub index_scan_micros_total: u64,
    pub index_errors: u64,
    pub index_drops: u64,
}

#[derive(Default)]
struct IndexStats {
    index_builds: AtomicU64,
    index_build_failures: AtomicU64,
    index_build_duration_ms_total: AtomicU64,
    index_entries_written: AtomicU64,
    index_entries_deleted: AtomicU64,
    index_lookups: AtomicU64,
    index_range_scans: AtomicU64,
    index_entries_examined: AtomicU64,
    index_rows_fetched: AtomicU64,
    index_scan_micros_total: AtomicU64,
    index_errors: AtomicU64,
    index_drops: AtomicU64,
}

impl IndexStats {
    fn snapshot(&self) -> IndexStatsSnapshot {
        IndexStatsSnapshot {
            index_builds: self.index_builds.load(Ordering::Relaxed),
            index_build_failures: self.index_build_failures.load(Ordering::Relaxed),
            index_build_duration_ms_total: self
                .index_build_duration_ms_total
                .load(Ordering::Relaxed),
            index_entries_written: self.index_entries_written.load(Ordering::Relaxed),
            index_entries_deleted: self.index_entries_deleted.load(Ordering::Relaxed),
            index_lookups: self.index_lookups.load(Ordering::Relaxed),
            index_range_scans: self.index_range_scans.load(Ordering::Relaxed),
            index_entries_examined: self.index_entries_examined.load(Ordering::Relaxed),
            index_rows_fetched: self.index_rows_fetched.load(Ordering::Relaxed),
            index_scan_micros_total: self.index_scan_micros_total.load(Ordering::Relaxed),
            index_errors: self.index_errors.load(Ordering::Relaxed),
            index_drops: self.index_drops.load(Ordering::Relaxed),
        }
    }
}

/// A held slot in the bounded concurrent-build limiter
/// (`MAX_CONCURRENT_INDEX_BUILDS`) — released automatically on `Drop`, so
/// a build that returns early via `?` never leaks its slot.
struct BuildSlot<'a>(&'a AtomicUsize);

impl Drop for BuildSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn acquire_build_slot(counter: &AtomicUsize) -> Result<BuildSlot<'_>> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        if current >= MAX_CONCURRENT_INDEX_BUILDS {
            return Err(RelationalError::InvalidInput {
                detail: format!(
                    "too many concurrent index builds in progress (max {MAX_CONCURRENT_INDEX_BUILDS})"
                ),
            });
        }
        match counter.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(BuildSlot(counter)),
            Err(observed) => current = observed,
        }
    }
}

pub struct IndexBuilder {
    engine: Arc<LsmEngine>,
    catalog: Arc<CatalogService>,
    table_store: Arc<TableStore>,
    stats: IndexStats,
    active_builds: AtomicUsize,
    /// Test-only seam (compiled out of every non-test build): called after each
    /// backfill / sweep chunk is written, so cancellation tests can flip the flag
    /// at an exact chunk boundary instead of racing a sleep.
    #[cfg(test)]
    pub(crate) test_after_chunk: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl IndexBuilder {
    pub fn new(
        engine: Arc<LsmEngine>,
        catalog: Arc<CatalogService>,
        table_store: Arc<TableStore>,
    ) -> Self {
        IndexBuilder {
            engine,
            catalog,
            table_store,
            stats: IndexStats::default(),
            active_builds: AtomicUsize::new(0),
            #[cfg(test)]
            test_after_chunk: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn after_chunk(&self) {
        let hook = self
            .test_after_chunk
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(h) = hook {
            h();
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn after_chunk(&self) {}

    pub fn stats(&self) -> IndexStatsSnapshot {
        self.stats.snapshot()
    }

    // -----------------------------------------------------------------
    // Online CREATE INDEX — `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §4–7.
    // -----------------------------------------------------------------

    /// `CREATE INDEX` with online backfill — the table remains fully
    /// writable throughout. T0 (catalog row inserted `Building`) is
    /// committed under the table's epoch *write* lock, held only for
    /// that one `write_batch` call; T1 (the backfill snapshot boundary)
    /// is captured immediately afterward; backfill (T2–T4) runs
    /// concurrently with ordinary writes; T5 (the atomic `Building` ->
    /// `Ready` transition) commits once backfill completes. On any
    /// failure the index is marked `Failed` (never left `Building`
    /// forever, never silently promoted) and the original error is
    /// returned.
    pub fn create_index_online(
        &self,
        table_id: u32,
        name: &str,
        kind: IndexKind,
        column_ordinals: &[u16],
    ) -> Result<u32> {
        let _slot = acquire_build_slot(&self.active_builds)?;
        let started = std::time::Instant::now();
        self.stats.index_builds.fetch_add(1, Ordering::Relaxed);

        let epoch = self.table_store.epoch_lock(table_id);
        let index_id = {
            // T0: the catalog `Building`-row insert is the one moment
            // that changes the maintained-index set, so it alone needs
            // the epoch lock's *write* side (see `TableStore::epoch_
            // lock`'s doc comment for the full proof) — held only for
            // this one call, not for backfill.
            let _write_guard = epoch.write().unwrap_or_else(|p| p.into_inner());
            self.catalog
                .create_index(table_id, name, kind, column_ordinals)?
        };

        // A client-driven `CREATE INDEX` is never cancelled (`None`).
        match self.backfill_and_activate(table_id, index_id, None) {
            Ok(_) => {
                self.stats
                    .index_build_duration_ms_total
                    .fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                Ok(index_id)
            }
            Err(e) => {
                self.stats
                    .index_build_failures
                    .fetch_add(1, Ordering::Relaxed);
                self.stats.index_errors.fetch_add(1, Ordering::Relaxed);
                if let Err(mark_err) = self.catalog.mark_index_failed(index_id) {
                    eprintln!(
                        "rubixdb: index {index_id} build failed AND could not be marked Failed \
                         ({mark_err}); it remains Building until the next process restart's \
                         recovery pass retries it"
                    );
                }
                Err(e)
            }
        }
    }

    /// Runs backfill for an already-`Building` index and, on success,
    /// atomically promotes it to `Ready` (T5). Shared by `create_index_
    /// online` and `recover_incomplete_builds` — restart-recovery re-runs
    /// exactly this same path against a fresh snapshot (the ADR's chosen
    /// "restart," not "resume," recovery policy — see §8).
    ///
    /// `cancel` (recovery only): checked once per chunk by `backfill`. Returns
    /// `Ok(true)` when the index was promoted to `Ready`, `Ok(false)` when the
    /// pass was cancelled -- in that case NOTHING is changed in the catalog: the
    /// index stays `Building`, exactly the state a process kill leaves, and the
    /// next start restarts it from scratch.
    fn backfill_and_activate(
        &self,
        table_id: u32,
        index_id: u32,
        cancel: Option<&AtomicBool>,
    ) -> Result<bool> {
        let index_row =
            self.catalog
                .get_index(index_id)?
                .ok_or_else(|| RelationalError::NotFound {
                    object: format!("index {index_id}"),
                })?;
        if index_row.state != IndexState::Building {
            return Err(RelationalError::InvalidInput {
                detail: format!(
                    "index {index_id} is in state {:?}, expected Building to backfill",
                    index_row.state
                ),
            });
        }
        if self.backfill(table_id, &index_row, cancel)?.is_none() {
            return Ok(false);
        }
        self.catalog.mark_index_ready(index_id)?;
        Ok(true)
    }

    /// The backfill pass itself (T1–T4). Enumerates every primary key
    /// visible at a single consistent snapshot (T1, captured once, up
    /// front) in bounded chunks (never materializing the whole table),
    /// then — for each chunk — acquires the table epoch lock's *write*
    /// side and re-validates each candidate row's **current** state
    /// before writing its entry.
    ///
    /// This re-validation-under-exclusion is the specific mechanism that
    /// closes the "phantom entry" race `PHASE_RELATIONAL_INDEX_BACKFILL_
    /// ADR.md` §7 proves: without it, a row deleted between T1 and this
    /// chunk's flush could have its now-stale T1 value written *after*
    /// the concurrent `delete_row` call's own index-entry tombstone,
    /// resurrecting an entry for a row that no longer exists. Under the
    /// epoch write lock, no `put_row`/`delete_row` call (which holds the
    /// *read* side for its own critical section) can be mid-flight, so
    /// the re-validating `get` here is guaranteed to observe either (a)
    /// every maintenance write that completed before this chunk started,
    /// reflected in what it reads, or (b) nothing from a maintenance
    /// write that starts after — never a value that a concurrent
    /// maintenance write is simultaneously changing. Because this
    /// section commits the entry (or skips it, if the row is now gone)
    /// based on that just-observed current truth, its own write can
    /// never be "stale" relative to what any writer could have already
    /// established.
    ///
    /// Cancellation (`cancel`, recovery only): the flag is read once at the top
    /// of every chunk, **outside** the epoch write lock and before any write of
    /// that chunk, so a cancelled pass never stops in the middle of a chunk's
    /// critical section. Returns `Ok(None)` when cancelled (entries already
    /// written are harmless: a restarted backfill overwrites them with
    /// identical values, and a `Building` index is invisible to the planner).
    fn backfill(
        &self,
        table_id: u32,
        index_row: &IndexRow,
        cancel: Option<&AtomicBool>,
    ) -> Result<Option<u64>> {
        let table = self
            .catalog
            .get_table(table_id)?
            .ok_or_else(|| RelationalError::NotFound {
                object: format!("table {table_id}"),
            })?;
        let columns = self.catalog.get_columns(table_id)?;
        let pk_types: Vec<RelationalType> = table
            .pk_ordinals
            .iter()
            .map(|&ord| {
                columns
                    .get(ord as usize)
                    .ok_or_else(|| RelationalError::InvalidInput {
                        detail: format!("primary-key ordinal {ord} has no matching column"),
                    })
                    .and_then(relational_type_from_column)
            })
            .collect::<Result<_>>()?;

        // T1: one consistent read-view boundary for the whole backfill
        // enumeration. Captured once, before any chunk is read. `_snapshot`
        // is deliberately kept alive for this entire function (registered
        // in `SnapshotRegistry`, consulted by `compaction::merge` via
        // `oldest_live_snapshot_seq()`) — dropping it early would let
        // Compaction reclaim a row version this backfill's later chunks
        // still need to read at `snap_seq`, corrupting an in-progress
        // build. Do not shorten its scope.
        let _snapshot = self.engine.snapshot();
        let snap_seq = _snapshot.seq();

        let (start, end) = table_row_range(table_id);
        let epoch = self.table_store.epoch_lock(table_id);
        let mut total_written: u64 = 0;
        let mut rows_enumerated: u64 = 0;

        let mut iter = self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), snap_seq);
        loop {
            if is_cancelled(cancel) {
                return Ok(None);
            }
            let mut candidates: Vec<(Vec<RelationalValue>, Vec<u8>)> =
                Vec::with_capacity(BACKFILL_CHUNK_ROWS);
            for row in iter.by_ref().take(BACKFILL_CHUNK_ROWS) {
                let (key, _value) = row?;
                let pk_bytes = key.get(9..).ok_or_else(|| RelationalError::InvalidInput {
                    detail: "table row key shorter than the fixed 9-byte header".to_string(),
                })?;
                let pk_values = decode_composite_key(&pk_types, pk_bytes)?;
                candidates.push((pk_values, pk_bytes.to_vec()));
            }
            if candidates.is_empty() {
                break;
            }
            rows_enumerated += candidates.len() as u64;

            let mut ops = Vec::with_capacity(candidates.len());
            {
                let _write_guard = epoch.write().unwrap_or_else(|p| p.into_inner());
                for (pk_values, encoded_pk) in &candidates {
                    let row_key = table_row_key(table_id, encoded_pk);
                    if let Some(value_bytes) = self.engine.get(&row_key)? {
                        let full_row: Row =
                            decode_full_row(&table, &columns, pk_values, &value_bytes)?;
                        let entry_key =
                            indexed_entry_key(table_id, index_row, &full_row, encoded_pk)?;
                        ops.push(WriteOp::Put {
                            key: entry_key,
                            value: Vec::new(),
                        });
                    }
                    // Row no longer exists (deleted at or after T1, before
                    // this chunk's re-validation ran): correctly skipped —
                    // no entry from a row that isn't there, and no stale
                    // resurrection of one `delete_row`'s own maintenance
                    // already removed.
                }
                if !ops.is_empty() {
                    self.engine.write_batch(&ops)?;
                }
            }
            total_written += ops.len() as u64;
            self.stats
                .index_entries_written
                .fetch_add(ops.len() as u64, Ordering::Relaxed);
            self.after_chunk();
        }
        // Increment 17: the enumeration visited every row of the table at
        // the backfill snapshot -- a free, exact row count for the cost
        // model's statistics.
        self.table_store
            .runtime_stats()
            .observe_row_count(table_id, rows_enumerated);
        Ok(Some(total_written))
    }

    // -----------------------------------------------------------------
    // Crash recovery — `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §8.
    // -----------------------------------------------------------------

    /// Every index found `Building` at process startup is restarted from
    /// scratch (a fresh T1 snapshot, a fresh full backfill pass) — never
    /// silently promoted to `Ready`, never resumed from an unproven
    /// partial cursor. Idempotent: a restarted backfill re-derives every
    /// entry from current truth, so entries a prior, interrupted attempt
    /// already wrote are simply overwritten with identical values (same
    /// key, same empty marker value). Returns the `index_id`s
    /// successfully recovered to `Ready`; an index that fails recovery
    /// is marked `Failed` (not retried again automatically) and omitted.
    pub fn recover_incomplete_builds(&self) -> Result<Vec<u32>> {
        Ok(self
            .recover_incomplete_builds_cancellable(&AtomicBool::new(false))?
            .recovered)
    }

    /// Same as [`Self::recover_incomplete_builds`], but stops promptly when
    /// `cancel` becomes true (ADR-LIFECYCLE-001): between indexes and once per
    /// backfill chunk. A cancelled index is left `Building` -- not `Failed`, not
    /// promoted -- so the next start restarts it from scratch, the state a kill
    /// leaves. `RecoverySummary::cancelled` tells the caller recovery is
    /// incomplete; `recovered` lists the indexes that did reach `Ready`.
    pub fn recover_incomplete_builds_cancellable(
        &self,
        cancel: &AtomicBool,
    ) -> Result<RecoverySummary> {
        let building = self.catalog.list_indexes_in_state(IndexState::Building)?;
        let mut recovered = Vec::new();
        for index_row in building {
            if is_cancelled(Some(cancel)) {
                return Ok(RecoverySummary {
                    recovered,
                    cancelled: true,
                });
            }
            let _slot = match acquire_build_slot(&self.active_builds) {
                Ok(slot) => slot,
                Err(e) => {
                    self.stats.index_errors.fetch_add(1, Ordering::Relaxed);
                    return Err(e);
                }
            };
            match self.backfill_and_activate(index_row.table_id, index_row.index_id, Some(cancel)) {
                Ok(true) => recovered.push(index_row.index_id),
                Ok(false) => {
                    return Ok(RecoverySummary {
                        recovered,
                        cancelled: true,
                    })
                }
                Err(e) => {
                    self.stats
                        .index_build_failures
                        .fetch_add(1, Ordering::Relaxed);
                    if let Err(mark_err) = self.catalog.mark_index_failed(index_row.index_id) {
                        eprintln!(
                            "rubixdb: recovery of index {} failed AND could not be marked \
                             Failed ({mark_err}); original error: {e}",
                            index_row.index_id
                        );
                    }
                }
            }
        }
        Ok(RecoverySummary {
            recovered,
            cancelled: false,
        })
    }

    /// Every index found `Dropping` at startup has its physical sweep
    /// restarted from the beginning of its key range (idempotent:
    /// deleting an already-tombstoned entry is a no-op) and, once
    /// complete, its catalog row removed. Returns the `index_id`s fully
    /// removed.
    pub fn recover_incomplete_drops(&self) -> Result<Vec<u32>> {
        Ok(self
            .recover_incomplete_drops_cancellable(&AtomicBool::new(false))?
            .recovered)
    }

    /// Same as [`Self::recover_incomplete_drops`], but stops promptly when
    /// `cancel` becomes true (between indexes and once per sweep chunk). A
    /// cancelled index stays `Dropping` (its catalog row is not removed) and the
    /// next start restarts the sweep.
    pub fn recover_incomplete_drops_cancellable(
        &self,
        cancel: &AtomicBool,
    ) -> Result<RecoverySummary> {
        let dropping = self.catalog.list_indexes_in_state(IndexState::Dropping)?;
        let mut recovered = Vec::new();
        for index_row in dropping {
            if is_cancelled(Some(cancel))
                || !self.sweep_index_entries(
                    index_row.table_id,
                    index_row.index_id,
                    Some(cancel),
                )?
            {
                return Ok(RecoverySummary {
                    recovered,
                    cancelled: true,
                });
            }
            self.catalog.remove_index_row(index_row.index_id)?;
            self.stats.index_drops.fetch_add(1, Ordering::Relaxed);
            recovered.push(index_row.index_id);
        }
        Ok(RecoverySummary {
            recovered,
            cancelled: false,
        })
    }

    // -----------------------------------------------------------------
    // DROP INDEX — bounded, resumable physical sweep (D13's `DROPPING`-
    // table precedent, mirrored for indexes; `PHASE_RELATIONAL_INDEX_
    // BACKFILL_ADR.md` §11).
    // -----------------------------------------------------------------

    /// `DROP INDEX`, online. The catalog transition to `Dropping` commits
    /// under the table epoch's write lock (so no writer's critical
    /// section can straddle it and keep maintaining an index that's
    /// disappearing); after that, no further entries are ever added
    /// (every writer's own `TableStore::maintained_indexes` call will see
    /// `Dropping`, not `Building`/`Ready`), so the sweep needs no further
    /// synchronization — it is a plain, bounded, chunked, resumable-by-
    /// restart delete of everything already present.
    pub fn drop_index_online(&self, index_id: u32) -> Result<()> {
        let index_row =
            self.catalog
                .get_index(index_id)?
                .ok_or_else(|| RelationalError::NotFound {
                    object: format!("index {index_id}"),
                })?;
        let table_id = index_row.table_id;
        {
            let epoch = self.table_store.epoch_lock(table_id);
            let _write_guard = epoch.write().unwrap_or_else(|p| p.into_inner());
            self.catalog.mark_index_dropping(index_id)?;
        }
        // A client-driven `DROP INDEX` is never cancelled (`None`).
        self.sweep_index_entries(table_id, index_id, None)?;
        self.catalog.remove_index_row(index_id)?;
        self.stats.index_drops.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Deletes every physical entry under `(table_id, index_id)` in
    /// bounded chunks, resuming from just past the last key deleted each
    /// iteration (never re-scanning an already-tombstoned prefix —
    /// `O(entry_count)` total work, not `O(entry_count^2 /
    /// chunk_size)`). No epoch lock needed here: by the time this runs,
    /// `mark_index_dropping` has already committed (under the epoch write
    /// lock, by every caller of this method), so no writer will ever
    /// again add a new entry for this index.
    ///
    /// `cancel` (recovery only) is read once per chunk. Returns `Ok(true)` when
    /// the whole range was swept, `Ok(false)` when cancelled (nothing else is
    /// changed; deleting is idempotent, so a restarted sweep is safe).
    fn sweep_index_entries(
        &self,
        table_id: u32,
        index_id: u32,
        cancel: Option<&AtomicBool>,
    ) -> Result<bool> {
        let (range_start, range_end) = index_entry_range(table_id, index_id);
        let mut cursor = range_start;
        loop {
            if is_cancelled(cancel) {
                return Ok(false);
            }
            let mut batch = Vec::with_capacity(SWEEP_CHUNK_ROWS);
            let mut last_key: Option<Vec<u8>> = None;
            for row in self
                .engine
                .range_scan(as_bound_ref(&cursor), as_bound_ref(&range_end), u64::MAX)
                .take(SWEEP_CHUNK_ROWS)
            {
                let (key, _value) = row?;
                last_key = Some(key.clone());
                batch.push(WriteOp::Delete { key });
            }
            if batch.is_empty() {
                break;
            }
            self.engine.write_batch(&batch)?;
            self.stats
                .index_entries_deleted
                .fetch_add(batch.len() as u64, Ordering::Relaxed);
            self.after_chunk();
            cursor = Bound::Excluded(last_key.expect("batch non-empty"));
        }
        Ok(true)
    }

    // -----------------------------------------------------------------
    // Read access — index-then-fetch (item 16/17/18). Query-usable only
    // for a `Ready` index (item 8: a `Building` index must never be used
    // by normal reads).
    // -----------------------------------------------------------------

    /// Increment 17 (F-2): is `index_id` a faithful representation of the
    /// table as visible at `as_of_seq`? Returns the index's catalog row
    /// *as that snapshot saw it* iff its state there was `Ready`, else
    /// `None`.
    ///
    /// Why this is exactly "`snapshot_seq >= index_ready_seq`": backfill
    /// writes the index entries at engine sequence numbers strictly below
    /// the `Building -> Ready` promotion's own catalog write, and from the
    /// promotion onward every writer maintains the index in the same
    /// atomic batch as the row. A reader whose snapshot is at or after the
    /// promotion therefore sees a complete, correct index; a reader whose
    /// snapshot is before it sees the index absent, `Building`, or only
    /// partially populated (and, if an indexed value changed in between,
    /// entries that disagree with the row version it can see) -- it must
    /// not use it. The promotion's version in the (MVCC) catalog row is
    /// the persisted, crash-safe ready sequence; no new metadata exists to
    /// lose or migrate. An index that was `Dropping` at the snapshot is
    /// likewise unusable (writers had stopped maintaining it), while one
    /// dropped *after* the snapshot is still valid for it (its entry
    /// tombstones carry later sequences).
    pub fn index_row_usable_at(&self, index_id: u32, as_of_seq: u64) -> Result<Option<IndexRow>> {
        match self.catalog.get_index_as_of(index_id, as_of_seq)? {
            Some(row) if row.state == IndexState::Ready => Ok(Some(row)),
            _ => Ok(None),
        }
    }

    fn indexed_types(
        index_row: &IndexRow,
        columns: &[crate::catalog::schema::ColumnRow],
    ) -> Result<Vec<RelationalType>> {
        index_row
            .column_ordinals
            .iter()
            .map(|&ord| {
                columns
                    .get(ord as usize)
                    .ok_or_else(|| RelationalError::InvalidInput {
                        detail: format!("index column ordinal {ord} has no matching column"),
                    })
                    .and_then(relational_type_from_column)
            })
            .collect()
    }

    /// Equality/prefix lookup: `prefix_values.len()` must be between 1
    /// and the index's own column count (a true prefix, item 20's
    /// "prefix equality"). Real `O(log n)`-class index lookup (bloom/
    /// block-index-assisted, per the certified Read Engine — never a
    /// full-table scan), then one `TableStore::get_row` per matching
    /// entry (index-then-fetch, D7/item 18).
    pub fn index_lookup(
        &self,
        index_id: u32,
        prefix_values: &[Option<RelationalValue>],
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        self.index_lookup_as_of(index_id, prefix_values, u64::MAX)
    }

    /// `PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md` §2: the
    /// snapshotted counterpart `index_lookup` lacked. Closes a
    /// previously-documented, accepted gap from Increment 5
    /// (`scan_entries`'s own former doc comment called the un-
    /// snapshotted index-then-fetch race "expected... under no active
    /// transaction/snapshot isolation for reads — D10's future
    /// transaction layer is what removes this"): D10 now exists
    /// (Increment 7), and the executor (Increment 9) is its first real
    /// caller for scan-shaped reads.
    pub fn index_lookup_as_of(
        &self,
        index_id: u32,
        prefix_values: &[Option<RelationalValue>],
        as_of_seq: u64,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        self.index_lookup_as_of_bounded(index_id, prefix_values, as_of_seq, usize::MAX)
    }

    /// `index_lookup_as_of` with the materialization bound enforced
    /// *while collecting* (Increment 16): fails closed with
    /// `RelationalError::ResourceLimit` the moment more than `max_rows`
    /// rows have been fetched, instead of building an unbounded `Vec`
    /// first and checking afterward.
    pub fn index_lookup_as_of_bounded(
        &self,
        index_id: u32,
        prefix_values: &[Option<RelationalValue>],
        as_of_seq: u64,
        max_rows: usize,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        let spec = IndexScanSpec::Equality(prefix_values.to_vec());
        self.scan_via_probe(index_id, &spec, as_of_seq, max_rows)
    }

    /// A real ordered range scan over one index's entries — item 17's
    /// inclusive/exclusive/composite bounds, expressed directly as
    /// `Bound<Vec<Option<RelationalValue>>>` over the index's declared
    /// leading columns (a caller may bound on a strict prefix of the
    /// index's columns; trailing columns are unconstrained within that
    /// bound). Uses the certified `range_scan` infrastructure exactly as
    /// `TableStore::scan_table` does — never re-implemented.
    pub fn index_range_scan(
        &self,
        index_id: u32,
        start: Bound<Vec<Option<RelationalValue>>>,
        end: Bound<Vec<Option<RelationalValue>>>,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        self.index_range_scan_as_of(index_id, start, end, u64::MAX)
    }

    /// `PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md` §2 — see
    /// `index_lookup_as_of`'s own doc comment.
    pub fn index_range_scan_as_of(
        &self,
        index_id: u32,
        start: Bound<Vec<Option<RelationalValue>>>,
        end: Bound<Vec<Option<RelationalValue>>>,
        as_of_seq: u64,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        self.index_range_scan_as_of_bounded(index_id, start, end, as_of_seq, usize::MAX)
    }

    /// Bounded counterpart of `index_range_scan_as_of` -- see
    /// `index_lookup_as_of_bounded`.
    pub fn index_range_scan_as_of_bounded(
        &self,
        index_id: u32,
        start: Bound<Vec<Option<RelationalValue>>>,
        end: Bound<Vec<Option<RelationalValue>>>,
        as_of_seq: u64,
        max_rows: usize,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        let spec = IndexScanSpec::Range { start, end };
        self.scan_via_probe(index_id, &spec, as_of_seq, max_rows)
    }

    /// The eager one-call scan every pre-Increment-17 caller uses: probe
    /// every entry, then fetch every row. An index that was not `Ready` at
    /// `as_of_seq` is an error here (these callers have no fallback path);
    /// the SQL executor instead calls `probe_index_entries_as_of` directly
    /// and falls back to a table scan.
    fn scan_via_probe(
        &self,
        index_id: u32,
        spec: &IndexScanSpec,
        as_of_seq: u64,
        max_rows: usize,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        match self.probe_index_entries_as_of(index_id, spec, as_of_seq, usize::MAX, max_rows)? {
            IndexProbe::Entries(entries) => self.fetch_index_rows_as_of(entries, as_of_seq),
            IndexProbe::Unusable => Err(RelationalError::InvalidInput {
                detail: format!(
                    "index {index_id} was not Ready at snapshot {as_of_seq} and cannot be used \
                     for reads"
                ),
            }),
            IndexProbe::Truncated => unreachable!("entry_limit is usize::MAX"),
        }
    }

    /// Validates the index for the snapshot (F-2) and resolves everything an
    /// entry enumeration needs, once: the physical key range, the table's
    /// metadata, and the indexed-column / primary-key decode types.
    /// `None` = the index was not `Ready` as of the snapshot.
    fn probe_setup(
        &self,
        index_id: u32,
        spec: &IndexScanSpec,
        as_of_seq: u64,
    ) -> Result<Option<ProbeSetup>> {
        let Some(index_row) = self.index_row_usable_at(index_id, as_of_seq)? else {
            return Ok(None);
        };
        let (start, end) = match spec {
            IndexScanSpec::Equality(prefix_values) => {
                if prefix_values.is_empty() || prefix_values.len() > index_row.column_ordinals.len()
                {
                    return Err(RelationalError::InvalidInput {
                        detail: format!(
                            "index {index_id} covers {} column(s); lookup prefix must supply 1..={} value(s), got {}",
                            index_row.column_ordinals.len(),
                            index_row.column_ordinals.len(),
                            prefix_values.len()
                        ),
                    });
                }
                let prefix_bytes = encode_indexed_columns(prefix_values)?;
                index_entry_prefix_range(index_row.table_id, index_id, &prefix_bytes)
            }
            IndexScanSpec::Range { start, end } => {
                let encode_bound =
                    |b: &Bound<Vec<Option<RelationalValue>>>| -> Result<Bound<Vec<u8>>> {
                        Ok(match b {
                            Bound::Unbounded => Bound::Unbounded,
                            Bound::Included(v) => Bound::Included(encode_indexed_columns(v)?),
                            Bound::Excluded(v) => Bound::Excluded(encode_indexed_columns(v)?),
                        })
                    };
                index_scan_range(
                    index_row.table_id,
                    index_id,
                    encode_bound(start)?,
                    encode_bound(end)?,
                )
            }
        };
        match spec {
            IndexScanSpec::Equality(_) => self.stats.index_lookups.fetch_add(1, Ordering::Relaxed),
            IndexScanSpec::Range { .. } => {
                self.stats.index_range_scans.fetch_add(1, Ordering::Relaxed)
            }
        };

        // Resolved once per scan (Increment 16) and carried to phase 2.
        let (table, columns) = self.table_store.resolve_table(index_row.table_id)?;
        let indexed_types = Self::indexed_types(&index_row, &columns)?;
        let pk_types: Vec<RelationalType> = table
            .pk_ordinals
            .iter()
            .map(|&ord| {
                columns
                    .get(ord as usize)
                    .ok_or_else(|| RelationalError::InvalidInput {
                        detail: format!("primary-key ordinal {ord} has no matching column"),
                    })
                    .and_then(relational_type_from_column)
            })
            .collect::<Result<_>>()?;
        Ok(Some(ProbeSetup {
            start,
            end,
            table,
            columns,
            indexed_types,
            pk_types,
        }))
    }

    /// Increment 17, phase 1 of an index read: validate that the index is
    /// usable for the snapshot (F-2), then enumerate the matching index
    /// entries **without fetching any row**. Entry enumeration costs
    /// ~0.6us per entry (Increment 16 measurement) against ~15us per row
    /// fetched, so the caller learns the exact match count -- and can still
    /// choose a different access path -- at a small fraction of the cost of
    /// the index path itself.
    ///
    /// - `Unusable`: the index was not `Ready` as of `as_of_seq`; it does
    ///   not represent the table that snapshot sees and must not be used.
    /// - `Truncated`: more than `entry_limit` entries match (enumeration
    ///   stopped early; nothing was fetched).
    /// - `max_rows` is the hard resource bound: exceeding it fails closed
    ///   with `ResourceLimit` (checked in preference to `Truncated` when
    ///   `max_rows <= entry_limit`).
    pub fn probe_index_entries_as_of(
        &self,
        index_id: u32,
        spec: &IndexScanSpec,
        as_of_seq: u64,
        entry_limit: usize,
        max_rows: usize,
    ) -> Result<IndexProbe> {
        let started = std::time::Instant::now();
        let Some(ProbeSetup {
            start,
            end,
            table,
            columns,
            indexed_types,
            pk_types,
        }) = self.probe_setup(index_id, spec, as_of_seq)?
        else {
            return Ok(IndexProbe::Unusable);
        };

        let cap = entry_limit.min(max_rows);
        let mut entries: Vec<(Vec<RelationalValue>, Vec<u8>)> = Vec::new();
        let mut examined: u64 = 0;
        let mut outcome = None;
        for entry in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), as_of_seq)
        {
            let (key, _value) = entry?;
            examined += 1;
            if entries.len() >= cap {
                outcome = Some(if max_rows <= entry_limit {
                    Err(RelationalError::ResourceLimit {
                        detail: format!(
                            "index scan exceeded the {max_rows}-row materialization limit (max_index_scan_rows)"
                        ),
                    })
                } else {
                    Ok(IndexProbe::Truncated)
                });
                break;
            }
            let body = key.get(9..).ok_or_else(|| RelationalError::InvalidInput {
                detail: "index entry key shorter than the fixed 9-byte header".to_string(),
            })?;
            let (_indexed_values, consumed) = decode_indexed_columns(&indexed_types, body)?;
            let pk_bytes = &body[consumed..];
            let pk_values = decode_composite_key(&pk_types, pk_bytes)?;
            entries.push((pk_values, pk_bytes.to_vec()));
        }
        self.stats
            .index_entries_examined
            .fetch_add(examined, Ordering::Relaxed);
        self.stats
            .index_scan_micros_total
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        if let Some(o) = outcome {
            return o;
        }
        Ok(IndexProbe::Entries(IndexEntries {
            table,
            columns,
            entries,
        }))
    }

    /// Opens a resumable enumeration of the entries matching `spec` as of
    /// `as_of_seq`, or `None` if the index was not `Ready` at the snapshot
    /// (F-2). Nothing is enumerated until `advance_until` is called.
    pub fn probe_cursor(
        &self,
        index_id: u32,
        spec: &IndexScanSpec,
        as_of_seq: u64,
    ) -> Result<Option<IndexProbeCursor>> {
        let Some(setup) = self.probe_setup(index_id, spec, as_of_seq)? else {
            return Ok(None);
        };
        let iter = self.engine.range_scan(
            as_bound_ref(&setup.start),
            as_bound_ref(&setup.end),
            as_of_seq,
        );
        Ok(Some(IndexProbeCursor {
            iter,
            table: setup.table,
            columns: setup.columns,
            indexed_types: setup.indexed_types,
            pk_types: setup.pk_types,
            entries: Vec::new(),
            done: false,
            examined: 0,
        }))
    }

    /// Turns a fully enumerated cursor into the entries `lazy_row_fetcher`
    /// consumes, recording the work it did.
    pub fn finish_probe_cursor(&self, cursor: IndexProbeCursor) -> IndexEntries {
        self.stats
            .index_entries_examined
            .fetch_add(cursor.examined, Ordering::Relaxed);
        IndexEntries {
            table: cursor.table,
            columns: cursor.columns,
            entries: cursor.entries,
        }
    }

    /// Phase 2, lazily: an iterator that fetches each enumerated entry's
    /// row on demand (see `IndexRowFetcher`).
    pub fn lazy_row_fetcher<'a>(
        &'a self,
        entries: IndexEntries,
        as_of_seq: u64,
    ) -> IndexRowFetcher<'a> {
        let IndexEntries {
            table,
            columns,
            entries,
        } = entries;
        IndexRowFetcher {
            store: &self.table_store,
            table,
            columns,
            entries: entries.into_iter(),
            as_of_seq,
            stats: &self.stats,
        }
    }

    /// Phase 2: fetch the row for every enumerated entry at `as_of_seq`.
    /// A row whose entry exists but which is not visible at the snapshot
    /// (deleted before it) is skipped, exactly as before.
    pub fn fetch_index_rows_as_of(
        &self,
        entries: IndexEntries,
        as_of_seq: u64,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        let started = std::time::Instant::now();
        let IndexEntries {
            table,
            columns,
            entries,
        } = entries;
        let mut out = Vec::with_capacity(entries.len());
        for (pk_values, pk_bytes) in entries {
            if let Some(row) = self
                .table_store
                .fetch_row_by_encoded_pk(&table, &columns, &pk_values, &pk_bytes, as_of_seq)?
            {
                out.push((pk_values, row));
            }
        }
        self.stats
            .index_rows_fetched
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        self.stats
            .index_scan_micros_total
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        Ok(out)
    }
}

/// Increment 18: phase 2 of an index read as a *lazy* iterator. The matching
/// index entries were enumerated up front (key-only, ~0.6us each, bounded by
/// `max_index_scan_rows`); the point read and decode of each row now happens
/// only when a consumer asks for it. Holds the enumerated entries (primary
/// keys, tens of bytes each) -- never the rows -- so a `LIMIT 10` over a
/// 25,000-match index range fetches ten rows, not 25,000, and an operator
/// that stops pulling (cancellation, deadline, early exit) stops the work.
/// Same snapshot as the probe, so MVCC visibility is unchanged.
pub struct IndexRowFetcher<'a> {
    store: &'a TableStore,
    table: crate::catalog::schema::TableRow,
    columns: Vec<crate::catalog::schema::ColumnRow>,
    entries: std::vec::IntoIter<(Vec<RelationalValue>, Vec<u8>)>,
    as_of_seq: u64,
    stats: &'a IndexStats,
}

impl IndexRowFetcher<'_> {
    /// Entries not yet fetched (an upper bound on the rows still to come).
    pub fn remaining(&self) -> usize {
        self.entries.len()
    }
}

impl Iterator for IndexRowFetcher<'_> {
    type Item = Result<(Vec<RelationalValue>, Row)>;

    fn next(&mut self) -> Option<Self::Item> {
        for (pk_values, pk_bytes) in self.entries.by_ref() {
            match self.store.fetch_row_by_encoded_pk(
                &self.table,
                &self.columns,
                &pk_values,
                &pk_bytes,
                self.as_of_seq,
            ) {
                // A row whose entry exists but which is not visible at the
                // snapshot (deleted before it) is skipped, as in the eager
                // path.
                Ok(None) => continue,
                Ok(Some(row)) => {
                    self.stats
                        .index_rows_fetched
                        .fetch_add(1, Ordering::Relaxed);
                    return Some(Ok((pk_values, row)));
                }
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
}

/// Everything an entry enumeration needs, resolved once (see `probe_setup`).
struct ProbeSetup {
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
    table: crate::catalog::schema::TableRow,
    columns: Vec<crate::catalog::schema::ColumnRow>,
    indexed_types: Vec<RelationalType>,
    pk_types: Vec<RelationalType>,
}

/// Increment 18: a *resumable* index-entry enumeration. The cost-based access
/// chooser races several candidates (a PK range and secondary indexes) in
/// lockstep in cost units: each is advanced by the same slice of estimated
/// cost, the first to finish wins, and no candidate's work is ever repeated.
/// The loser has enumerated only about as much as the winner's cost, so
/// pricing a candidate costs a few percent of executing it.
pub struct IndexProbeCursor {
    iter: crate::lsm::RangeScanIter,
    table: crate::catalog::schema::TableRow,
    columns: Vec<crate::catalog::schema::ColumnRow>,
    indexed_types: Vec<RelationalType>,
    pk_types: Vec<RelationalType>,
    entries: Vec<(Vec<RelationalValue>, Vec<u8>)>,
    done: bool,
    examined: u64,
}

impl IndexProbeCursor {
    /// Enumerates until at least `total` entries are held or the range is
    /// exhausted; returns whether it is exhausted. Never errors on size --
    /// the caller bounds `total` by `max_index_scan_rows`.
    pub fn advance_until(&mut self, total: usize) -> Result<bool> {
        while !self.done && self.entries.len() < total {
            match self.iter.next() {
                None => self.done = true,
                Some(entry) => {
                    let (key, _value) = entry?;
                    self.examined += 1;
                    let body = key.get(9..).ok_or_else(|| RelationalError::InvalidInput {
                        detail: "index entry key shorter than the fixed 9-byte header".to_string(),
                    })?;
                    let (_indexed_values, consumed) =
                        decode_indexed_columns(&self.indexed_types, body)?;
                    let pk_bytes = &body[consumed..];
                    let pk_values = decode_composite_key(&self.pk_types, pk_bytes)?;
                    self.entries.push((pk_values, pk_bytes.to_vec()));
                }
            }
        }
        Ok(self.done)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_done(&self) -> bool {
        self.done
    }
}

/// What an index read is asked to enumerate (Increment 17).
#[derive(Debug, Clone)]
pub enum IndexScanSpec {
    /// Prefix equality over the index's leading columns.
    Equality(Vec<Option<RelationalValue>>),
    /// Inclusive/exclusive/unbounded bounds over the leading columns.
    Range {
        start: Bound<Vec<Option<RelationalValue>>>,
        end: Bound<Vec<Option<RelationalValue>>>,
    },
}

/// Phase-1 result of an index read: the enumerated entries (primary-key
/// values + encoded PK bytes) and the table metadata resolved once for the
/// scan, ready for `IndexBuilder::fetch_index_rows_as_of`.
pub struct IndexEntries {
    table: crate::catalog::schema::TableRow,
    columns: Vec<crate::catalog::schema::ColumnRow>,
    entries: Vec<(Vec<RelationalValue>, Vec<u8>)>,
}

impl IndexEntries {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops every entry whose encoded primary key fails `keep`. A
    /// transaction's scan uses this to hide base-snapshot rows the
    /// transaction has itself rewritten or deleted (their local version, if
    /// any, is added back by the overlay), *before* any row is fetched.
    pub fn retain_pk_bytes(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        self.entries.retain(|(_, pk_bytes)| keep(pk_bytes));
    }
}

/// Outcome of `IndexBuilder::probe_index_entries_as_of`.
pub enum IndexProbe {
    /// The index was not `Ready` as of the snapshot (F-2): do not use it.
    Unusable,
    /// More than the caller's `entry_limit` entries match; nothing fetched.
    Truncated,
    /// Every matching entry.
    Entries(IndexEntries),
}

fn as_bound_ref(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}
