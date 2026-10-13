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

mod access_op;
pub mod cost;
pub mod expr_eval;
pub mod operators;
mod recursive;
mod relation;
pub(crate) mod setop;
mod subquery;
mod window;
pub mod write;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
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
    /// SQL-mission-2: the values the `Window` operators below computed for this
    /// row, indexed by window slot (`BoundExprKind::WindowRef`). Slots no
    /// operator has filled yet read as `NULL`.
    windows: Vec<Option<RelationalValue>>,
    /// SQL-mission-2: set only by `AggregateOp` for `ROLLUP` / `CUBE` /
    /// `GROUPING SETS`: the value of each grouping expression for the output
    /// row (`NULL` where its grouping set leaves the expression out), indexed
    /// like `BoundSelect::group_by`.
    group_keys: Vec<Option<RelationalValue>>,
    /// Bit `i` is set when grouping expression `i` is left out of this row's
    /// grouping set (what `GROUPING()` reports). 0 for every ordinary row.
    grouping_mask: u64,
}

impl RowContext {
    pub fn new() -> Self {
        RowContext::default()
    }

    pub fn single(table_ref: u32, row: Row) -> Self {
        RowContext {
            rows: vec![(table_ref, row)],
            ..RowContext::default()
        }
    }

    /// The same bindings with a different set of rows (every derived value --
    /// aggregates, window results, group keys -- carries over).
    fn with_rows(&self, rows: Vec<(u32, Row)>) -> RowContext {
        RowContext {
            rows,
            aggregates: self.aggregates.clone(),
            windows: self.windows.clone(),
            group_keys: self.group_keys.clone(),
            grouping_mask: self.grouping_mask,
        }
    }

    /// `AggregateOp`'s constructor for the output row of a grouping set: the
    /// grouping expressions' values and the set's "left out" bit mask.
    pub fn with_grouping(
        mut self,
        keys: Vec<Option<RelationalValue>>,
        mask: u64,
    ) -> RowContext {
        self.group_keys = keys;
        self.grouping_mask = mask;
        self
    }

    pub fn get_group_key(&self, index: usize) -> Option<&RelationalValue> {
        self.group_keys.get(index).and_then(|v| v.as_ref())
    }

    pub fn grouping_mask(&self) -> u64 {
        self.grouping_mask
    }

    /// Stores window slot `slot`'s value for this row.
    pub fn set_window_value(&mut self, slot: usize, value: Option<RelationalValue>) {
        if self.windows.len() <= slot {
            self.windows.resize(slot + 1, None);
        }
        self.windows[slot] = value;
    }

    pub fn get_window(&self, slot: usize) -> Option<&RelationalValue> {
        self.windows.get(slot).and_then(|v| v.as_ref())
    }

    /// Estimated bytes the rows of this context occupy (for the window
    /// operator's materialization bound).
    pub fn estimated_bytes(&self) -> usize {
        self.rows
            .iter()
            .map(|(_, row)| crate::exec::setop::row_bytes(row))
            .sum::<usize>()
            + std::mem::size_of::<RowContext>()
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

    /// `self` plus one more table's row, taking the row by value -- exactly
    /// `self.merged(&RowContext::single(table_ref, row))` without cloning the
    /// row a second time (every fetched row of every access passes through
    /// here).
    pub fn with_row(&self, table_ref: u32, row: Row) -> RowContext {
        let mut rows = Vec::with_capacity(self.rows.len() + 1);
        rows.extend(self.rows.iter().cloned());
        rows.push((table_ref, row));
        self.with_rows(rows)
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
        RowContext {
            rows,
            aggregates,
            ..RowContext::default()
        }
    }

    /// Item 29/30: the unmatched-inner-row case for `LEFT JOIN` — see
    /// this type's own doc comment for why an empty `Row` suffices.
    pub fn with_null_extension(&self, table_ref: u32) -> RowContext {
        let mut rows = self.rows.clone();
        rows.push((table_ref, Vec::new()));
        self.with_rows(rows)
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
    /// Increment 17: how the executor chooses between an `IndexScan` and
    /// a table scan (`Auto` = the cost model). The `Force*` modes select
    /// between result-equivalent paths and exist for benchmarking and
    /// diagnostics; they are server-side configuration, never reachable
    /// from SQL.
    pub access_path: crate::exec::cost::AccessPathMode,
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
    /// SQL-mission-1: the hard cap on the estimated bytes one subquery's
    /// materialized result (an `IN (SELECT ...)` value set; later, a CTE's or
    /// set operation's row buffer) may occupy. `max_materialized_rows` bounds
    /// the *count*; this bounds the *size*, because a few rows of large
    /// `TEXT`/`BLOB` values can exhaust memory long before the row cap. Same
    /// default and rationale as `max_aggregate_state_bytes` (a structurally
    /// similar "state held for the life of one statement" resource).
    pub max_materialized_bytes: usize,
    /// SQL-mission-1 Slice B: the most times *all* correlated subqueries of
    /// one statement together may be re-executed (once per enclosing-query
    /// row that drives them). A correlated subquery runs its inner query once
    /// per outer row, so without this bound a large outer table turns one
    /// statement into an unbounded amount of work. Exceeding it is
    /// `RESOURCE_LIMIT`, never a silent truncation.
    ///
    /// Default 1,000,000: the same figure as `max_materialized_rows` and
    /// `max_group_count` -- the codebase's existing "one statement may touch
    /// about a million of these" budget -- so a correlated query over a
    /// million-row outer table is still permitted while one over a hundred
    /// million is refused up front rather than left to run into the
    /// deadline.
    pub max_correlated_reexecutions: u64,
    /// SQL-mission-1 Slice B: the most times *one* correlated subquery may be
    /// evaluated in a statement, i.e. the number of enclosing-query rows
    /// allowed to drive it. A secondary guard under
    /// `max_correlated_reexecutions`: it stops a single subquery from
    /// consuming the whole budget (and, for a subquery nested in another
    /// correlated subquery, counts every evaluation across all the outer
    /// evaluations).
    ///
    /// Default 100,000: one tenth of the statement-wide budget, so a
    /// statement with several correlated subqueries over a table of up to
    /// 100,000 rows runs, while one runaway subquery cannot starve the rest.
    pub max_subquery_outer_rows: u64,
    /// SQL-mission-1 Slice C: the most `WITH` definitions (CTEs) one
    /// statement may contain, counted over the whole statement including
    /// CTEs defined inside subqueries and derived tables. Checked when
    /// execution starts, before any work. Default 64: far more than a
    /// hand-written query uses, small enough that the per-definition
    /// bookkeeping (one materialized row set each) stays bounded.
    pub max_cte_definitions: u32,
    /// The most CTE *references* (`FROM name` occurrences, across all CTEs)
    /// one statement may contain. Default 256: four references to each of the
    /// default 64 definitions.
    pub max_cte_references: u32,
    /// The deepest nesting of derived tables (`FROM (SELECT ... FROM (SELECT
    /// ...))`) one statement may execute. Default 32, the same as the
    /// subquery nesting limit (`SqlLimits::max_subquery_depth`), which the
    /// binder also applies to derived tables.
    pub max_derived_table_depth: u32,
    /// SQL-mission-1 Slice E: the most operand queries of set operations
    /// (`UNION` / `INTERSECT` / `EXCEPT`) one statement may execute, counted
    /// over the whole statement: for every set operation, one per operand that
    /// is not itself directly a set operation, so `A UNION B INTERSECT C` is 3.
    /// Checked when execution starts, before any work. Default 32: a
    /// hand-written query combines a handful of queries; every operand is a
    /// full query plan that runs (and, for `INTERSECT` / `EXCEPT` /
    /// `UNION`, holds rows), so the count is what bounds the work and memory a
    /// small statement can ask for. The rows a set operation holds are bounded
    /// by the existing `max_materialized_rows` / `max_materialized_bytes`.
    pub max_set_op_branches: u32,
    /// SQL-mission-1 Slice D: the most times one recursive CTE's recursive term
    /// may be evaluated (iterations of its fixpoint) in one statement. Default
    /// 1,000: a hand-written recursion (a counter, a hierarchy, a graph walk)
    /// finishes in tens of iterations, while a missing stop condition would
    /// otherwise run for ever; the bound is per CTE and not shared. Exceeding it
    /// is `RESOURCE_LIMIT` naming the CTE.
    pub max_recursive_cte_depth: u32,
    /// The most rows one recursive CTE may produce in total, the seed and every
    /// iteration included. Default 1,000,000, the codebase's usual "about a
    /// million rows" budget (`max_materialized_rows`). The rows held are also
    /// bounded in bytes by `max_materialized_bytes`.
    pub max_recursive_cte_rows: u64,
    /// The most recursive CTEs one statement may run. Default 8: each owns a
    /// working table and a result set, and a statement needing more is almost
    /// certainly generated.
    pub max_recursive_cte_definitions: u32,
    /// SQL-mission-2: the most rows one window partition may hold. A partition
    /// is buffered whole (its frame arithmetic needs random access to it), so
    /// this bounds the memory of a window function over an unpartitioned or
    /// skewed relation. Default 1,000,000, the codebase's "about a million rows"
    /// budget (`max_materialized_rows`); the bytes the buffered rows occupy are
    /// also bounded by `max_materialized_bytes`. Exceeding either is
    /// `RESOURCE_LIMIT`, never a truncated result.
    pub max_window_partition_rows: u64,
    /// SQL-mission-2: the most distinct window function calls one statement may
    /// evaluate (every call is a pass over the buffered rows). Default 64, the
    /// same as `max_cte_definitions`: far more than a hand-written query uses.
    pub max_window_functions_per_statement: u32,
    /// SQL-mission-2: the most grouping sets all `ROLLUP` / `CUBE` /
    /// `GROUPING SETS` clauses of one statement may expand to (each set holds
    /// its own groups, so this multiplies the aggregation state). Default 64:
    /// `CUBE` of six columns, or `ROLLUP` of sixty-three.
    pub max_grouping_sets_per_statement: u32,
}

impl Default for ExecLimits {
    fn default() -> Self {
        ExecLimits {
            max_result_rows: 100_000,
            max_materialized_rows: 1_000_000,
            max_index_scan_rows: 1_000_000,
            access_path: crate::exec::cost::AccessPathMode::Auto,
            deadline: Some(std::time::Duration::from_secs(30)),
            max_dml_target_rows: 10_000,
            max_group_count: 1_000_000,
            max_aggregate_state_bytes: 256 * 1024 * 1024,
            max_materialized_bytes: 256 * 1024 * 1024,
            max_correlated_reexecutions: 1_000_000,
            max_subquery_outer_rows: 100_000,
            max_cte_definitions: 64,
            max_cte_references: 256,
            max_derived_table_depth: 32,
            max_set_op_branches: 32,
            max_recursive_cte_depth: 1_000,
            max_recursive_cte_rows: 1_000_000,
            max_recursive_cte_definitions: 8,
            max_window_partition_rows: 1_000_000,
            max_window_functions_per_statement: 64,
            max_grouping_sets_per_statement: 64,
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
    index_snapshot_fallbacks: AtomicU64,
    index_cost_fallbacks: AtomicU64,
    access_path_switches: AtomicU64,
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
    /// SQL-mission-1: inner-query executions started for a subquery
    /// expression. An uncorrelated subquery contributes at most 1 per
    /// statement; a correlated one 1 per evaluation (per outer row).
    subquery_executions: AtomicU64,
    /// SQL-mission-1: evaluations of an uncorrelated subquery answered from
    /// the per-statement result cache instead of re-running the inner query.
    subquery_cache_hits: AtomicU64,
    /// SQL-mission-1 Slice B: evaluations of a *correlated* subquery, i.e.
    /// times its inner query was re-run for an enclosing-query row. (Each
    /// one is also counted in `subquery_executions`.)
    correlated_subquery_reexecs: AtomicU64,
    /// SQL-mission-1 Slice C: CTEs whose rows were materialized (once each
    /// per execution, however many references read them; a CTE nobody
    /// reads is never materialized).
    cte_materializations_total: AtomicU64,
    /// SQL-mission-1 Slice E: rows a set operation's distinct step removed --
    /// for `UNION`, a row equal to one already produced; for `INTERSECT` and
    /// `EXCEPT`, a left row that qualified but repeated one already produced.
    /// (`ALL` forms remove nothing; rows an `INTERSECT` / `EXCEPT` filters
    /// out for being absent from / present in the other side are not counted.)
    set_op_rows_deduplicated_total: AtomicU64,
    /// SQL-mission-1 Slice E: operand queries of the set operations a
    /// statement executes (counted once per statement, when it starts; see
    /// `ExecLimits::max_set_op_branches` for what is counted).
    set_op_branches_total: AtomicU64,
    /// SQL-mission-1 Slice D: evaluations of a recursive term (one per
    /// iteration, across every recursive CTE the statement runs).
    recursive_cte_iterations_total: AtomicU64,
    /// SQL-mission-1 Slice D: rows the fixpoints added to their results (seed
    /// rows and every iteration's new rows; under `UNION`, after deduplication).
    recursive_cte_rows_emitted_total: AtomicU64,
    /// SQL-mission-2: window function values computed -- one per window
    /// function per input row of its `Window` operator.
    window_functions_evaluated_total: AtomicU64,
    /// SQL-mission-2: distinct window partitions (distinct `PARTITION BY` keys;
    /// one for a window without `PARTITION BY` over a non-empty input) the
    /// `Window` operators formed.
    window_partitions_created_total: AtomicU64,
    /// SQL-mission-2: (row, aggregate) pairs an aggregate's `FILTER` clause kept
    /// out of its state (a `NULL` or false condition); counted for plain and
    /// window aggregates, once per input row however many grouping sets it feeds.
    aggregate_filter_rows_skipped_total: AtomicU64,
    /// SQL-mission-2: grouping sets aggregated -- the expanded sets of every
    /// `ROLLUP` / `CUBE` / `GROUPING SETS` aggregation the statement ran.
    grouping_sets_evaluated_total: AtomicU64,
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
    /// Increment 17 (F-2): index scans that ran as table scans because the
    /// index was not `Ready` as of the transaction's snapshot.
    pub index_snapshot_fallbacks: u64,
    /// Increment 17: index scans abandoned for a table scan by the cost
    /// model.
    pub index_cost_fallbacks: u64,
    /// Increment 18: accesses executed through a different candidate than
    /// the planner's structural choice (a cheaper PK range or secondary
    /// index, priced by exact row counts).
    pub access_path_switches: u64,
    pub pk_range_scans: u64,
    pub joins: u64,
    pub execution_time_ms_total: u64,
    pub groups_created: u64,
    pub groups_emitted: u64,
    pub aggregate_rows_processed: u64,
    pub aggregate_resource_limit_hits: u64,
    pub subquery_executions: u64,
    pub subquery_cache_hits: u64,
    pub correlated_subquery_reexecs: u64,
    pub cte_materializations_total: u64,
    pub set_op_rows_deduplicated_total: u64,
    pub set_op_branches_total: u64,
    pub recursive_cte_iterations_total: u64,
    pub recursive_cte_rows_emitted_total: u64,
    pub window_functions_evaluated_total: u64,
    pub window_partitions_created_total: u64,
    pub aggregate_filter_rows_skipped_total: u64,
    pub grouping_sets_evaluated_total: u64,
}

impl ExecMetrics {
    pub fn record_window_functions_evaluated(&self, n: u64) {
        self.window_functions_evaluated_total
            .fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_window_partitions_created(&self, n: u64) {
        self.window_partitions_created_total
            .fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_aggregate_filter_rows_skipped(&self, n: u64) {
        self.aggregate_filter_rows_skipped_total
            .fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_grouping_sets_evaluated(&self, n: u64) {
        self.grouping_sets_evaluated_total
            .fetch_add(n, Ordering::Relaxed);
    }

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
    pub fn record_index_snapshot_fallback(&self) {
        self.index_snapshot_fallbacks
            .fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_index_cost_fallback(&self) {
        self.index_cost_fallbacks.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_access_path_switch(&self) {
        self.access_path_switches.fetch_add(1, Ordering::Relaxed);
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

    pub fn record_subquery_execution(&self) {
        self.subquery_executions.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_subquery_cache_hit(&self) {
        self.subquery_cache_hits.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_set_op_rows_deduplicated(&self, n: u64) {
        self.set_op_rows_deduplicated_total
            .fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_recursive_cte_iteration(&self) {
        self.recursive_cte_iterations_total
            .fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_recursive_cte_rows_emitted(&self, n: u64) {
        self.recursive_cte_rows_emitted_total
            .fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_set_op_branches(&self, n: u64) {
        self.set_op_branches_total.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_cte_materialization(&self) {
        self.cte_materializations_total
            .fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_correlated_subquery_reexec(&self) {
        self.correlated_subquery_reexecs
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
            index_snapshot_fallbacks: self.index_snapshot_fallbacks.load(Ordering::Relaxed),
            index_cost_fallbacks: self.index_cost_fallbacks.load(Ordering::Relaxed),
            access_path_switches: self.access_path_switches.load(Ordering::Relaxed),
            pk_range_scans: self.pk_range_scans.load(Ordering::Relaxed),
            joins: self.joins.load(Ordering::Relaxed),
            execution_time_ms_total: self.execution_time_ms_total.load(Ordering::Relaxed),
            groups_created: self.groups_created.load(Ordering::Relaxed),
            groups_emitted: self.groups_emitted.load(Ordering::Relaxed),
            aggregate_rows_processed: self.aggregate_rows_processed.load(Ordering::Relaxed),
            aggregate_resource_limit_hits: self
                .aggregate_resource_limit_hits
                .load(Ordering::Relaxed),
            subquery_executions: self.subquery_executions.load(Ordering::Relaxed),
            subquery_cache_hits: self.subquery_cache_hits.load(Ordering::Relaxed),
            correlated_subquery_reexecs: self
                .correlated_subquery_reexecs
                .load(Ordering::Relaxed),
            cte_materializations_total: self.cte_materializations_total.load(Ordering::Relaxed),
            set_op_rows_deduplicated_total: self
                .set_op_rows_deduplicated_total
                .load(Ordering::Relaxed),
            set_op_branches_total: self.set_op_branches_total.load(Ordering::Relaxed),
            recursive_cte_iterations_total: self
                .recursive_cte_iterations_total
                .load(Ordering::Relaxed),
            recursive_cte_rows_emitted_total: self
                .recursive_cte_rows_emitted_total
                .load(Ordering::Relaxed),
            window_functions_evaluated_total: self
                .window_functions_evaluated_total
                .load(Ordering::Relaxed),
            window_partitions_created_total: self
                .window_partitions_created_total
                .load(Ordering::Relaxed),
            aggregate_filter_rows_skipped_total: self
                .aggregate_filter_rows_skipped_total
                .load(Ordering::Relaxed),
            grouping_sets_evaluated_total: self
                .grouping_sets_evaluated_total
                .load(Ordering::Relaxed),
        }
    }
}

// =======================================================================
// Execution context — item 4/5/35: everything execution needs and
// nothing else. No raw SQL text, no credentials, no filesystem path, no
// mutable global state.
// =======================================================================

/// Correlated-subquery evaluations already spent against
/// `max_correlated_reexecutions` / `max_subquery_outer_rows`. An `UPDATE`
/// evaluates its `WHERE` and then its `SET` expressions in separate execution
/// contexts; carrying this from one to the next keeps the limits per
/// *statement*, not per phase.
#[derive(Debug, Clone, Default)]
pub struct CorrelatedUsage {
    total: u64,
    by_subquery: HashMap<u32, u64>,
}

/// The stored rows of one materialized relation, shared by its readers.
type RelationRows = Rc<Vec<Vec<Option<RelationalValue>>>>;

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
    /// SQL-mission-1: results of *uncorrelated* subqueries already evaluated
    /// in this execution, keyed by the binder-allocated subquery id. An
    /// uncorrelated subquery's result cannot depend on the outer row, and
    /// every read in one execution goes through one pinned snapshot, so
    /// evaluating it once per statement is exact, not an approximation.
    /// Execution is single-threaded (`ExecCtx` is borrowed `&`, never
    /// shared across threads), so `RefCell` suffices.
    subquery_cache: RefCell<HashMap<u32, Rc<subquery::CachedSubquery>>>,
    /// SQL-mission-1 Slice B: correlated-subquery evaluations so far in this
    /// execution -- in total, and per subquery id -- checked against
    /// `ExecLimits::{max_correlated_reexecutions, max_subquery_outer_rows}`
    /// *before* each re-execution starts.
    correlated_total: Cell<u64>,
    correlated_by_subquery: RefCell<HashMap<u32, u64>>,
    /// SQL-mission-1 Slice C: rows of the relations materialized in this
    /// execution (every CTE; a derived table the join re-reads), keyed by
    /// relation id. Such a relation cannot read an enclosing row, so its rows
    /// are the same whenever and however often it is read in one statement.
    relation_cache: RefCell<HashMap<u32, RelationRows>>,
    /// SQL-mission-1 Slice D: the working table of each recursive CTE that is
    /// iterating (the previous iteration's rows), keyed by working-table id.
    working_tables: RefCell<HashMap<u32, RelationRows>>,
    /// The finished rows of each recursive CTE run in this execution, keyed by
    /// CTE id (a recursive CTE read twice runs its fixpoint once).
    recursive_cache: RefCell<HashMap<u32, RelationRows>>,
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
            subquery_cache: RefCell::new(HashMap::new()),
            correlated_total: Cell::new(0),
            correlated_by_subquery: RefCell::new(HashMap::new()),
            relation_cache: RefCell::new(HashMap::new()),
            working_tables: RefCell::new(HashMap::new()),
            recursive_cache: RefCell::new(HashMap::new()),
        }
    }

    /// What this context has spent against the correlated-subquery limits.
    pub fn correlated_usage(&self) -> CorrelatedUsage {
        CorrelatedUsage {
            total: self.correlated_total.get(),
            by_subquery: self.correlated_by_subquery.borrow().clone(),
        }
    }

    /// Continues counting from `usage` (see `CorrelatedUsage`).
    pub fn with_correlated_usage(self, usage: CorrelatedUsage) -> Self {
        self.correlated_total.set(usage.total);
        *self.correlated_by_subquery.borrow_mut() = usage.by_subquery;
        self
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

    let set_op_branches = check_relation_limits(plan, limits)?;
    metrics.record_set_op_branches(set_op_branches);
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

/// Refuses, before any work, a statement whose CTE definitions, CTE
/// references, derived-table nesting or set-operation operands exceed the
/// configured bounds (SQL-mission-1 Slices C and E). `RESOURCE_LIMIT`, never a
/// truncation. Returns the statement's set-operation operand count (for the
/// `set_op_branches_total` metric).
pub(crate) fn check_relation_limits(plan: &Plan, limits: &ExecLimits) -> Result<u64> {
    let stats = crate::plan::relstats::plan_stats(plan);
    if stats.recursive_cte_definitions.len() as u64 > u64::from(limits.max_recursive_cte_definitions) {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "statement runs {} recursive CTEs; max_recursive_cte_definitions is {}",
                stats.recursive_cte_definitions.len(),
                limits.max_recursive_cte_definitions
            ),
        });
    }
    if stats.set_op_branches > u64::from(limits.max_set_op_branches) {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "statement combines {} queries with UNION/INTERSECT/EXCEPT; max_set_op_branches is {}",
                stats.set_op_branches, limits.max_set_op_branches
            ),
        });
    }
    // Counts every definition in the statement, including ones nothing reads.
    let defined = crate::plan::relstats::defined_ctes(plan).max(stats.cte_definitions.len() as u32);
    if u64::from(defined) > u64::from(limits.max_cte_definitions) {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "statement defines {defined} CTEs; max_cte_definitions is {}",
                limits.max_cte_definitions
            ),
        });
    }
    if stats.window_functions > u64::from(limits.max_window_functions_per_statement) {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "statement evaluates {} window functions; max_window_functions_per_statement is {}",
                stats.window_functions, limits.max_window_functions_per_statement
            ),
        });
    }
    if stats.grouping_sets > u128::from(limits.max_grouping_sets_per_statement) {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "statement expands to {} grouping sets; max_grouping_sets_per_statement is {}",
                if stats.grouping_sets == u128::MAX {
                    "more than 2^120".to_string()
                } else {
                    stats.grouping_sets.to_string()
                },
                limits.max_grouping_sets_per_statement
            ),
        });
    }
    if stats.cte_references.len() as u64 > u64::from(limits.max_cte_references) {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "statement references CTEs {} times; max_cte_references is {}",
                stats.cte_references.len(),
                limits.max_cte_references
            ),
        });
    }
    if stats.max_derived_depth as u64 > u64::from(limits.max_derived_table_depth) {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "derived tables are nested {} deep; max_derived_table_depth is {}",
                stats.max_derived_depth, limits.max_derived_table_depth
            ),
        });
    }
    Ok(stats.set_op_branches)
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
        PhysicalPlan::SetOp(node) => Some(&node.items),
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
