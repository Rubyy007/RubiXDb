//! Derived tables and CTE references at execution (SQL-mission-1 Slice C).
//!
//! Both are leaves of a query's operator tree that yield the rows of a nested,
//! already-planned query as a relation: each output row becomes the
//! `RowContext` entry for the relation's `table_ref`, so the ordinary
//! expression evaluator reads its columns exactly like a table's.
//!
//! | node | when it runs | rows |
//! |---|---|---|
//! | derived table | streamed, as the plan above pulls | not stored |
//! | derived table on the right of a join (`rescanned`) | once per statement | materialized, re-read per left row |
//! | `LATERAL` derived table that reads the items before it | once per left row, through the correlated-subquery gate | not stored |
//! | CTE reference | its definition runs **once per statement**, at the first read | materialized; every reference reads the same rows |
//!
//! Materialization is bounded by `max_materialized_rows` and
//! `max_materialized_bytes` and fails closed with `RESOURCE_LIMIT`; it never
//! truncates and never falls back to re-running the definition.

use std::rc::Rc;

use rubixdb::relational::RelationalValue;

use crate::aggregate::GroupingKey;
use crate::error::{Result, SqlError};
use crate::exec::operators::{build_operator, Operator};
use crate::exec::{ExecCtx, RowContext, Tuple};
use crate::plan::physical::{CtePlan, PhysicalPlan};

type Row = Vec<Option<RelationalValue>>;

pub(super) fn open_derived<'a>(
    table_ref: u32,
    id: u32,
    plan: &PhysicalPlan,
    correlated: bool,
    rescanned: bool,
    outer: &RowContext,
    ec: &ExecCtx<'a>,
) -> Result<Box<dyn Operator<'a> + 'a>> {
    if correlated {
        // LATERAL: the body reads the row of the items before it, so it is
        // re-run for each such row and counted against the same limits (and
        // the same deadline check) as a correlated subquery.
        super::subquery::begin_correlated_relation(id, ec)?;
        return Ok(Box::new(StreamOp {
            inner: build_operator(plan, outer, ec)?,
            table_ref,
            outer: outer.clone(),
        }));
    }
    if rescanned {
        let rows = materialized_rows(id, plan, false, ec)?;
        return Ok(Box::new(RowsOp {
            rows,
            pos: 0,
            table_ref,
            outer: outer.clone(),
        }));
    }
    // An ordinary derived table can read no enclosing row: it runs against an
    // empty outer context.
    Ok(Box::new(StreamOp {
        inner: build_operator(plan, &RowContext::new(), ec)?,
        table_ref,
        outer: outer.clone(),
    }))
}

pub(super) fn open_cte<'a>(
    table_ref: u32,
    cte: &CtePlan,
    outer: &RowContext,
    ec: &ExecCtx<'a>,
) -> Result<Box<dyn Operator<'a> + 'a>> {
    // A recursive CTE owns its working table and its own result cache; it does
    // not go through the materialization below.
    let rows = match &cte.plan {
        PhysicalPlan::RecursiveCte(node) => super::recursive::recursive_rows(cte.id, node, ec)?,
        plan => materialized_rows(cte.id, plan, true, ec)?,
    };
    Ok(Box::new(RowsOp::new(rows, table_ref, outer.clone())))
}

/// The rows of relation `id`, running `plan` to produce them the first time
/// they are asked for in this execution and serving the stored copy after.
fn materialized_rows(
    id: u32,
    plan: &PhysicalPlan,
    is_cte: bool,
    ec: &ExecCtx<'_>,
) -> Result<Rc<Vec<Row>>> {
    if let Some(rows) = ec.relation_cache.borrow().get(&id) {
        return Ok(Rc::clone(rows));
    }
    let what = if is_cte { "a CTE" } else { "a derived table" };
    let mut op = build_operator(plan, &RowContext::new(), ec)?;
    let mut rows: Vec<Row> = Vec::new();
    let mut bytes = 0usize;
    while let Some(tuple) = op.next(ec)? {
        ec.check()?;
        if rows.len() >= ec.limits.max_materialized_rows {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "{what} produced more than max_materialized_rows ({}) rows",
                    ec.limits.max_materialized_rows
                ),
            });
        }
        bytes = bytes.saturating_add(GroupingKey::from_values(&tuple.projected).estimated_bytes());
        if bytes > ec.limits.max_materialized_bytes {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "{what} result exceeded max_materialized_bytes ({})",
                    ec.limits.max_materialized_bytes
                ),
            });
        }
        rows.push(tuple.projected);
    }
    if is_cte {
        ec.metrics.record_cte_materialization();
    }
    let rows = Rc::new(rows);
    ec.relation_cache.borrow_mut().insert(id, Rc::clone(&rows));
    Ok(rows)
}

/// Yields the nested query's rows as they are produced.
struct StreamOp<'a> {
    inner: Box<dyn Operator<'a> + 'a>,
    table_ref: u32,
    outer: RowContext,
}

impl<'a> Operator<'a> for StreamOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        let Some(tuple) = self.inner.next(ec)? else {
            return Ok(None);
        };
        Ok(Some(Tuple {
            ctx: self.outer.with_row(self.table_ref, tuple.projected),
            projected: Vec::new(),
        }))
    }
}

/// Yields a stored row set, front to back.
pub(super) struct RowsOp {
    rows: Rc<Vec<Row>>,
    pos: usize,
    table_ref: u32,
    outer: RowContext,
}

impl RowsOp {
    pub(super) fn new(rows: Rc<Vec<Row>>, table_ref: u32, outer: RowContext) -> Self {
        RowsOp {
            rows,
            pos: 0,
            table_ref,
            outer,
        }
    }
}

impl<'a> Operator<'a> for RowsOp {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        ec.check()?;
        let Some(row) = self.rows.get(self.pos) else {
            return Ok(None);
        };
        self.pos += 1;
        Ok(Some(Tuple {
            ctx: self.outer.with_row(self.table_ref, row.clone()),
            projected: Vec::new(),
        }))
    }
}
