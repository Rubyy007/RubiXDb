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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
        }
    }

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

        match self.backfill_and_activate(table_id, index_id) {
            Ok(()) => {
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
    fn backfill_and_activate(&self, table_id: u32, index_id: u32) -> Result<()> {
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
        self.backfill(table_id, &index_row)?;
        self.catalog.mark_index_ready(index_id)?;
        Ok(())
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
    fn backfill(&self, table_id: u32, index_row: &IndexRow) -> Result<u64> {
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

        let mut iter = self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), snap_seq);
        loop {
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
        }
        Ok(total_written)
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
        let building = self.catalog.list_indexes_in_state(IndexState::Building)?;
        let mut recovered = Vec::new();
        for index_row in building {
            let _slot = match acquire_build_slot(&self.active_builds) {
                Ok(slot) => slot,
                Err(e) => {
                    self.stats.index_errors.fetch_add(1, Ordering::Relaxed);
                    return Err(e);
                }
            };
            match self.backfill_and_activate(index_row.table_id, index_row.index_id) {
                Ok(()) => recovered.push(index_row.index_id),
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
        Ok(recovered)
    }

    /// Every index found `Dropping` at startup has its physical sweep
    /// restarted from the beginning of its key range (idempotent:
    /// deleting an already-tombstoned entry is a no-op) and, once
    /// complete, its catalog row removed. Returns the `index_id`s fully
    /// removed.
    pub fn recover_incomplete_drops(&self) -> Result<Vec<u32>> {
        let dropping = self.catalog.list_indexes_in_state(IndexState::Dropping)?;
        let mut recovered = Vec::new();
        for index_row in dropping {
            self.sweep_index_entries(index_row.table_id, index_row.index_id)?;
            self.catalog.remove_index_row(index_row.index_id)?;
            self.stats.index_drops.fetch_add(1, Ordering::Relaxed);
            recovered.push(index_row.index_id);
        }
        Ok(recovered)
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
        self.sweep_index_entries(table_id, index_id)?;
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
    fn sweep_index_entries(&self, table_id: u32, index_id: u32) -> Result<()> {
        let (range_start, range_end) = index_entry_range(table_id, index_id);
        let mut cursor = range_start;
        loop {
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
            cursor = Bound::Excluded(last_key.expect("batch non-empty"));
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Read access — index-then-fetch (item 16/17/18). Query-usable only
    // for a `Ready` index (item 8: a `Building` index must never be used
    // by normal reads).
    // -----------------------------------------------------------------

    fn ready_index(&self, index_id: u32) -> Result<IndexRow> {
        let index_row =
            self.catalog
                .get_index(index_id)?
                .ok_or_else(|| RelationalError::NotFound {
                    object: format!("index {index_id}"),
                })?;
        if index_row.state != IndexState::Ready {
            return Err(RelationalError::InvalidInput {
                detail: format!(
                    "index {index_id} is not Ready (state {:?}) and cannot be used for reads",
                    index_row.state
                ),
            });
        }
        Ok(index_row)
    }

    fn indexed_types(&self, index_row: &IndexRow) -> Result<Vec<RelationalType>> {
        let columns = self.catalog.get_columns(index_row.table_id)?;
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
        let index_row = self.ready_index(index_id)?;
        if prefix_values.is_empty() || prefix_values.len() > index_row.column_ordinals.len() {
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
        let (start, end) = index_entry_prefix_range(index_row.table_id, index_id, &prefix_bytes);
        let rows = self.scan_entries(
            &index_row,
            as_bound_ref(&start),
            as_bound_ref(&end),
            as_of_seq,
        )?;
        self.stats.index_lookups.fetch_add(1, Ordering::Relaxed);
        Ok(rows)
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
        let index_row = self.ready_index(index_id)?;
        let encode_bound = |b: Bound<Vec<Option<RelationalValue>>>| -> Result<Bound<Vec<u8>>> {
            Ok(match b {
                Bound::Unbounded => Bound::Unbounded,
                Bound::Included(v) => Bound::Included(encode_indexed_columns(&v)?),
                Bound::Excluded(v) => Bound::Excluded(encode_indexed_columns(&v)?),
            })
        };
        let start_bytes = encode_bound(start)?;
        let end_bytes = encode_bound(end)?;
        let (phys_start, phys_end) =
            index_scan_range(index_row.table_id, index_id, start_bytes, end_bytes);
        let rows = self.scan_entries(
            &index_row,
            as_bound_ref(&phys_start),
            as_bound_ref(&phys_end),
            as_of_seq,
        )?;
        self.stats.index_range_scans.fetch_add(1, Ordering::Relaxed);
        Ok(rows)
    }

    fn scan_entries(
        &self,
        index_row: &IndexRow,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        as_of_seq: u64,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        let indexed_types = self.indexed_types(index_row)?;
        let table = self.catalog.get_table(index_row.table_id)?.ok_or_else(|| {
            RelationalError::NotFound {
                object: format!("table {}", index_row.table_id),
            }
        })?;
        let columns = self.catalog.get_columns(index_row.table_id)?;
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

        let mut out = Vec::new();
        let mut examined: u64 = 0;
        for entry in self.engine.range_scan(start, end, as_of_seq) {
            let (key, _value) = entry?;
            examined += 1;
            let body = key.get(9..).ok_or_else(|| RelationalError::InvalidInput {
                detail: "index entry key shorter than the fixed 9-byte header".to_string(),
            })?;
            let (_indexed_values, consumed) = decode_indexed_columns(&indexed_types, body)?;
            let pk_bytes = &body[consumed..];
            let pk_values = decode_composite_key(&pk_types, pk_bytes)?;
            // Both the index-entry scan above and this row fetch use the
            // *same* `as_of_seq` (`u64::MAX` for the un-snapshotted
            // `index_lookup`/`index_range_scan` callers, a real pinned
            // seq for `..._as_of`) — this is what actually closes the
            // index-then-fetch race the un-snapshotted path still has
            // (see `index_lookup_as_of`'s own doc comment): a row
            // deleted *after* `as_of_seq` still resolves here, exactly
            // as D10 requires; one deleted strictly *before* it is
            // correctly absent, not an error, same as always.
            if let Some(row) =
                self.table_store
                    .get_row_as_of(index_row.table_id, &pk_values, as_of_seq)?
            {
                out.push((pk_values, row));
            }
        }
        self.stats
            .index_entries_examined
            .fetch_add(examined, Ordering::Relaxed);
        Ok(out)
    }
}

fn as_bound_ref(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}
