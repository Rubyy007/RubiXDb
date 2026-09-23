//! Executable physical operators — item 9: one struct per `PhysicalPlan`
//! node kind, each with clear inputs/outputs, never one giant match
//! statement holding all execution logic. `Operator::next` is the one
//! pull-based interface every operator implements (item 6/44): a
//! consumer above only ever asks for the next row, so `LIMIT`,
//! cancellation, and bounded memory all fall out of the model itself
//! rather than needing special-casing per operator.

use std::ops::Bound;

use rubixdb::relational::{RelationalValue, Row};

use crate::ast::JoinKind;
use crate::bound::{BoundExpr, BoundOrderByItem, BoundSelectItem, NullsOrder};
use crate::error::{Result, SqlError};
use crate::exec::expr_eval::{eval, eval_predicate};
use crate::exec::{ExecCtx, RowContext, Tuple};
use crate::plan::access::{IndexAccessMode, PhysicalAccess};
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
                })
            }
            PhysicalAccess::IndexScan {
                table_ref,
                index_id,
                mode,
                residual,
                ..
            } => {
                let table_ref = *table_ref;
                let as_of = ec.txn.snapshot_seq();
                let rows = match mode {
                    IndexAccessMode::Equality { prefix } => {
                        match resolve_values(prefix, outer, ec)? {
                            None => Vec::new(),
                            Some(values) => {
                                ec.metrics.record_index_scan();
                                ec.index_builder.index_lookup_as_of(
                                    *index_id,
                                    &values.into_iter().map(Some).collect::<Vec<_>>(),
                                    as_of,
                                )?
                            }
                        }
                    }
                    IndexAccessMode::Range { start, end } => {
                        match (
                            resolve_bound(start, outer, ec)?,
                            resolve_bound(end, outer, ec)?,
                        ) {
                            (Some(start), Some(end)) => {
                                ec.metrics.record_index_scan();
                                ec.index_builder
                                    .index_range_scan_as_of(*index_id, start, end, as_of)?
                            }
                            _ => Vec::new(), // a NULL bound endpoint can never match (item 62)
                        }
                    }
                };
                if rows.len() > ec.limits.max_index_scan_rows {
                    return Err(SqlError::ResourceLimit {
                        detail: format!(
                            "index scan examined {} row(s); max_index_scan_rows is {}",
                            rows.len(),
                            ec.limits.max_index_scan_rows
                        ),
                    });
                }
                ec.metrics.record_index_rows_examined(rows.len() as u64);
                ec.metrics.record_table_fetch();
                Ok(AccessOp {
                    table_ref,
                    residual: residual.clone(),
                    source: AccessSource::Vec(rows.into_iter()),
                    outer: outer.clone(),
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
                })
            }
        }
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
            let Some(row) = self.fetch_next(ec)? else {
                return Ok(None);
            };
            let row_ctx = self.outer.merged(&RowContext::single(self.table_ref, row));
            let keep = match &self.residual {
                None => true,
                Some(predicate) => eval_predicate(predicate, &row_ctx, ec)?.is_true(),
            };
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
        | PhysicalAccess::SeqScan { table_ref, .. } => *table_ref,
    }
}
