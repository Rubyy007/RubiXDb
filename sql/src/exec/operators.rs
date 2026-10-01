//! Executable physical operators — item 9: one struct per `PhysicalPlan`
//! node kind, each with clear inputs/outputs, never one giant match
//! statement holding all execution logic. `Operator::next` is the one
//! pull-based interface every operator implements (item 6/44): a
//! consumer above only ever asks for the next row, so `LIMIT`,
//! cancellation, and bounded memory all fall out of the model itself
//! rather than needing special-casing per operator.

use std::ops::Bound;

use rubixdb::relational::index::{IndexProbe, IndexScanSpec};
use rubixdb::relational::{RelationalValue, Row};

use crate::aggregate::{AggregateArg, AggregateState, GroupingKey};
use crate::ast::JoinKind;
use crate::bound::{BoundAggregateExpr, BoundExpr, BoundOrderByItem, BoundSelectItem, NullsOrder};
use crate::error::{Result, SqlError};
use crate::exec::cost::{self, AccessPathMode};
use crate::exec::expr_eval::{eval, eval_predicate};
use crate::exec::{ExecCtx, RowContext, Tuple};
use crate::plan::access::{IndexAccessMode, IndexFallback, PhysicalAccess};
use crate::plan::physical::{JoinAlgorithm, PhysicalPlan};

/// The one interface every executable operator implements — item 6:
/// pull-based, so a consumer only ever asks for the next row. Carries
/// the same `'a` lifetime as the `ExecCtx`/storage borrows every
/// operator ultimately reads through (`AccessOp`'s own lazy `SeqScan`
/// iterator borrows `ExecCtx::table_store` directly) — every `next`
/// call across one query's execution passes the *same* `ExecCtx<'a>`,
/// so tying the trait itself to `'a` lets the compiler verify that
/// rather than merely assuming it.
pub trait Operator<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>>;
}

/// Builds an operator tree for `plan`, resolving any `BoundExpr` an
/// `Access` node's own key/bounds carry against `outer` — empty at the
/// top of a query, and the *current outer tuple's* own row context when
/// rebuilding a `Join`'s right side per outer row for a correlated
/// (`IndexNestedLoop`) lookup (item 31: "the inner lookup key must be
/// evaluated from the current outer row... do NOT cache one outer row's
/// lookup result for unrelated outer rows" — rebuilding fresh, here, is
/// exactly what prevents that).
pub fn build_operator<'a>(
    plan: &PhysicalPlan,
    outer: &RowContext,
    ec: &ExecCtx<'a>,
) -> Result<Box<dyn Operator<'a> + 'a>> {
    match plan {
        PhysicalPlan::EmptyRelation => Ok(Box::new(EmptyRelationOp { done: false })),
        PhysicalPlan::Access(access) => Ok(Box::new(AccessOp::build(access, outer, ec)?)),
        PhysicalPlan::Join {
            left,
            right,
            kind,
            on,
            algorithm,
        } => Ok(Box::new(NestedLoopJoinOp::build(
            left,
            (**right).clone(),
            *kind,
            on.clone(),
            *algorithm,
            outer,
            ec,
        )?)),
        PhysicalPlan::Filter { input, predicate } => Ok(Box::new(FilterOp {
            input: build_operator(input, outer, ec)?,
            predicate: predicate.clone(),
        })),
        PhysicalPlan::Projection { input, items } => Ok(Box::new(ProjectionOp {
            input: build_operator(input, outer, ec)?,
            items: items.clone(),
        })),
        PhysicalPlan::Distinct { input } => Ok(Box::new(DistinctOp {
            input: build_operator(input, outer, ec)?,
            seen: Vec::new(),
        })),
        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => Ok(Box::new(AggregateOp::build(
            build_operator(input, outer, ec)?,
            group_by.clone(),
            aggregates.clone(),
        ))),
        PhysicalPlan::Sort { input, items } => Ok(Box::new(SortOp::build(
            build_operator(input, outer, ec)?,
            items.clone(),
        ))),
        PhysicalPlan::Limit {
            input,
            limit,
            offset,
            ..
        } => Ok(Box::new(LimitOp::build(
            build_operator(input, outer, ec)?,
            limit.clone(),
            offset.clone(),
            outer,
            ec,
        )?)),
    }
}

// =======================================================================
// EmptyRelation — a `FROM`-less `SELECT`'s single implicit row.
// =======================================================================

struct EmptyRelationOp {
    done: bool,
}

impl<'a> Operator<'a> for EmptyRelationOp {
    fn next(&mut self, _ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Tuple {
            ctx: RowContext::new(),
            projected: Vec::new(),
        }))
    }
}

// =======================================================================
// Access — `PkLookup` / `IndexScan` / `SeqScan` (items 10/11/12/13/14/15).
// =======================================================================

enum AccessSource<'a> {
    /// `PkLookup`: at most one row, already fetched.
    Single(Option<Row>),
    /// `IndexScan`: `IndexBuilder`'s own eager `Vec` result (item 12/13;
    /// see `ExecLimits::max_index_scan_rows`'s own doc comment for why
    /// this is bounded rather than lazy).
    Vec(std::vec::IntoIter<(Vec<RelationalValue>, Row)>),
    /// `SeqScan`: genuinely lazy, row-at-a-time (item 10 — never
    /// materializes the whole table).
    Lazy(Box<dyn Iterator<Item = rubixdb::relational::Result<(Vec<RelationalValue>, Row)>> + 'a>),
}

struct AccessOp<'a> {
    table_ref: u32,
    residual: Option<BoundExpr>,
    source: AccessSource<'a>,
    /// The outer context this access was built against — merged with
    /// every fetched row before evaluating `residual`, since a
    /// correlated residual (rare, but structurally possible) may still
    /// reference the outer side.
    outer: RowContext,
    /// Cost-model observation of a lazy table scan (statistics only).
    obs: Option<ScanObs>,
}

impl<'a> AccessOp<'a> {
    fn build(access: &PhysicalAccess, outer: &RowContext, ec: &ExecCtx<'a>) -> Result<Self> {
        match access {
            PhysicalAccess::PkLookup {
                table_ref,
                key_values,
                residual,
                ..
            } => {
                let table_ref = *table_ref;
                let resolved = resolve_values(key_values, outer, ec)?;
                let row = match resolved {
                    None => None, // a NULL key component can never match (item 62)
                    Some(values) => {
                        ec.metrics.record_pk_lookup();
                        let row = ec.txn.get_row(access.table_id(), &values)?;
                        if row.is_some() {
                            ec.metrics.record_table_fetch();
                        }
                        row
                    }
                };
                Ok(AccessOp {
                    table_ref,
                    residual: residual.clone(),
                    source: AccessSource::Single(row),
                    outer: outer.clone(),
                    obs: None,
                })
            }
            PhysicalAccess::IndexScan {
                table_ref,
                index_id,
                mode,
                residual,
                fallback,
                ..
            } => {
                let table_ref = *table_ref;
                let table_id = access.table_id();
                let as_of = ec.txn.snapshot_seq();
                // Resolve the key/bounds from *this execution's* values (a
                // parameter, or the current outer row of a correlated
                // join). A NULL component can never match (item 62).
                let spec = match mode {
                    IndexAccessMode::Equality { prefix } => resolve_values(prefix, outer, ec)?
                        .map(|v| IndexScanSpec::Equality(v.into_iter().map(Some).collect())),
                    IndexAccessMode::Range { start, end } => {
                        match (
                            resolve_bound(start, outer, ec)?,
                            resolve_bound(end, outer, ec)?,
                        ) {
                            (Some(start), Some(end)) => Some(IndexScanSpec::Range { start, end }),
                            _ => None,
                        }
                    }
                };
                let Some(spec) = spec else {
                    return Ok(AccessOp {
                        table_ref,
                        residual: residual.clone(),
                        source: AccessSource::Vec(Vec::new().into_iter()),
                        outer: outer.clone(),
                        obs: None,
                    });
                };
                ec.metrics.record_index_scan();

                let path_mode = ec.limits.access_path;
                if path_mode == AccessPathMode::ForceSeq {
                    return Self::build_fallback(
                        table_id,
                        table_ref,
                        fallback,
                        outer,
                        ec,
                        FallbackReason::Cost,
                    );
                }

                // The cost-based decision happens here, at execution, because
                // it needs what a plan cannot know: the bound key values (a
                // correlated join supplies a different one per outer row),
                // the transaction snapshot, and the table's current size. An
                // `ORDER BY` an eliminated Sort relies on never abandons the
                // index (its ordering is part of its value).
                let may_abandon =
                    path_mode == AccessPathMode::Auto && fallback.order_ordinals.is_empty();
                let stats = ec.table_store.runtime_stats();
                let max_rows = ec.limits.max_index_scan_rows;
                let started = std::time::Instant::now();
                let mut counted = false;
                let entries = loop {
                    let est = stats.row_estimate(table_id);
                    let entry_limit = if may_abandon {
                        cost::entry_limit(est, stats.cost_params())
                    } else {
                        usize::MAX
                    };
                    match ec.index_builder.probe_index_entries_as_of(
                        *index_id,
                        &spec,
                        as_of,
                        entry_limit,
                        max_rows,
                    )? {
                        IndexProbe::Entries(entries) => break entries,
                        // F-2: the index was not Ready as of this snapshot,
                        // so it does not represent the table this
                        // transaction sees. Never use it, never return an
                        // empty/partial result: run the semantically
                        // identical table scan instead.
                        IndexProbe::Unusable => {
                            return Self::build_fallback(
                                table_id,
                                table_ref,
                                fallback,
                                outer,
                                ec,
                                FallbackReason::Snapshot,
                            );
                        }
                        // More matches than the index path is estimated to
                        // be worth. If the table size is unknown or stale,
                        // count it once (cached) and decide again; otherwise
                        // the scan is cheaper.
                        IndexProbe::Truncated => {
                            if !counted && cost::estimate_is_unreliable(est) {
                                ec.table_store.count_rows(table_id)?;
                                counted = true;
                                continue;
                            }
                            return Self::build_fallback(
                                table_id,
                                table_ref,
                                fallback,
                                outer,
                                ec,
                                FallbackReason::Cost,
                            );
                        }
                    }
                };
                let n_entries = entries.len();
                let rows = ec.index_builder.fetch_index_rows_as_of(entries, as_of)?;
                if !counted && n_entries >= INDEX_COST_SAMPLE_ROWS {
                    stats.observe_index_cost(n_entries as u64, started.elapsed().as_nanos() as u64);
                }
                ec.metrics.record_index_rows_examined(rows.len() as u64);
                ec.metrics.record_table_fetch();
                Ok(AccessOp {
                    table_ref,
                    residual: residual.clone(),
                    source: AccessSource::Vec(rows.into_iter()),
                    outer: outer.clone(),
                    obs: None,
                })
            }
            PhysicalAccess::SeqScan {
                table_ref,
                predicate,
                ..
            } => {
                let table_ref = *table_ref;
                ec.metrics.record_seq_scan();
                let as_of = ec.txn.snapshot_seq();
                let iter = ec
                    .table_store
                    .scan_table_rows_as_of(access.table_id(), as_of)?;
                Ok(AccessOp {
                    table_ref,
                    residual: predicate.clone(),
                    source: AccessSource::Lazy(Box::new(iter)),
                    outer: outer.clone(),
                    obs: Some(ScanObs::new(access.table_id(), true)),
                })
            }
            PhysicalAccess::PkRangeScan {
                table_ref,
                start,
                end,
                residual,
                ..
            } => {
                let table_ref = *table_ref;
                let as_of = ec.txn.snapshot_seq();
                match (
                    resolve_pk_bound(start, outer, ec)?,
                    resolve_pk_bound(end, outer, ec)?,
                ) {
                    (Some(start), Some(end)) => {
                        ec.metrics.record_pk_range_scan();
                        let iter = ec.table_store.scan_table_pk_range_rows_as_of(
                            access.table_id(),
                            start,
                            end,
                            as_of,
                        )?;
                        ec.metrics.record_table_fetch();
                        Ok(AccessOp {
                            table_ref,
                            residual: residual.clone(),
                            source: AccessSource::Lazy(Box::new(iter)),
                            outer: outer.clone(),
                            obs: Some(ScanObs::new(access.table_id(), false)),
                        })
                    }
                    // A NULL bound endpoint can never match (item 62,
                    // same rule `PkLookup`/`IndexScan` already apply).
                    _ => Ok(AccessOp {
                        table_ref,
                        residual: residual.clone(),
                        source: AccessSource::Vec(Vec::new().into_iter()),
                        outer: outer.clone(),
                        obs: None,
                    }),
                }
            }
        }
    }

    /// The table-scan an `IndexScan` falls back to (see `IndexFallback`):
    /// the table's whole predicate over a snapshot-consistent scan, with
    /// the index's ordering re-established when an eliminated `Sort` relied
    /// on it. Result-equivalent to the index path by construction -- it is
    /// the same predicate over the same snapshot of the table, only without
    /// the index.
    fn build_fallback(
        table_id: u32,
        table_ref: u32,
        fallback: &IndexFallback,
        outer: &RowContext,
        ec: &ExecCtx<'a>,
        reason: FallbackReason,
    ) -> Result<Self> {
        ec.metrics.record_seq_scan();
        match reason {
            FallbackReason::Snapshot => ec.metrics.record_index_snapshot_fallback(),
            FallbackReason::Cost => ec.metrics.record_index_cost_fallback(),
        }
        let as_of = ec.txn.snapshot_seq();
        let iter = ec.table_store.scan_table_rows_as_of(table_id, as_of)?;
        if fallback.order_ordinals.is_empty() {
            return Ok(AccessOp {
                table_ref,
                residual: fallback.predicate.clone(),
                source: AccessSource::Lazy(Box::new(iter)),
                outer: outer.clone(),
                obs: Some(ScanObs::new(table_id, true)),
            });
        }
        // Ordered: collect the matching rows, then sort by the index's own
        // column order (ascending, NULLS FIRST -- the only order an index
        // scan can deliver). The scan yields primary-key order and the sort
        // is stable, so ties come out in primary-key order, exactly the
        // physical index order. Bounded like every other materializing
        // operator.
        let mut rows: Vec<(Vec<RelationalValue>, Row)> = Vec::new();
        let mut scanned: u64 = 0;
        for item in iter {
            ec.check()?;
            let (pk, row) = item?;
            scanned += 1;
            ec.metrics.record_rows_scanned(1);
            let keep = match &fallback.predicate {
                None => true,
                Some(p) => {
                    let ctx = outer.merged(&RowContext::single(table_ref, row.clone()));
                    eval_predicate(p, &ctx, ec)?.is_true()
                }
            };
            if !keep {
                ec.metrics.record_rows_filtered(1);
                continue;
            }
            if rows.len() >= ec.limits.max_materialized_rows {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "ordered index-scan fallback exceeds max_materialized_rows ({})",
                        ec.limits.max_materialized_rows
                    ),
                });
            }
            rows.push((pk, row));
        }
        ec.table_store
            .runtime_stats()
            .observe_row_count(table_id, scanned);
        let ords = &fallback.order_ordinals;
        rows.sort_by(|(_, a), (_, b)| compare_rows_by_ordinals(ords, a, b));
        Ok(AccessOp {
            table_ref,
            residual: None,
            source: AccessSource::Vec(rows.into_iter()),
            outer: outer.clone(),
            obs: None,
        })
    }

    fn fetch_next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Row>> {
        match &mut self.source {
            AccessSource::Single(row) => Ok(row.take()),
            AccessSource::Vec(iter) => Ok(iter.next().map(|(_, row)| row)),
            AccessSource::Lazy(iter) => {
                let next = iter.next().transpose()?;
                if next.is_some() {
                    ec.metrics.record_rows_scanned(1);
                }
                Ok(next.map(|(_, row)| row))
            }
        }
    }
}

impl<'a> Operator<'a> for AccessOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        loop {
            ec.check()?;
            let sample = self.obs.as_mut().and_then(|o| o.start());
            let Some(row) = self.fetch_next(ec)? else {
                if let Some(o) = &self.obs {
                    o.finish(ec);
                }
                return Ok(None);
            };
            if let Some(o) = self.obs.as_mut() {
                o.rows += 1;
            }
            let row_ctx = self.outer.merged(&RowContext::single(self.table_ref, row));
            let keep = match &self.residual {
                None => true,
                Some(predicate) => eval_predicate(predicate, &row_ctx, ec)?.is_true(),
            };
            if let Some(o) = self.obs.as_mut() {
                o.stop(sample);
            }
            if keep {
                return Ok(Some(Tuple {
                    ctx: row_ctx,
                    projected: Vec::new(),
                }));
            }
            ec.metrics.record_rows_filtered(1);
        }
    }
}

/// Rows behind an index-path cost observation (below it fixed per-scan
/// overheads would distort the per-row figure).
const INDEX_COST_SAMPLE_ROWS: usize = 256;

/// Every `SCAN_SAMPLE_EVERY`-th scanned row of a lazy scan is timed (fetch,
/// decode, and residual-predicate evaluation -- the whole per-row cost of
/// the scan path), so the model learns the real per-row cost of *this*
/// machine and data at a cost of two clock reads per 16 rows.
const SCAN_SAMPLE_EVERY: u64 = 16;

/// Observes one lazy table scan for the cost model: its sampled per-row
/// cost, and -- if it visited the whole table and ran to exhaustion -- the
/// table's exact row count. Purely statistical; never affects results.
struct ScanObs {
    table_id: u32,
    /// `true` for a scan of the entire table (a `SeqScan` or the cost/
    /// snapshot fallback); `false` for a bounded range, which yields cost
    /// samples but no table size.
    whole_table: bool,
    rows: u64,
    attempts: u64,
    sampled_rows: u64,
    sampled_ns: u64,
}

impl ScanObs {
    fn new(table_id: u32, whole_table: bool) -> Self {
        ScanObs {
            table_id,
            whole_table,
            rows: 0,
            attempts: 0,
            sampled_rows: 0,
            sampled_ns: 0,
        }
    }

    #[inline]
    fn start(&mut self) -> Option<std::time::Instant> {
        self.attempts += 1;
        if self.attempts % SCAN_SAMPLE_EVERY == 1 {
            Some(std::time::Instant::now())
        } else {
            None
        }
    }

    #[inline]
    fn stop(&mut self, started: Option<std::time::Instant>) {
        if let Some(t) = started {
            self.sampled_ns += t.elapsed().as_nanos() as u64;
            self.sampled_rows += 1;
        }
    }

    /// Called when the scan is exhausted.
    fn finish(&self, ec: &ExecCtx) {
        let stats = ec.table_store.runtime_stats();
        if self.whole_table {
            stats.observe_row_count(self.table_id, self.rows);
        }
        stats.observe_seq_cost(self.sampled_rows, self.sampled_ns);
    }
}

/// Why an `IndexScan` ran as a table scan instead.
enum FallbackReason {
    /// F-2: the index was not `Ready` as of the transaction's snapshot.
    Snapshot,
    /// The cost model (or a forced mode) chose the table scan.
    Cost,
}

/// Ascending, `NULLS FIRST` comparison of two rows over `ordinals` -- the
/// same comparator `SortOp` applies to an `ORDER BY` item with those
/// defaults, so a fallback's order is exactly what the eliminated `Sort`
/// would have produced.
fn compare_rows_by_ordinals(ordinals: &[u16], a: &Row, b: &Row) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for &ord in ordinals {
        let av = a.get(ord as usize).and_then(|v| v.as_ref());
        let bv = b.get(ord as usize).and_then(|v| v.as_ref());
        let o = match (av, bv) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => value_ordering(x, y),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Evaluates every expression in `exprs` against `outer`; `None` overall
/// (short-circuiting the caller to zero rows, never a wrong lookup) the
/// instant any single one evaluates to `NULL` — item 62's own "a `NULL`
/// component of an equality/range key can never match" rule, applied
/// uniformly to `PkLookup` keys and `IndexScan` bounds alike, rather
/// than passing a `NULL` into `IndexBuilder::index_lookup_as_of`, which
/// would incorrectly search for physically-`NULL`-indexed rows (`IS
/// NULL` semantics) instead of correctly matching nothing (`=`
/// semantics).
fn resolve_values(
    exprs: &[BoundExpr],
    outer: &RowContext,
    ec: &ExecCtx,
) -> Result<Option<Vec<RelationalValue>>> {
    let mut out = Vec::with_capacity(exprs.len());
    for e in exprs {
        match eval(e, outer, ec)? {
            Some(v) => out.push(v),
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

fn resolve_bound(
    bound: &Bound<Vec<BoundExpr>>,
    outer: &RowContext,
    ec: &ExecCtx,
) -> Result<Option<Bound<Vec<Option<RelationalValue>>>>> {
    Ok(match bound {
        Bound::Unbounded => Some(Bound::Unbounded),
        Bound::Included(exprs) => resolve_values(exprs, outer, ec)?
            .map(|v| Bound::Included(v.into_iter().map(Some).collect())),
        Bound::Excluded(exprs) => resolve_values(exprs, outer, ec)?
            .map(|v| Bound::Excluded(v.into_iter().map(Some).collect())),
    })
}

/// `PkRangeScan`'s own bound resolver -- unlike `resolve_bound` above,
/// primary-key values are never `Option`-wrapped (D5: PK columns are
/// never `NULL`), so this produces a plain `Bound<Vec<RelationalValue>>`
/// directly, matching `TableStore::scan_table_pk_range_rows_as_of`'s
/// (and `relational::key::encode_composite_key`'s) own signature.
fn resolve_pk_bound(
    bound: &Bound<Vec<BoundExpr>>,
    outer: &RowContext,
    ec: &ExecCtx,
) -> Result<Option<Bound<Vec<RelationalValue>>>> {
    Ok(match bound {
        Bound::Unbounded => Some(Bound::Unbounded),
        Bound::Included(exprs) => resolve_values(exprs, outer, ec)?.map(Bound::Included),
        Bound::Excluded(exprs) => resolve_values(exprs, outer, ec)?.map(Bound::Excluded),
    })
}

// =======================================================================
// Filter — item 16: three-valued `WHERE` semantics.
// =======================================================================

struct FilterOp<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    predicate: BoundExpr,
}

impl<'a> Operator<'a> for FilterOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        loop {
            ec.check()?;
            let Some(tuple) = self.input.next(ec)? else {
                return Ok(None);
            };
            if eval_predicate(&self.predicate, &tuple.ctx, ec)?.is_true() {
                return Ok(Some(tuple));
            }
            ec.metrics.record_rows_filtered(1);
        }
    }
}

// =======================================================================
// Projection — item 19/20: exact `SELECT`-list order, already-resolved
// wildcard expansion (the binder's own job, never repeated here).
// =======================================================================

struct ProjectionOp<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    items: Vec<BoundSelectItem>,
}

impl<'a> Operator<'a> for ProjectionOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        let Some(tuple) = self.input.next(ec)? else {
            return Ok(None);
        };
        let projected = self
            .items
            .iter()
            .map(|item| eval(&item.expr, &tuple.ctx, ec))
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(Tuple {
            ctx: tuple.ctx,
            projected,
        }))
    }
}

// =======================================================================
// Distinct — item 21/22: `RelationalValue`'s own `PartialEq` contract
// (via `Option<RelationalValue>`'s derived equality — `NULL`s compare
// equal to each other for grouping purposes here, the standard SQL
// `DISTINCT`/`GROUP BY` rule, deliberately different from `WHERE`'s
// three-valued `NULL = NULL -> UNKNOWN`), bounded by `ExecLimits::
// max_materialized_rows` (never an unbounded seen-set, item 22).
// =======================================================================

struct DistinctOp<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    seen: Vec<Vec<Option<RelationalValue>>>,
}

impl<'a> Operator<'a> for DistinctOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        loop {
            ec.check()?;
            let Some(tuple) = self.input.next(ec)? else {
                return Ok(None);
            };
            if self.seen.contains(&tuple.projected) {
                continue;
            }
            if self.seen.len() >= ec.limits.max_materialized_rows {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "DISTINCT examined more than max_materialized_rows ({}) distinct values",
                        ec.limits.max_materialized_rows
                    ),
                });
            }
            self.seen.push(tuple.projected.clone());
            ec.metrics.record_distinct_rows(1);
            return Ok(Some(tuple));
        }
    }
}

// =======================================================================
// Aggregate — Increment 11 (`PHASE_RELATIONAL_AGGREGATION_ARCHITECTURE.
// md`): hash aggregation over `crate::aggregate::GroupingKey`'s own
// canonical, collision-free grouping representation (never a lossy
// debug-string key). A blocking operator, exactly like `Sort`/`Distinct`
// above (the same reason: correctness requires seeing every input row
// before any group's aggregate state is final) — never an unbounded
// materialization of raw input *rows*, though: only one `RowContext`
// (the group's first-seen representative row, item 12's own
// "streaming... discard the input row once it has contributed to
// aggregate state") plus a small, fixed-shape `Vec<AggregateState>` is
// retained per distinct group, bounded by `ExecLimits::max_group_count`/
// `max_aggregate_state_bytes` (item 30/73), checked *before* inserting a
// new group or growing a state past the byte bound — never after.
// =======================================================================

struct AggregateOp<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    group_by: Vec<BoundExpr>,
    aggregates: Vec<BoundAggregateExpr>,
    output: Option<std::vec::IntoIter<Tuple>>,
}

impl<'a> AggregateOp<'a> {
    fn build(
        input: Box<dyn Operator<'a> + 'a>,
        group_by: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregateExpr>,
    ) -> Self {
        AggregateOp {
            input,
            group_by,
            aggregates,
            output: None,
        }
    }

    fn initial_states(&self) -> Result<Vec<AggregateState>> {
        self.aggregates
            .iter()
            .map(|agg| {
                let is_wildcard = matches!(agg.arg, AggregateArg::Wildcard);
                let input_ty = match &agg.arg {
                    AggregateArg::Wildcard => None,
                    AggregateArg::Expr(e) => e.ty,
                };
                AggregateState::initial(agg.func, is_wildcard, input_ty)
            })
            .collect()
    }

    fn materialize(&mut self, ec: &ExecCtx<'a>) -> Result<()> {
        use std::collections::HashMap;

        // item 61: emission order must be a deterministic function of
        // input scan order, never raw `HashMap` iteration order — the
        // map here only ever answers "have I seen this key," an index
        // into the insertion-ordered `groups` vector actually iterated
        // below.
        let mut index_of: HashMap<GroupingKey, usize> = HashMap::new();
        let mut groups: Vec<(RowContext, Vec<AggregateState>)> = Vec::new();
        let mut estimated_bytes: usize = 0usize;
        let mut rows_processed: u64 = 0;

        while let Some(tuple) = self.input.next(ec)? {
            ec.check()?;
            rows_processed += 1;

            let key_values: Vec<Option<RelationalValue>> = self
                .group_by
                .iter()
                .map(|g| eval(g, &tuple.ctx, ec))
                .collect::<Result<_>>()?;
            let key = GroupingKey::from_values(&key_values);

            let idx = match index_of.get(&key) {
                Some(&i) => i,
                None => {
                    // item 30/74: checked *before* the new group is
                    // created, never after — a query that would exceed
                    // the bound fails closed on the row that discovers
                    // it, with every prior group's state simply dropped
                    // (item 66: never a partial aggregate result).
                    if groups.len() >= ec.limits.max_group_count {
                        ec.metrics.record_aggregate_resource_limit_hit();
                        return Err(SqlError::ResourceLimit {
                            detail: format!(
                                "GROUP BY produced more than max_group_count ({}) groups",
                                ec.limits.max_group_count
                            ),
                        });
                    }
                    estimated_bytes = estimated_bytes.saturating_add(key.estimated_bytes());
                    groups.push((tuple.ctx.clone(), self.initial_states()?));
                    let new_idx = groups.len() - 1;
                    index_of.insert(key, new_idx);
                    ec.metrics.record_group_created();
                    new_idx
                }
            };

            let (_, states) = &mut groups[idx];
            for (agg, state) in self.aggregates.iter().zip(states.iter_mut()) {
                let val = match &agg.arg {
                    AggregateArg::Wildcard => None,
                    AggregateArg::Expr(e) => eval(e, &tuple.ctx, ec)?,
                };
                let before = state.estimated_bytes();
                state.update(val.as_ref())?;
                let after = state.estimated_bytes();
                estimated_bytes = estimated_bytes.saturating_sub(before).saturating_add(after);
            }
            if estimated_bytes > ec.limits.max_aggregate_state_bytes {
                ec.metrics.record_aggregate_resource_limit_hit();
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "aggregate state exceeded max_aggregate_state_bytes ({})",
                        ec.limits.max_aggregate_state_bytes
                    ),
                });
            }
        }

        // item 11/44: an aggregate query with no `GROUP BY` clause
        // always produces exactly one result row, even over zero
        // qualifying input rows (every aggregate's own documented
        // empty-input contract — `COUNT` is `0`, the rest are `NULL`).
        if self.group_by.is_empty() && groups.is_empty() {
            groups.push((RowContext::new(), self.initial_states()?));
        }

        ec.metrics.record_aggregate_rows_processed(rows_processed);
        ec.metrics.record_groups_emitted(groups.len() as u64);

        let out: Vec<Tuple> = groups
            .into_iter()
            .map(|(ctx, states)| {
                let values: Vec<Option<RelationalValue>> =
                    states.into_iter().map(AggregateState::finalize).collect();
                Tuple {
                    ctx: ctx.with_aggregates(values),
                    projected: Vec::new(),
                }
            })
            .collect();
        self.output = Some(out.into_iter());
        Ok(())
    }
}

impl<'a> Operator<'a> for AggregateOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        if self.output.is_none() {
            self.materialize(ec)?;
        }
        Ok(self.output.as_mut().expect("materialized above").next())
    }
}

// =======================================================================
// Sort — item 23/24: ascending/descending, NULLS FIRST/LAST per the
// bound `ORDER BY` representation; bounded materialization (never an
// unbounded `Vec`, item 24 — no external sort/spill exists, matching
// D17's own "bounded, reject when exceeded" v1 decision).
// =======================================================================

struct SortOp<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    items: Vec<BoundOrderByItem>,
    buffer: Option<std::vec::IntoIter<Tuple>>,
}

impl<'a> SortOp<'a> {
    fn build(input: Box<dyn Operator<'a> + 'a>, items: Vec<BoundOrderByItem>) -> Self {
        SortOp {
            input,
            items,
            buffer: None,
        }
    }

    fn materialize(&mut self, ec: &ExecCtx<'a>) -> Result<()> {
        let mut rows = Vec::new();
        while let Some(tuple) = self.input.next(ec)? {
            ec.check()?;
            if rows.len() >= ec.limits.max_materialized_rows {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "ORDER BY input exceeds max_materialized_rows ({})",
                        ec.limits.max_materialized_rows
                    ),
                });
            }
            rows.push(tuple);
        }
        ec.metrics.record_rows_sorted(rows.len() as u64);

        // Evaluate every sort key once per row up front (never re-
        // evaluated per comparison) -- correctness-neutral, but avoids
        // silently quadratic-in-expression-cost behavior on a large sort.
        let mut keyed: Vec<(Vec<Option<RelationalValue>>, Tuple)> = Vec::with_capacity(rows.len());
        for tuple in rows {
            let key = self
                .items
                .iter()
                .map(|item| eval(&item.expr, &tuple.ctx, ec))
                .collect::<Result<Vec<_>>>()?;
            keyed.push((key, tuple));
        }

        let items = &self.items;
        keyed.sort_by(|(a, _), (b, _)| compare_sort_keys(items, a, b));

        self.buffer = Some(
            keyed
                .into_iter()
                .map(|(_, t)| t)
                .collect::<Vec<_>>()
                .into_iter(),
        );
        Ok(())
    }
}

fn compare_sort_keys(
    items: &[BoundOrderByItem],
    a: &[Option<RelationalValue>],
    b: &[Option<RelationalValue>],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (item, (av, bv)) in items.iter().zip(a.iter().zip(b.iter())) {
        // `NULLS FIRST`/`LAST` is an absolute placement, independent of
        // `ASC`/`DESC` (item 23) -- only a `Some`/`Some` value pair's
        // own relative order flips with `descending`; a `None` involved
        // on either side must never be reversed a second time, or
        // `DESC NULLS LAST` would wrongly become `NULLS FIRST`.
        let ord = match (av, bv) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => {
                if item.nulls == NullsOrder::First {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (Some(_), None) => {
                if item.nulls == NullsOrder::First {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (Some(x), Some(y)) => {
                let vo = value_ordering(x, y);
                if item.descending {
                    vo.reverse()
                } else {
                    vo
                }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// A total order even across `f32`/`f64` (`partial_cmp` falls back to
/// `Equal` only for a `NaN` pair — sort stability, never a panic; a
/// `NaN` cannot arise from any bound comparison this increment executes
/// anyway, since `value_cmp` in `expr_eval` already rejects it before a
/// row could be produced with one as a sort key's source column... this
/// helper exists only because `Sort` evaluates keys independently of
/// `WHERE`, so it takes the same defensive stance rather than assuming).
fn value_ordering(a: &RelationalValue, b: &RelationalValue) -> std::cmp::Ordering {
    use RelationalValue::*;
    match (a, b) {
        (Boolean(x), Boolean(y)) => x.cmp(y),
        (Integer(x), Integer(y)) => x.cmp(y),
        (Bigint(x), Bigint(y)) => x.cmp(y),
        (Real(x), Real(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Double(x), Double(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Decimal(x, _), Decimal(y, _)) => x.cmp(y),
        (Text(x), Text(y)) => x.cmp(y),
        (Blob(x), Blob(y)) => x.cmp(y),
        (Date(x), Date(y)) => x.cmp(y),
        (Time(x), Time(y)) => x.cmp(y),
        (Timestamp(x), Timestamp(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    }
}

impl<'a> Operator<'a> for SortOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        if self.buffer.is_none() {
            self.materialize(ec)?;
        }
        Ok(self.buffer.as_mut().expect("materialized above").next())
    }
}

// =======================================================================
// Limit / Offset — item 25/26: early termination, lazy offset-skip.
// =======================================================================

struct LimitOp<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    limit: Option<u64>,
    offset: u64,
    skipped: u64,
    produced: u64,
}

impl<'a> LimitOp<'a> {
    fn build(
        input: Box<dyn Operator<'a> + 'a>,
        limit: Option<BoundExpr>,
        offset: Option<BoundExpr>,
        outer: &RowContext,
        ec: &ExecCtx<'a>,
    ) -> Result<Self> {
        let limit = limit
            .as_ref()
            .map(|e| resolve_count(e, outer, ec))
            .transpose()?;
        let offset = offset
            .as_ref()
            .map(|e| resolve_count(e, outer, ec))
            .transpose()?
            .unwrap_or(0);
        Ok(LimitOp {
            input,
            limit,
            offset,
            skipped: 0,
            produced: 0,
        })
    }
}

fn resolve_count(expr: &BoundExpr, outer: &RowContext, ec: &ExecCtx) -> Result<u64> {
    match eval(expr, outer, ec)? {
        Some(RelationalValue::Integer(n)) if n >= 0 => Ok(n as u64),
        Some(RelationalValue::Bigint(n)) if n >= 0 => Ok(n as u64),
        _ => Err(SqlError::ExecutionParameter {
            detail: "LIMIT/OFFSET must evaluate to a non-negative integer".to_string(),
        }),
    }
}

impl<'a> Operator<'a> for LimitOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        if let Some(limit) = self.limit {
            if self.produced >= limit {
                // Item 25: never request another upstream row once the
                // limit is satisfied.
                return Ok(None);
            }
        }
        loop {
            ec.check()?;
            let Some(tuple) = self.input.next(ec)? else {
                return Ok(None);
            };
            if self.skipped < self.offset {
                self.skipped += 1;
                continue;
            }
            self.produced += 1;
            return Ok(Some(tuple));
        }
    }
}

// =======================================================================
// Join — item 28/29/31/32/33: `INNER`/`LEFT`, `NestedLoop`/
// `IndexNestedLoop`, the full `ON` condition always evaluated in full.
// =======================================================================

struct NestedLoopJoinOp<'a> {
    left: Box<dyn Operator<'a> + 'a>,
    right_plan: PhysicalPlan,
    right_table_refs: Vec<u32>,
    kind: JoinKind,
    on: BoundExpr,
    #[allow(dead_code)]
    algorithm: JoinAlgorithm,
    current_outer: Option<Tuple>,
    current_inner: Option<Box<dyn Operator<'a> + 'a>>,
    outer_had_match: bool,
}

impl<'a> NestedLoopJoinOp<'a> {
    #[allow(clippy::too_many_arguments)]
    fn build(
        left: &PhysicalPlan,
        right: PhysicalPlan,
        kind: JoinKind,
        on: BoundExpr,
        algorithm: JoinAlgorithm,
        outer: &RowContext,
        ec: &ExecCtx<'a>,
    ) -> Result<NestedLoopJoinOp<'a>> {
        ec.metrics.record_join();
        let right_table_refs = right_side_table_refs(&right);
        Ok(NestedLoopJoinOp {
            left: build_operator(left, outer, ec)?,
            right_plan: right,
            right_table_refs,
            kind,
            on,
            algorithm,
            current_outer: None,
            current_inner: None,
            outer_had_match: false,
        })
    }
}

impl<'a> Operator<'a> for NestedLoopJoinOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        loop {
            ec.check()?;
            if self.current_outer.is_none() {
                let Some(outer_tuple) = self.left.next(ec)? else {
                    return Ok(None);
                };
                // Item 31/33: a fresh inner operator per outer row --
                // never reused across outer rows, never a materialized
                // inner table. For a correlated (`IndexNestedLoop`)
                // right side, this is what actually re-evaluates the
                // key against *this* outer row (item 31's own "do NOT
                // cache one outer row's lookup result for unrelated
                // outer rows"); for a plain `NestedLoop` right side, it
                // is what makes the algorithm a real O(|outer| x
                // |inner|) nested loop rather than a hidden hash join.
                self.current_inner = Some(build_operator(&self.right_plan, &outer_tuple.ctx, ec)?);
                self.outer_had_match = false;
                self.current_outer = Some(outer_tuple);
            }

            let inner_op = self.current_inner.as_mut().expect("set above");
            let inner_next = inner_op.next(ec)?;

            match inner_next {
                Some(inner_tuple) => {
                    let outer_tuple = self.current_outer.as_ref().expect("set above");
                    let merged = outer_tuple.ctx.merged(&inner_tuple.ctx);
                    // Item 28: the full ON condition, always -- never
                    // the access path's own already-consumed key alone
                    // (that only narrowed candidates, item 10).
                    if eval_predicate(&self.on, &merged, ec)?.is_true() {
                        self.outer_had_match = true;
                        return Ok(Some(Tuple {
                            ctx: merged,
                            projected: Vec::new(),
                        }));
                    }
                    // no match on this inner row -- keep pulling
                }
                None => {
                    // Inner side exhausted for this outer row.
                    let outer_tuple = self.current_outer.take().expect("set above");
                    self.current_inner = None;
                    let emit_null_extended = self.kind == JoinKind::Left && !self.outer_had_match;
                    if emit_null_extended {
                        let mut merged = outer_tuple.ctx;
                        for &tr in &self.right_table_refs {
                            merged = merged.with_null_extension(tr);
                        }
                        return Ok(Some(Tuple {
                            ctx: merged,
                            projected: Vec::new(),
                        }));
                    }
                    // INNER JOIN with no match, or LEFT JOIN that already
                    // emitted its null-extended row -- move to the next
                    // outer row.
                }
            }
        }
    }
}

/// Every `table_ref` the right-hand subtree could ever introduce — item
/// 29/30: a `LEFT JOIN`'s null-extension must cover every table on the
/// nullable side, not just a bare scan's own single `table_ref` (the
/// right side can itself be a nested join).
fn right_side_table_refs(plan: &PhysicalPlan) -> Vec<u32> {
    let mut out = Vec::new();
    fn walk(plan: &PhysicalPlan, out: &mut Vec<u32>) {
        match plan {
            PhysicalPlan::EmptyRelation => {}
            PhysicalPlan::Access(access) => out.push(access_table_ref(access)),
            PhysicalPlan::Join { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Projection { input, .. }
            | PhysicalPlan::Distinct { input }
            | PhysicalPlan::Aggregate { input, .. }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::Limit { input, .. } => walk(input, out),
        }
    }
    walk(plan, &mut out);
    out
}

fn access_table_ref(access: &PhysicalAccess) -> u32 {
    match access {
        PhysicalAccess::PkLookup { table_ref, .. }
        | PhysicalAccess::IndexScan { table_ref, .. }
        | PhysicalAccess::SeqScan { table_ref, .. }
        | PhysicalAccess::PkRangeScan { table_ref, .. } => *table_ref,
    }
}
