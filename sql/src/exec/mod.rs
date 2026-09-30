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
    /// Set only by `AggregateOp` (Increment 11) on the tuple it emits per
    /// group — the group's finalized aggregate results, positionally
    /// indexed exactly as `BoundSelect::aggregates`/`BoundExprKind::
    /// AggregateRef` addresses them. Empty for every row below an
    /// `Aggregate` plan node (no query this crate plans ever evaluates
    /// an `AggregateRef` there — `crate::bind::select`'s own binder never
    /// produces one outside an aggregated statement's projection/
    /// `HAVING`/`ORDER BY`).
    aggregates: Vec<Option<RelationalValue>>,
}

impl RowContext {
    pub fn new() -> Self {
        RowContext {
            rows: Vec::new(),
            aggregates: Vec::new(),
        }
    }

    pub fn single(table_ref: u32, row: Row) -> Self {
        RowContext {
            rows: vec![(table_ref, row)],
            aggregates: Vec::new(),
        }
    }

    /// `AggregateOp`'s own constructor for the tuple it emits per group —
    /// `self`'s row bindings are kept unchanged (the group's
    /// representative row, still resolvable by any `Column`/`group_by`-
    /// matching expression above), `aggregates` is the group's finalized
    /// per-aggregate results.
    pub fn with_aggregates(mut self, aggregates: Vec<Option<RelationalValue>>) -> RowContext {
        self.aggregates = aggregates;
        self
    }

    pub fn get_aggregate(&self, index: usize) -> Option<&RelationalValue> {
        self.aggregates.get(index).and_then(|v| v.as_ref())
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
        // Neither side of a `Join` (the only caller) ever carries
        // `aggregates` — `AggregateOp` always sits above every `Join` in
        // any plan this crate builds — but this is written to be correct
        // either way rather than silently dropping one side's values.
        let aggregates = if !self.aggregates.is_empty() {
            self.aggregates.clone()
        } else {
            other.aggregates.clone()
        };
        RowContext { rows, aggregates }
    }

    /// Item 29/30: the unmatched-inner-row case for `LEFT JOIN` — see
    /// this type's own doc comment for why an empty `Row` suffices.
    pub fn with_null_extension(&self, table_ref: u32) -> RowContext {
        let mut rows = self.rows.clone();
        rows.push((table_ref, Vec::new()));
        RowContext {
            rows,
            aggregates: self.aggregates.clone(),
        }
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
    /// Item 30/73/74 (`AggregateOp`): the hard cap on distinct `GROUP BY`
    /// groups one query's hash-aggregation state may hold before failing
    /// closed. Checked *before* a new group is inserted, never after —
    /// the same discipline `max_materialized_rows` already applies to
    /// `Distinct`'s seen-value set, which this defaults to matching
    /// exactly (a group's own state is a structurally similar "one entry
    /// per distinct key" resource shape; no independent constant is
    /// invented for it).
    pub max_group_count: usize,
    /// Item 30/34/59/73: the hard cap on total estimated bytes
    /// (`GroupingKey::estimated_bytes` + `AggregateState::estimated_
    /// bytes`, summed across every live group) one query's aggregation
    /// state may occupy. `max_group_count` alone bounds group *count*,
    /// but a `TEXT`/`BLOB` grouping key or `MIN`/`MAX` state can each be
    /// arbitrarily large per group — this is the independent byte-level
    /// bound item 30 requires when a pure count-based limit is not
    /// itself sufficient. 256 MiB is a defensible, documented, non-
    /// arbitrary production default: two orders of magnitude below the
    /// crate's smallest process-level assumption (a multi-GB host), and
    /// the same order of magnitude as `max_materialized_rows`'s own
    /// worst-case `Tuple` buffer for a wide row shape — chosen and
    /// recorded here (`PHASE_RELATIONAL_AGGREGATION_ARCHITECTURE.md`)
    /// per item 30's own "choose a defensible bound and document it"
    /// instruction, since no existing limit already covers this shape.
    pub max_aggregate_state_bytes: usize,
}

impl Default for ExecLimits {
    fn default() -> Self {
        ExecLimits {
            max_result_rows: 100_000,
            max_materialized_rows: 1_000_000,
            max_index_scan_rows: 1_000_000,
            deadline: Some(std::time::Duration::from_secs(30)),
            max_dml_target_rows: 10_000,
            max_group_count: 1_000_000,
            max_aggregate_state_bytes: 256 * 1024 * 1024,
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
    pk_range_scans: AtomicU64,
    joins: AtomicU64,
    execution_time_ms_total: AtomicU64,
    /// Item 71/72: incremented once per newly-created `GROUP BY` group
    /// (never per input row — a group already seen on a later row is not
    /// double-counted).
    groups_created: AtomicU64,
    /// Item 71/72: the final group count actually emitted as result
    /// rows, one query's own `AggregateOp::materialize` call at a time —
    /// always equal to `groups_created` for that same query (kept as a
    /// separate counter, not derived, so a divergence would itself be
    /// observable evidence of a bug rather than something the metric
    /// shape could hide).
    groups_emitted: AtomicU64,
    /// Item 71/72: input rows `AggregateOp` consumed — distinct from
    /// `rows_scanned` (which counts raw storage-layer fetches below any
    /// `WHERE` filtering) and from `groups_emitted` (never conflated with
    /// either).
    aggregate_rows_processed: AtomicU64,
    /// Item 71: incremented once per `SqlError::ResourceLimit` an
    /// aggregation-specific check (`max_group_count`/`max_aggregate_
    /// state_bytes`) actually raised.
    aggregate_resource_limit_hits: AtomicU64,
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
    pub pk_range_scans: u64,
    pub joins: u64,
    pub execution_time_ms_total: u64,
    pub groups_created: u64,
    pub groups_emitted: u64,
    pub aggregate_rows_processed: u64,
    pub aggregate_resource_limit_hits: u64,
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
    pub fn record_pk_range_scan(&self) {
        self.pk_range_scans.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_join(&self) {
        self.joins.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_group_created(&self) {
        self.groups_created.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_groups_emitted(&self, n: u64) {
        self.groups_emitted.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_aggregate_rows_processed(&self, n: u64) {
        self.aggregate_rows_processed
            .fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_aggregate_resource_limit_hit(&self) {
        self.aggregate_resource_limit_hits
            .fetch_add(1, Ordering::Relaxed);
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
            pk_range_scans: self.pk_range_scans.load(Ordering::Relaxed),
            joins: self.joins.load(Ordering::Relaxed),
            execution_time_ms_total: self.execution_time_ms_total.load(Ordering::Relaxed),
            groups_created: self.groups_created.load(Ordering::Relaxed),
            groups_emitted: self.groups_emitted.load(Ordering::Relaxed),
            aggregate_rows_processed: self.aggregate_rows_processed.load(Ordering::Relaxed),
            aggregate_resource_limit_hits: self
                .aggregate_resource_limit_hits
                .load(Ordering::Relaxed),
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
