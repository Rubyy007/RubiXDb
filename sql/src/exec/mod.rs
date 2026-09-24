//! The query executor — `PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md`.
//! Consumes a `crate::plan::Plan`/`PhysicalPlan` and produces typed
//! result rows against the real `TableStore`/`IndexBuilder`/
//! `Transaction` primitives. **Read-only this increment**: only
//! `Plan::Query` (a bound `SELECT`) is executed; every other `Plan`
//! variant (`Insert`/`Update`/`Delete`/`Ddl`/`Begin`/`Commit`/
//! `Rollback`) returns `SqlError::UnsupportedExecution` rather than
//! silently doing nothing or something wrong (item 5's "all writes are
//! outside this increment," item 65's "no plan node may silently fall
//! through").

pub mod expr_eval;
pub mod operators;
pub mod write;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::{RelationalType, RelationalValue, Row, Transaction};

use crate::bound::BoundSelectItem;
use crate::error::{Result, SqlError};
use crate::plan::physical::PhysicalPlan;
use crate::plan::Plan;
use operators::build_operator;

// =======================================================================
// Row context — item 8/9/19/28/29: the multi-table row state every
// `BoundExpr` evaluation resolves `ColumnRef::table_ref` against. An
// unmatched `LEFT JOIN` inner side is represented as an *empty* `Row`
// (`vec![]`) rather than a correctly-column-counted all-`NULL` row —
// `RowContext::get`'s own out-of-bounds `Vec::get` already returns
// `None` for any ordinal against an empty row, which is exactly SQL
// `NULL`, so no operator needs to know the inner table's column count
// just to null-extend it (item 29's "emit exactly one row with NULL
// values for the inner side," achieved without a catalog lookup).
// =======================================================================

#[derive(Clone, Debug, Default)]
pub struct RowContext {
    rows: Vec<(u32, Row)>,
}

impl RowContext {
    pub fn new() -> Self {
        RowContext { rows: Vec::new() }
    }

    pub fn single(table_ref: u32, row: Row) -> Self {
        RowContext {
            rows: vec![(table_ref, row)],
        }
    }

    pub fn get(&self, table_ref: u32, ordinal: u16) -> Option<&RelationalValue> {
        self.rows
            .iter()
            .find(|(tr, _)| *tr == table_ref)
            .and_then(|(_, row)| row.get(ordinal as usize))
            .and_then(|v| v.as_ref())
    }

    /// The full physical `Row` bound to `table_ref`, if any — `crate::
    /// exec::write`'s own requirement: `UPDATE`'s new-row construction
    /// needs every column of the old row (not just the ones an
    /// assignment or predicate happens to reference), and `DELETE`
    /// needs the full row to extract its `PRIMARY KEY` columns.
    pub fn row_for(&self, table_ref: u32) -> Option<&Row> {
        self.rows
            .iter()
            .find(|(tr, _)| *tr == table_ref)
            .map(|(_, row)| row)
    }

    /// Combines this context with another (a `Join`'s own left/right row
    /// contexts) — item 28: exact join multiplicity depends on never
    /// losing either side's own bindings.
    pub fn merged(&self, other: &RowContext) -> RowContext {
        let mut rows = self.rows.clone();
        rows.extend(other.rows.iter().cloned());
        RowContext { rows }
    }

    /// Item 29/30: the unmatched-inner-row case for `LEFT JOIN` — see
    /// this type's own doc comment for why an empty `Row` suffices.
    pub fn with_null_extension(&self, table_ref: u32) -> RowContext {
        let mut rows = self.rows.clone();
        rows.push((table_ref, Vec::new()));
        RowContext { rows }
    }
}

/// One row flowing through the operator tree. `ctx` is the full multi-
/// table row state (needed by anything — `Sort`, in particular — whose
/// own `BoundExpr`s may reference a column outside the final `SELECT`
/// list, item 27's standard-SQL allowance); `projected` is filled in by
/// a `Projection` operator and simply carried, unread, by every operator
/// below it and passed through unchanged by every operator above it
/// (`Distinct`/`Sort`/`Limit`) until the top-level driver extracts it as
/// the caller-visible result row.
#[derive(Clone, Debug, Default)]
pub struct Tuple {
    pub ctx: RowContext,
    pub projected: Vec<Option<RelationalValue>>,
}

// =======================================================================
// Result schema — item 7/8: typed metadata, never collapsed to `String`.
// =======================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct ResultField {
    pub name: String,
    pub ty: Option<RelationalType>,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultSchema {
    pub fields: Vec<ResultField>,
}

impl ResultSchema {
    pub fn from_projection(items: &[BoundSelectItem]) -> Self {
        ResultSchema {
            fields: items
                .iter()
                .map(|item| ResultField {
                    name: item.output_name.clone(),
                    ty: item.expr.ty,
                    nullable: item.expr.nullable,
                })
                .collect(),
        }
    }
}

pub type ResultRow = Vec<Option<RelationalValue>>;

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub schema: ResultSchema,
    pub rows: Vec<ResultRow>,
}

// =======================================================================
// Resource limits (item 22/24/42/70) — checked before the corresponding
// allocation, never after.
// =======================================================================

#[derive(Debug, Clone, Copy)]
pub struct ExecLimits {
    /// Item 42: the hard cap on rows a single query's `QueryResult` may
    /// buffer. Exceeding it is a controlled `ResourceLimit` error, never
    /// silent truncation.
    pub max_result_rows: usize,
    /// Item 22/24: the row-count cap `Distinct`'s own seen-value set and
    /// `Sort`'s own materialization buffer may each grow to before
    /// failing closed. Distinct from `max_result_rows` because a
    /// `DISTINCT`/`ORDER BY` over a huge, low-selectivity input can need
    /// to examine far more rows than it will ever return.
    pub max_materialized_rows: usize,
    /// Item 42's own escape hatch for pathological `IndexScan`
    /// selectivity (`PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md`
    /// §2's documented scope boundary: `IndexBuilder::index_lookup_as_
    /// of`/`index_range_scan_as_of` are not lazy — this bounds their
    /// eager `Vec` instead of leaving it unbounded).
    pub max_index_scan_rows: usize,
    /// Item 41: wall-clock (monotonic) budget for one query's entire
    /// execution, checked at every operator's own natural iteration
    /// point — never only at the top level, since a single `Filter`
    /// call that discards every row without yielding would otherwise
    /// never come back to a top-level check at all.
    pub deadline: Option<std::time::Duration>,
    /// Item 47/48 (`crate::exec::write`): the hard cap on how many
    /// primary keys an `UPDATE`/`DELETE`'s own target-row-finding pass
    /// may collect before failing closed with a controlled
    /// `ResourceLimit` error — checked *while collecting*, not only
    /// once collection finishes, so a predicate matching millions of
    /// rows can never build an unbounded in-memory `Vec` first and only
    /// discover the problem afterward. Defaults to the same value as
    /// `Transaction`'s own certified `TxnLimits::max_write_set_ops`
    /// (D27) — the write-set this many target rows would produce is
    /// exactly at that same certified boundary either way; this check
    /// exists to fail with a clear, write-executor-attributed error
    /// *before* reaching it, not to impose a materially different bound.
    pub max_dml_target_rows: usize,
}

impl Default for ExecLimits {
    fn default() -> Self {
        ExecLimits {
            max_result_rows: 100_000,
            max_materialized_rows: 1_000_000,
            max_index_scan_rows: 1_000_000,
            deadline: Some(std::time::Duration::from_secs(30)),
            max_dml_target_rows: 10_000,
        }
    }
}

/// Item 40's own "smallest correct internal mechanism": a plain shared
/// `AtomicBool` a caller sets from another thread to request early
/// termination. Checked at the same points `ExecLimits::deadline` is —
/// one shared `check` call covers both (item 40's "every potentially
/// long operation should periodically check cancellation").
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        CancellationToken(Arc::new(AtomicBool::new(false)))
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

// =======================================================================
// Metrics (item 45/46) — bounded-cardinality counters only.
// =======================================================================

#[derive(Debug, Default)]
pub struct ExecMetrics {
    queries_executed: AtomicU64,
    queries_failed: AtomicU64,
    queries_cancelled: AtomicU64,
    rows_scanned: AtomicU64,
    rows_returned: AtomicU64,
    rows_filtered: AtomicU64,
    rows_sorted: AtomicU64,
    distinct_rows: AtomicU64,
    index_rows_examined: AtomicU64,
    table_fetches: AtomicU64,
    pk_lookups: AtomicU64,
    seq_scans: AtomicU64,
    index_scans: AtomicU64,
    joins: AtomicU64,
    execution_time_ms_total: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecMetricsSnapshot {
    pub queries_executed: u64,
    pub queries_failed: u64,
    pub queries_cancelled: u64,
    pub rows_scanned: u64,
    pub rows_returned: u64,
    pub rows_filtered: u64,
    pub rows_sorted: u64,
    pub distinct_rows: u64,
    pub index_rows_examined: u64,
    pub table_fetches: u64,
    pub pk_lookups: u64,
    pub seq_scans: u64,
    pub index_scans: u64,
    pub joins: u64,
    pub execution_time_ms_total: u64,
}

impl ExecMetrics {
    pub fn record_rows_scanned(&self, n: u64) {
        self.rows_scanned.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_rows_returned(&self, n: u64) {
        self.rows_returned.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_rows_filtered(&self, n: u64) {
        self.rows_filtered.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_rows_sorted(&self, n: u64) {
        self.rows_sorted.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_distinct_rows(&self, n: u64) {
        self.distinct_rows.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_index_rows_examined(&self, n: u64) {
        self.index_rows_examined.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_table_fetch(&self) {
        self.table_fetches.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_pk_lookup(&self) {
        self.pk_lookups.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_seq_scan(&self) {
        self.seq_scans.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_index_scan(&self) {
        self.index_scans.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_join(&self) {
        self.joins.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ExecMetricsSnapshot {
        ExecMetricsSnapshot {
            queries_executed: self.queries_executed.load(Ordering::Relaxed),
            queries_failed: self.queries_failed.load(Ordering::Relaxed),
            queries_cancelled: self.queries_cancelled.load(Ordering::Relaxed),
            rows_scanned: self.rows_scanned.load(Ordering::Relaxed),
            rows_returned: self.rows_returned.load(Ordering::Relaxed),
            rows_filtered: self.rows_filtered.load(Ordering::Relaxed),
            rows_sorted: self.rows_sorted.load(Ordering::Relaxed),
            distinct_rows: self.distinct_rows.load(Ordering::Relaxed),
            index_rows_examined: self.index_rows_examined.load(Ordering::Relaxed),
            table_fetches: self.table_fetches.load(Ordering::Relaxed),
            pk_lookups: self.pk_lookups.load(Ordering::Relaxed),
            seq_scans: self.seq_scans.load(Ordering::Relaxed),
            index_scans: self.index_scans.load(Ordering::Relaxed),
            joins: self.joins.load(Ordering::Relaxed),
            execution_time_ms_total: self.execution_time_ms_total.load(Ordering::Relaxed),
        }
    }
}

// =======================================================================
// Execution context — item 4/5/35: everything execution needs and
// nothing else. No raw SQL text, no credentials, no filesystem path, no
// mutable global state.
// =======================================================================

pub struct ExecCtx<'a> {
    /// Item 35: every read goes through this transaction's own pinned
    /// snapshot (`Transaction::get_row`) or its `snapshot_seq()`
    /// (`TableStore`/`IndexBuilder`'s `..._as_of` primitives,
    /// `PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md` §2) — never a
    /// second, separately-invented query-snapshot mechanism. Autocommit
    /// execution (`execute_autocommit`, below) supplies a throwaway
    /// transaction here; an explicit transaction supplies the caller's
    /// own.
    pub txn: &'a Transaction,
    pub table_store: &'a TableStore,
    pub index_builder: &'a IndexBuilder,
    /// One-based `$n` parameter values, already validated against the
    /// statement's own declared `max_parameter` at plan-build time
    /// (`crate::plan::validate`) — index 0 here is `$1`.
    pub params: &'a [Option<RelationalValue>],
    pub limits: &'a ExecLimits,
    pub metrics: &'a ExecMetrics,
    pub cancellation: &'a CancellationToken,
    deadline_at: Option<Instant>,
}

impl<'a> ExecCtx<'a> {
    pub fn new(
        txn: &'a Transaction,
        table_store: &'a TableStore,
        index_builder: &'a IndexBuilder,
        params: &'a [Option<RelationalValue>],
        limits: &'a ExecLimits,
        metrics: &'a ExecMetrics,
        cancellation: &'a CancellationToken,
    ) -> Self {
        let deadline_at = limits.deadline.map(|d| Instant::now() + d);
        ExecCtx {
            txn,
            table_store,
            index_builder,
            params,
            limits,
            metrics,
            cancellation,
            deadline_at,
        }
    }

    /// Item 40/41/71: called at every operator's own natural iteration
    /// point (never only once at the top level — see `ExecLimits::
    /// deadline`'s own doc comment for why). Monotonic (`Instant`), never
    /// wall-clock.
    pub fn check(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(SqlError::Cancelled);
        }
        if let Some(at) = self.deadline_at {
            if Instant::now() >= at {
                return Err(SqlError::DeadlineExceeded);
            }
        }
        Ok(())
    }

    pub fn resolve_parameter(&self, index: u32) -> Result<Option<RelationalValue>> {
        let ordinal =
            (index as usize)
                .checked_sub(1)
                .ok_or_else(|| SqlError::ExecutionParameter {
                    detail: "parameter index must be >= 1".to_string(),
                })?;
        self.params
            .get(ordinal)
            .cloned()
            .ok_or_else(|| SqlError::ExecutionParameter {
                detail: "missing runtime value for a referenced parameter".to_string(),
            })
    }
}

// =======================================================================
// Top-level entry points
// =======================================================================

/// Executes an already-planned `SELECT` (`Plan::Query`) against an
/// explicit, caller-owned `Transaction` — item 4/35/36/37: every read
/// resolves through that transaction's own snapshot and local write-set
/// overlay (`Transaction::get_row`) or its pinned `snapshot_seq()`
/// (scan-shaped reads), exactly as already certified by `PHASE_
/// RELATIONAL_TRANSACTION_ARCHITECTURE.md` — this function adds no
/// second read-consistency mechanism of its own.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    plan: &Plan,
    txn: &Transaction,
    table_store: &TableStore,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    metrics: &ExecMetrics,
    cancellation: &CancellationToken,
) -> Result<QueryResult> {
    let start = Instant::now();
    let result = execute_inner(
        plan,
        txn,
        table_store,
        index_builder,
        params,
        limits,
        metrics,
        cancellation,
    );
    metrics
        .execution_time_ms_total
        .fetch_add(start.elapsed().as_millis() as u64, Ordering::Relaxed);
    match &result {
        Ok(_) => {
            metrics.queries_executed.fetch_add(1, Ordering::Relaxed);
        }
        Err(SqlError::Cancelled) => {
            metrics.queries_cancelled.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            metrics.queries_failed.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn execute_inner(
    plan: &Plan,
    txn: &Transaction,
    table_store: &TableStore,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    metrics: &ExecMetrics,
    cancellation: &CancellationToken,
) -> Result<QueryResult> {
    let (physical, projection_items) = match plan {
        Plan::Query { physical, .. } => (physical, final_projection_items(physical)),
        Plan::Explain(inner) => {
            return execute_inner(
                inner,
                txn,
                table_store,
                index_builder,
                params,
                limits,
                metrics,
                cancellation,
            )
        }
        other => {
            return Err(SqlError::UnsupportedExecution {
                detail: format!(
                    "{} is not executable this increment (item 5: writes are out of scope)",
                    plan_kind(other)
                ),
            })
        }
    };

    let ctx = ExecCtx::new(
        txn,
        table_store,
        index_builder,
        params,
        limits,
        metrics,
        cancellation,
    );
    let schema = projection_items
        .map(ResultSchema::from_projection)
        .unwrap_or_default();

    let mut operator = build_operator(physical, &RowContext::new(), &ctx)?;
    let mut rows = Vec::new();
    loop {
        ctx.check()?;
        match operator.next(&ctx)? {
            None => break,
            Some(tuple) => {
                if rows.len() >= limits.max_result_rows {
                    return Err(SqlError::ResourceLimit {
                        detail: format!(
                            "query result exceeds max_result_rows ({})",
                            limits.max_result_rows
                        ),
                    });
                }
                rows.push(tuple.projected);
            }
        }
    }
    metrics.record_rows_returned(rows.len() as u64);
    Ok(QueryResult { schema, rows })
}

fn plan_kind(plan: &Plan) -> &'static str {
    match plan {
        Plan::Query { .. } => "Query",
        Plan::Insert(_) => "Insert",
        Plan::Update { .. } => "Update",
        Plan::Delete { .. } => "Delete",
        Plan::Ddl(_) => "Ddl",
        Plan::Begin => "Begin",
        Plan::Commit => "Commit",
        Plan::Rollback => "Rollback",
        Plan::Explain(_) => "Explain",
    }
}

fn final_projection_items(plan: &PhysicalPlan) -> Option<&[BoundSelectItem]> {
    match plan {
        PhysicalPlan::Projection { items, .. } => Some(items),
        PhysicalPlan::Distinct { input }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Limit { input, .. } => final_projection_items(input),
        _ => None,
    }
}

/// Item 5's "autocommit execution foundation": begins a throwaway
/// read-only `Transaction` (reusing D10's own snapshot mechanism, never
/// a second one, item 5/45), executes, and commits it (trivial/no-op
/// for a read-only write-set, already certified — `PHASE_RELATIONAL_
/// TRANSACTION_ARCHITECTURE.md` §4's "an empty write-set commits
/// trivially... zero `write_batch` calls").
#[allow(clippy::too_many_arguments)]
pub fn execute_autocommit(
    plan: &Plan,
    txm: &rubixdb::relational::TransactionManager,
    table_store: &TableStore,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    metrics: &ExecMetrics,
    cancellation: &CancellationToken,
) -> Result<QueryResult> {
    let txn = txm.begin()?;
    let result = execute(
        plan,
        &txn,
        table_store,
        index_builder,
        params,
        limits,
        metrics,
        cancellation,
    );
    // A pure read never has anything in its write-set; commit is the
    // trivial, zero-`write_batch` path either way, and is preferred over
    // `rollback` here only for its `Ok(seq)` return shape symmetry —
    // both are equally correct for a read-only transaction.
    let _ = txn.commit();
    result
}
