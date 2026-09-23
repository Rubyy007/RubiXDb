//! The transaction engine — `PHASE_RELATIONAL_TRANSACTION_ARCHITECTURE.md`,
//! implementing D10's already-approved Snapshot Isolation model exactly:
//! `BEGIN` captures an `LsmEngine::Snapshot`; reads resolve against it,
//! overlaid with the transaction's own buffered writes (read-your-own-
//! writes); writes stay in an in-memory write-set until `COMMIT`, which
//! re-validates every touched physical key's freshness against the
//! *current* committed state and, for `UNIQUE` indexes, a real existence
//! check, then applies the whole write-set through one `LsmEngine::
//! write_batch` call — atomically, table row and every affected index
//! entry together (D11). `ROLLBACK` discards the write-set; nothing was
//! ever durable to undo.
//!
//! **Write skew is possible under Snapshot Isolation** — a documented,
//! accepted D10 limitation, not a bug (see `write_skew_is_possible_
//! under_snapshot_isolation` in this module's own tests). This is *not*
//! Serializable isolation, and this module never claims it is.
//!
//! No SQL executor exists yet (`sql/`'s own stop condition) — this
//! module's API takes already-resolved `table_id`s and already-typed
//! `RelationalValue`s, never SQL text or names, so a future executor can
//! use it directly without reparsing or re-binding (item 43).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLockWriteGuard};
use std::time::Instant;

use crate::catalog::schema::{IndexKind, IndexRow};
use crate::lsm::{LsmEngine, Snapshot, WriteOp};
use crate::relational::error::{RelationalError, Result};
use crate::relational::index_key::{encode_indexed_columns, index_entry_prefix_range};
use crate::relational::key::{encode_composite_key, table_row_key};
use crate::relational::table_store::{self, decode_full_row, indexed_entry_key, Row, TableStore};
use crate::relational::value::RelationalValue;

// =======================================================================
// Resource limits (item 10/32 — an ADR amendment: D27 states "max
// transaction write-set size 10,000 ops, configurable" and "max
// concurrent transactions, configurable, bounded pool" but not an exact
// byte bound; `max_write_set_bytes` is this increment's own smallest-
// necessary addition, sized as a round, generous-but-bounded default —
// documented here, not silently invented elsewhere).
// =======================================================================

#[derive(Debug, Clone, Copy)]
pub struct TxnLimits {
    /// D27's own default, verbatim.
    pub max_write_set_ops: usize,
    /// Not specified by D27 — this increment's own addition, defense in
    /// depth alongside `max_write_set_ops` (a small number of enormous
    /// `TEXT`/`BLOB` values could otherwise still exhaust memory within
    /// the op-count budget).
    pub max_write_set_bytes: usize,
    /// D27's own default, verbatim.
    pub max_concurrent_transactions: usize,
}

impl Default for TxnLimits {
    fn default() -> Self {
        TxnLimits {
            max_write_set_ops: 10_000,
            max_write_set_bytes: 16 * 1024 * 1024,
            max_concurrent_transactions: 1_000,
        }
    }
}

// =======================================================================
// Observability (item 46) — bounded-cardinality counters only; never a
// transaction ID, SQL text, table name, key, value, or principal as a
// label (the recorder is a fixed set of atomics, structurally incapable
// of holding any of that).
// =======================================================================

#[derive(Debug, Default)]
struct TxnMetrics {
    transactions_started: AtomicU64,
    transactions_committed: AtomicU64,
    transactions_rolled_back: AtomicU64,
    transactions_aborted: AtomicU64,
    transaction_conflicts: AtomicU64,
    write_set_ops_total: AtomicU64,
    write_set_bytes_total: AtomicU64,
    commit_duration_ms_total: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxnMetricsSnapshot {
    pub transactions_started: u64,
    pub transactions_committed: u64,
    pub transactions_rolled_back: u64,
    pub transactions_aborted: u64,
    pub transaction_conflicts: u64,
    pub active_transactions: u64,
    pub write_set_ops_total: u64,
    pub write_set_bytes_total: u64,
    pub commit_duration_ms_total: u64,
}

// =======================================================================
// Lifecycle
// =======================================================================

/// `PHASE_RELATIONAL_TRANSACTION_ARCHITECTURE.md` §3's lifecycle.
/// `Committing` is deliberately *not* a state a caller can ever observe
/// (item 4 lists it as a candidate state, but this implementation's
/// commit critical section is a single, uninterruptible, lock-held
/// sequence with no yield point a concurrent reader of `state()` could
/// observe mid-way — modeling it as a fourth state would suggest an
/// external observability hook this design does not have and does not
/// need).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    Active,
    Committed,
    RolledBack,
    /// Commit was attempted but validation found a conflict — the
    /// transaction never applied anything.
    Aborted,
}

enum RowOp {
    Put(Row),
    Delete,
}

/// `table_id -> encoded_pk -> (pk_values, latest op)` — see
/// `Transaction::writes`'s own doc comment.
type WriteSet = HashMap<u32, HashMap<Vec<u8>, (Vec<RelationalValue>, RowOp)>>;

fn approx_row_bytes(values: &[Option<RelationalValue>]) -> usize {
    values
        .iter()
        .map(|v| match v {
            None => 1,
            Some(RelationalValue::Text(s)) => s.len() + 8,
            Some(RelationalValue::Blob(b)) => b.len() + 8,
            Some(_) => 16,
        })
        .sum()
}

fn as_bound_ref(bound: &std::ops::Bound<Vec<u8>>) -> std::ops::Bound<&[u8]> {
    match bound {
        std::ops::Bound::Included(v) => std::ops::Bound::Included(v.as_slice()),
        std::ops::Bound::Excluded(v) => std::ops::Bound::Excluded(v.as_slice()),
        std::ops::Bound::Unbounded => std::ops::Bound::Unbounded,
    }
}

struct Inner {
    engine: Arc<LsmEngine>,
    table_store: Arc<TableStore>,
    next_txn_id: AtomicU64,
    active_count: AtomicUsize,
    limits: TxnLimits,
    metrics: TxnMetrics,
}

/// Owns the shared state every `Transaction` needs: the certified
/// engine and `TableStore` (never a duplicated catalog/table handle —
/// `TableStore` already owns the one `CatalogService` reference every
/// resolution goes through), the bounded active-transaction count (item
/// 22/32), resource limits, and metrics. Cheap to clone (`Arc`-backed);
/// intended to be constructed once per process and shared.
#[derive(Clone)]
pub struct TransactionManager(Arc<Inner>);

impl TransactionManager {
    pub fn new(engine: Arc<LsmEngine>, table_store: Arc<TableStore>) -> Self {
        Self::with_limits(engine, table_store, TxnLimits::default())
    }

    pub fn with_limits(
        engine: Arc<LsmEngine>,
        table_store: Arc<TableStore>,
        limits: TxnLimits,
    ) -> Self {
        TransactionManager(Arc::new(Inner {
            engine,
            table_store,
            next_txn_id: AtomicU64::new(1),
            active_count: AtomicUsize::new(0),
            limits,
            metrics: TxnMetrics::default(),
        }))
    }

    /// `BEGIN` — item 6: captures the engine's `Snapshot` exactly once
    /// (never re-snapshotted per statement); item 32: rejected before
    /// any allocation if the concurrent-transaction bound is already at
    /// capacity.
    pub fn begin(&self) -> Result<Transaction> {
        let active = self.0.active_count.fetch_add(1, Ordering::AcqRel) + 1;
        if active > self.0.limits.max_concurrent_transactions {
            self.0.active_count.fetch_sub(1, Ordering::AcqRel);
            return Err(RelationalError::ResourceLimit {
                detail: format!(
                    "too many concurrent transactions (max {})",
                    self.0.limits.max_concurrent_transactions
                ),
            });
        }
        let id = self.0.next_txn_id.fetch_add(1, Ordering::Relaxed);
        let snapshot = self.0.engine.snapshot();
        self.0
            .metrics
            .transactions_started
            .fetch_add(1, Ordering::Relaxed);
        Ok(Transaction {
            inner: Arc::clone(&self.0),
            id,
            snapshot,
            state: TxnState::Active,
            writes: HashMap::new(),
            op_count: 0,
            byte_count: 0,
            finished: false,
        })
    }

    /// item 17: the reusable autocommit primitive a future single-
    /// statement SQL executor uses for `INSERT`/`UPSERT` — `BEGIN` →
    /// `put_row` → `COMMIT` as one atomic boundary, never a separate,
    /// duplicated write path (`TableStore::put_row` remains the direct,
    /// non-transactional entry point it already was; this is an
    /// additive alternative built on the same primitives, not a
    /// replacement).
    pub fn autocommit_put_row(
        &self,
        table_id: u32,
        values: &[Option<RelationalValue>],
    ) -> Result<u64> {
        let mut tx = self.begin()?;
        tx.put_row(table_id, values)?;
        tx.commit()
    }

    pub fn autocommit_delete_row(
        &self,
        table_id: u32,
        pk_values: &[RelationalValue],
    ) -> Result<u64> {
        let mut tx = self.begin()?;
        tx.delete_row(table_id, pk_values)?;
        tx.commit()
    }

    pub fn active_transactions(&self) -> u64 {
        self.0.active_count.load(Ordering::Relaxed) as u64
    }

    pub fn metrics(&self) -> TxnMetricsSnapshot {
        let m = &self.0.metrics;
        TxnMetricsSnapshot {
            transactions_started: m.transactions_started.load(Ordering::Relaxed),
            transactions_committed: m.transactions_committed.load(Ordering::Relaxed),
            transactions_rolled_back: m.transactions_rolled_back.load(Ordering::Relaxed),
            transactions_aborted: m.transactions_aborted.load(Ordering::Relaxed),
            transaction_conflicts: m.transaction_conflicts.load(Ordering::Relaxed),
            active_transactions: self.active_transactions(),
            write_set_ops_total: m.write_set_ops_total.load(Ordering::Relaxed),
            write_set_bytes_total: m.write_set_bytes_total.load(Ordering::Relaxed),
            commit_duration_ms_total: m.commit_duration_ms_total.load(Ordering::Relaxed),
        }
    }
}

/// One transaction's read view + buffered write-set. Never `Clone`/
/// `Copy` (matching `Snapshot`'s own deliberate non-`Clone`ness) —
/// `commit`/`rollback` consume `self` by value, so calling either twice,
/// or calling any other method after either, is a **compile-time**
/// error (a moved value), not a runtime one — the strongest possible
/// answer to item 4/30's "illegal transitions must return controlled
/// errors, do not panic": most of them are not reachable at all.
pub struct Transaction {
    inner: Arc<Inner>,
    id: u64,
    snapshot: Snapshot,
    state: TxnState,
    /// `table_id -> encoded_pk -> (pk_values, latest op)` — read-your-
    /// own-writes storage (item 8/9). Only the *latest* op per key is
    /// kept (PUT then DELETE then PUT collapses to the final PUT), which
    /// is both correct (item 8's own enumerated cases) and what keeps
    /// `op_count` bounded by *distinct keys touched*, not by call count.
    writes: WriteSet,
    op_count: usize,
    byte_count: usize,
    finished: bool,
}

impl Transaction {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn state(&self) -> TxnState {
        self.state
    }

    /// `PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md` §2: the pinned
    /// seq this transaction's `BEGIN` captured — the one piece of
    /// snapshot state a caller outside this module needs to perform a
    /// *consistent, scan-shaped* read (`TableStore::scan_table_as_of`,
    /// `IndexBuilder::index_lookup_as_of`/`index_range_scan_as_of`) at
    /// the same snapshot `get_row`'s own internal `get_as_of` call
    /// already uses. Read-only; exposes no way to construct or mutate a
    /// `Snapshot`, and this transaction's own registration in
    /// `SnapshotRegistry` (keeping `oldest_live_snapshot_seq` correct)
    /// is entirely unaffected by reading this value.
    pub fn snapshot_seq(&self) -> u64 {
        self.snapshot.seq()
    }

    fn require_active(&self) -> Result<()> {
        match self.state {
            TxnState::Active => Ok(()),
            other => Err(RelationalError::InvalidTransactionState {
                detail: format!("transaction is {other:?}, expected Active"),
            }),
        }
    }

    // -------------------------------------------------------------
    // Reads — item 7/8/18: snapshot-consistent, overlaid with this
    // transaction's own buffered writes, never the engine's live
    // mutable state directly.
    // -------------------------------------------------------------

    pub fn get_row(&self, table_id: u32, pk_values: &[RelationalValue]) -> Result<Option<Row>> {
        self.require_active()?;
        let (table, columns) = self.inner.table_store.resolve_table(table_id)?;
        if pk_values.len() != table.pk_ordinals.len() {
            return Err(RelationalError::InvalidInput {
                detail: format!(
                    "table {table_id} has a {}-column primary key, got {} value(s)",
                    table.pk_ordinals.len(),
                    pk_values.len()
                ),
            });
        }
        let encoded_pk = encode_composite_key(pk_values)?;
        if let Some(table_writes) = self.writes.get(&table_id) {
            if let Some((_, op)) = table_writes.get(&encoded_pk) {
                return Ok(match op {
                    RowOp::Put(row) => Some(row.clone()),
                    RowOp::Delete => None,
                });
            }
        }
        let key = table_row_key(table_id, &encoded_pk);
        match self.inner.engine.get_as_of(&key, self.snapshot.seq())? {
            None => Ok(None),
            Some(bytes) => Ok(Some(decode_full_row(&table, &columns, pk_values, &bytes)?)),
        }
    }

    // -------------------------------------------------------------
    // Writes — item 9: buffered only, never touch the engine until
    // `commit` (D10's own explicit design).
    // -------------------------------------------------------------

    pub fn put_row(&mut self, table_id: u32, values: &[Option<RelationalValue>]) -> Result<()> {
        self.require_active()?;
        let (table, columns) = self.inner.table_store.resolve_table(table_id)?;
        table_store::validate_row_shape(&table, &columns, values)?;
        let pk_values = table_store::extract_pk_values(&table, values);
        let encoded_pk = encode_composite_key(&pk_values)?;
        let bytes = approx_row_bytes(values);
        self.record_op(
            table_id,
            encoded_pk,
            pk_values,
            RowOp::Put(values.to_vec()),
            bytes,
        )
    }

    pub fn delete_row(&mut self, table_id: u32, pk_values: &[RelationalValue]) -> Result<()> {
        self.require_active()?;
        let (table, _columns) = self.inner.table_store.resolve_table(table_id)?;
        if pk_values.len() != table.pk_ordinals.len() {
            return Err(RelationalError::InvalidInput {
                detail: format!(
                    "table {table_id} has a {}-column primary key, got {} value(s)",
                    table.pk_ordinals.len(),
                    pk_values.len()
                ),
            });
        }
        let encoded_pk = encode_composite_key(pk_values)?;
        self.record_op(table_id, encoded_pk, pk_values.to_vec(), RowOp::Delete, 0)
    }

    fn record_op(
        &mut self,
        table_id: u32,
        encoded_pk: Vec<u8>,
        pk_values: Vec<RelationalValue>,
        op: RowOp,
        approx_bytes: usize,
    ) -> Result<()> {
        let table_writes = self.writes.entry(table_id).or_default();
        let is_new_key = !table_writes.contains_key(&encoded_pk);
        if is_new_key && self.op_count >= self.inner.limits.max_write_set_ops {
            return Err(RelationalError::ResourceLimit {
                detail: format!(
                    "transaction write-set exceeds max_write_set_ops ({})",
                    self.inner.limits.max_write_set_ops
                ),
            });
        }
        if self.byte_count + approx_bytes > self.inner.limits.max_write_set_bytes {
            return Err(RelationalError::ResourceLimit {
                detail: format!(
                    "transaction write-set exceeds max_write_set_bytes ({})",
                    self.inner.limits.max_write_set_bytes
                ),
            });
        }
        table_writes.insert(encoded_pk, (pk_values, op));
        if is_new_key {
            self.op_count += 1;
        }
        self.byte_count += approx_bytes;
        Ok(())
    }

    // -------------------------------------------------------------
    // ROLLBACK — item 16: discard the write-set; nothing durable to
    // undo, since nothing durable was ever written.
    // -------------------------------------------------------------

    pub fn rollback(mut self) -> Result<()> {
        self.require_active()?;
        self.finish(TxnState::RolledBack);
        self.inner
            .metrics
            .transactions_rolled_back
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    // -------------------------------------------------------------
    // COMMIT — item 11-15: freshness + UNIQUE validation, then one
    // atomic `write_batch` call.
    // -------------------------------------------------------------

    /// Commits the transaction. Returns the durable sequence of the
    /// `write_batch` call (or the snapshot's own seq for a read-only
    /// transaction that touched no keys — no engine call is made in
    /// that case).
    pub fn commit(mut self) -> Result<u64> {
        self.require_active()?;
        let start = Instant::now();

        if self.writes.is_empty() {
            let seq = self.snapshot.seq();
            self.finish(TxnState::Committed);
            self.inner
                .metrics
                .transactions_committed
                .fetch_add(1, Ordering::Relaxed);
            return Ok(seq);
        }

        let mut table_ids: Vec<u32> = self.writes.keys().copied().collect();
        table_ids.sort_unstable();

        // The commit critical section: one epoch *write* lock per
        // touched table, acquired in a fixed (sorted) order across every
        // concurrently committing transaction — the deadlock-avoidance
        // discipline that makes acquiring more than one lock safe.
        // `PHASE_RELATIONAL_TRANSACTION_ARCHITECTURE.md` §6 has the full
        // proof this closes both the ordinary write-write TOCTOU race
        // and the `UNIQUE`-existence-check race, reusing the *same*
        // lock `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §6/§7 already
        // certified for online index builds — never a second, new
        // synchronization primitive.
        let locks: Vec<_> = table_ids
            .iter()
            .map(|&id| self.inner.table_store.epoch_lock(id))
            .collect();
        let _guards: Vec<RwLockWriteGuard<'_, ()>> = locks
            .iter()
            .map(|l| l.write().unwrap_or_else(|p| p.into_inner()))
            .collect();

        match self.validate_and_build_ops(&table_ids) {
            Ok(physical_ops) => {
                let seq = self.inner.engine.write_batch(&physical_ops)?;
                drop(_guards);
                self.finish(TxnState::Committed);
                self.inner
                    .metrics
                    .transactions_committed
                    .fetch_add(1, Ordering::Relaxed);
                self.inner
                    .metrics
                    .write_set_ops_total
                    .fetch_add(self.op_count as u64, Ordering::Relaxed);
                self.inner
                    .metrics
                    .write_set_bytes_total
                    .fetch_add(self.byte_count as u64, Ordering::Relaxed);
                self.inner
                    .metrics
                    .commit_duration_ms_total
                    .fetch_add(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                Ok(seq)
            }
            Err(e) => {
                drop(_guards);
                let is_conflict = matches!(e, RelationalError::Conflict { .. });
                self.finish(TxnState::Aborted);
                if is_conflict {
                    self.inner
                        .metrics
                        .transactions_aborted
                        .fetch_add(1, Ordering::Relaxed);
                    self.inner
                        .metrics
                        .transaction_conflicts
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(e)
            }
        }
    }

    /// Runs entirely under the caller's held epoch write-locks. Computes
    /// the complete physical `WriteOp` set (table rows + every affected
    /// index delta, reusing `table_store`'s own D11 logic verbatim —
    /// never a second implementation) and validates every freshness/
    /// uniqueness precondition before returning it. Returns
    /// `Err(Conflict)` — and constructs no partial `WriteOp` list the
    /// caller could mistakenly apply — the instant any check fails.
    fn validate_and_build_ops(&self, table_ids: &[u32]) -> Result<Vec<WriteOp>> {
        struct UniqueCheck {
            table_id: u32,
            index: IndexRow,
            prefix: Vec<u8>,
            our_pk: Vec<u8>,
        }

        let mut physical_ops: Vec<WriteOp> = Vec::new();
        let mut row_keys_to_validate: Vec<Vec<u8>> = Vec::new();
        let mut unique_checks: Vec<UniqueCheck> = Vec::new();

        for &table_id in table_ids {
            let (table, columns) = self.inner.table_store.resolve_table(table_id)?;
            let indexes = self.inner.table_store.maintained_indexes(table_id)?;
            let table_writes = &self.writes[&table_id];

            for (encoded_pk, (pk_values, op)) in table_writes {
                let row_key = table_row_key(table_id, encoded_pk);

                let old_row = match self.inner.engine.get_as_of(&row_key, self.snapshot.seq())? {
                    None => None,
                    Some(bytes) => Some(decode_full_row(&table, &columns, pk_values, &bytes)?),
                };
                let new_row: Option<Row> = match op {
                    RowOp::Put(row) => Some(row.clone()),
                    RowOp::Delete => None,
                };

                match &new_row {
                    Some(row) => physical_ops.push(table_store::build_put_op(
                        &table, encoded_pk, &columns, row,
                    )?),
                    None => physical_ops.push(WriteOp::Delete {
                        key: row_key.clone(),
                    }),
                }
                if !indexes.is_empty() {
                    physical_ops.extend(table_store::index_maintenance_ops(
                        table_id,
                        &indexes,
                        old_row.as_ref(),
                        new_row.as_ref(),
                        encoded_pk,
                    )?);
                }

                row_keys_to_validate.push(row_key);

                if let Some(row) = &new_row {
                    for index in indexes.iter().filter(|i| i.kind == IndexKind::Unique) {
                        let new_key = indexed_entry_key(table_id, index, row, encoded_pk)?;
                        let old_key = old_row
                            .as_ref()
                            .map(|r| indexed_entry_key(table_id, index, r, encoded_pk))
                            .transpose()?;
                        if old_key.as_ref() != Some(&new_key) {
                            let mut values = Vec::with_capacity(index.column_ordinals.len());
                            for &ord in &index.column_ordinals {
                                values.push(row.get(ord as usize).cloned().flatten());
                            }
                            // Standard SQL UNIQUE semantics (ISO/IEC 9075,
                            // matched by PostgreSQL/SQLite): NULL is never
                            // equal to another NULL, so a UNIQUE index never
                            // rejects a row solely because every indexed
                            // column is NULL. Only skip when *every* indexed
                            // column is NULL — a composite `UNIQUE(a, b)`
                            // with one NULL and one present column still
                            // enforces uniqueness on the present part.
                            if values.iter().any(|v| v.is_some()) {
                                unique_checks.push(UniqueCheck {
                                    table_id,
                                    index: index.clone(),
                                    prefix: encode_indexed_columns(&values)?,
                                    our_pk: encoded_pk.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }

        // 1. Row-level freshness (D10: "re-reads the current value of
        // every physical key the write-set touches, compares against
        // what the transaction's snapshot saw" — value comparison, not
        // seq comparison: the engine's public read API exposes values,
        // not per-key seqs, and value equality is exactly what "no
        // committed change since my snapshot" means here).
        for row_key in &row_keys_to_validate {
            let at_snapshot = self.inner.engine.get_as_of(row_key, self.snapshot.seq())?;
            let now = self.inner.engine.get_as_of(row_key, u64::MAX)?;
            if at_snapshot != now {
                return Err(RelationalError::Conflict {
                    detail: "a row this transaction wrote was modified after its snapshot"
                        .to_string(),
                });
            }
        }

        // 2a. Intra-transaction UNIQUE collision: two different rows in
        // *this same* write-set independently claiming one value. The
        // engine-state scan below cannot see this (neither row is
        // durable yet), so it is checked separately, first.
        {
            let mut claimed: HashMap<(u32, Vec<u8>), Vec<u8>> = HashMap::new();
            for check in &unique_checks {
                let key = (check.index.index_id, check.prefix.clone());
                if let Some(existing_pk) = claimed.get(&key) {
                    if existing_pk != &check.our_pk {
                        return Err(RelationalError::Conflict {
                            detail: "two rows in the same transaction claim the same UNIQUE value"
                                .to_string(),
                        });
                    }
                } else {
                    claimed.insert(key, check.our_pk.clone());
                }
            }
        }

        // 2b. Inter-transaction UNIQUE existence check, against current
        // engine state (race-free: every concurrently committing
        // transaction on this table is excluded by the epoch write-lock
        // held for this whole function). A found entry belonging to a
        // row *this same transaction* is deleting, or is itself moving
        // away from this value, is not a real conflict -- it will be
        // gone/changed by the time this transaction's own `write_batch`
        // applies (handles delete-then-reinsert-same-value within one
        // transaction correctly, not just the simple case).
        for check in &unique_checks {
            let (start, end) =
                index_entry_prefix_range(check.table_id, check.index.index_id, &check.prefix);
            let header_and_prefix_len = 9 + check.prefix.len();
            for entry in
                self.inner
                    .engine
                    .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
            {
                let (key, _value) = entry?;
                let entry_pk = key.get(header_and_prefix_len..).unwrap_or(&[]);
                if entry_pk == check.our_pk.as_slice() {
                    continue; // our own row's own (about-to-be-written) entry
                }
                if self.entry_is_self_vacated(
                    check.table_id,
                    entry_pk,
                    &check.index,
                    &check.prefix,
                )? {
                    continue;
                }
                return Err(RelationalError::Conflict {
                    detail: "UNIQUE constraint violated by an already-committed row".to_string(),
                });
            }
        }

        Ok(physical_ops)
    }

    /// Whether the index entry at `entry_pk` (some other row's PK, found
    /// by the `UNIQUE` existence scan) belongs to a row *this same
    /// transaction* is deleting, or is overwriting to a different
    /// indexed value for `index` — in either case that entry will not
    /// exist once this transaction's own `write_batch` applies, so it is
    /// not a real conflict against the *new* state this commit produces.
    fn entry_is_self_vacated(
        &self,
        table_id: u32,
        entry_pk: &[u8],
        index: &IndexRow,
        prefix: &[u8],
    ) -> Result<bool> {
        let Some(table_writes) = self.writes.get(&table_id) else {
            return Ok(false);
        };
        let Some((_, op)) = table_writes.get(entry_pk) else {
            return Ok(false);
        };
        match op {
            RowOp::Delete => Ok(true),
            RowOp::Put(row) => {
                let mut values = Vec::with_capacity(index.column_ordinals.len());
                for &ord in &index.column_ordinals {
                    values.push(row.get(ord as usize).cloned().flatten());
                }
                let new_prefix = encode_indexed_columns(&values)?;
                Ok(new_prefix != prefix)
            }
        }
    }

    fn finish(&mut self, state: TxnState) {
        self.state = state;
        if !self.finished {
            self.finished = true;
            self.inner.active_count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        // item 47: a caller that simply drops an active transaction gets
        // an implicit rollback, never an implicit commit — and the
        // active-transaction count/registry is always released, even if
        // the caller never explicitly called `rollback`.
        if !self.finished {
            self.finished = true;
            self.inner.active_count.fetch_sub(1, Ordering::AcqRel);
            self.inner
                .metrics
                .transactions_rolled_back
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}
