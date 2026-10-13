//! Recursive CTEs at execution (SQL-mission-1 Slice D): the fixpoint of
//! `WITH RECURSIVE name AS (seed UNION [ALL] recursive-term)`.
//!
//! This is **not** the non-recursive CTE materialization (`exec/relation.rs`):
//! a recursive CTE owns a *working table* and its own result cache, and shares
//! with the other kind only name resolution at bind time.
//!
//! # The algorithm
//!
//! ```text
//! result  = seed rows            (the seed is run exactly once)
//! working = seed rows
//! while working is not empty:
//!     iterations += 1            (> max_recursive_cte_depth  -> RESOURCE_LIMIT)
//!     new     = recursive term evaluated with `name` = working
//!     UNION ALL: working = new        (every row produced is kept)
//!     UNION:     working = the rows of `new` not already in `result`
//!     result += working
//! ```
//!
//! * Iteration N sees iteration N-1's output and nothing else: `working` is
//!   replaced only after the term has finished.
//! * **Termination.** The loop ends when `working` is empty. `UNION ALL`: an
//!   iteration produced no rows. `UNION`: an iteration produced no row that is
//!   new against *everything* produced so far (the null-safe `SetKey` set), so
//!   a cyclic graph terminates by itself. Every other query is stopped by one
//!   of the two bounds, which are checked before the work they bound:
//!   `max_recursive_cte_depth` (recursive-term evaluations) and
//!   `max_recursive_cte_rows` (rows in the result, seed included), plus
//!   `max_materialized_bytes` for the bytes held. A bound that fires is a typed
//!   `RESOURCE_LIMIT` naming the CTE; nothing is truncated.
//! * The recursive term reads base tables through the statement's own
//!   `ExecCtx` (one pinned transaction snapshot), so a row committed while the
//!   recursion runs is never seen by a later iteration.
//! * The finished rows are cached per statement by CTE id, so a recursive CTE
//!   read twice (or from a later CTE) runs its fixpoint once.

use std::collections::HashSet;
use std::rc::Rc;

use crate::error::{Result, SqlError};
use crate::exec::operators::{build_operator, Operator};
use crate::exec::relation::RowsOp;
use crate::exec::setop::{row_bytes, Row, SetKey};
use crate::exec::{ExecCtx, RowContext};
use crate::plan::physical::RecursiveCteNode;

/// The rows of the recursive CTE `cte_id`, running its fixpoint the first time
/// they are asked for in this execution.
pub(super) fn recursive_rows(
    cte_id: u32,
    node: &RecursiveCteNode,
    ec: &ExecCtx<'_>,
) -> Result<Rc<Vec<Row>>> {
    if let Some(rows) = ec.recursive_cache.borrow().get(&cte_id) {
        return Ok(Rc::clone(rows));
    }
    let rows = Rc::new(run_fixpoint(node, ec)?);
    ec.recursive_cache
        .borrow_mut()
        .insert(cte_id, Rc::clone(&rows));
    Ok(rows)
}

/// Rows and bytes accepted into one fixpoint's result so far.
struct Held {
    rows: u64,
    bytes: usize,
}

fn run_fixpoint(node: &RecursiveCteNode, ec: &ExecCtx<'_>) -> Result<Vec<Row>> {
    let name = &node.name;
    let max_depth = u64::from(ec.limits.max_recursive_cte_depth);
    let max_rows = ec.limits.max_recursive_cte_rows;
    let max_bytes = ec.limits.max_materialized_bytes;

    let mut result: Vec<Row> = Vec::new();
    let mut seen: HashSet<SetKey> = HashSet::new();
    let mut held = Held { rows: 0, bytes: 0 };

    // Takes one produced row into the result; `false` if `UNION` already has it.
    let mut accept = |row: Row, held: &mut Held, result: &mut Vec<Row>| -> Result<bool> {
        if node.distinct {
            let key = SetKey(row);
            if seen.contains(&key) {
                return Ok(false);
            }
            check_room(name, held, &key.0, max_rows, max_bytes)?;
            result.push(key.0.clone());
            seen.insert(key);
        } else {
            check_room(name, held, &row, max_rows, max_bytes)?;
            result.push(row);
        }
        held.rows += 1;
        Ok(true)
    };

    // The seed: run once, here, and never again.
    let mut working: Vec<Row> = Vec::new();
    {
        let mut op = build_operator(&node.seed_plan, &RowContext::new(), ec)?;
        let before = result.len();
        while let Some(tuple) = op.next(ec)? {
            ec.check()?;
            if accept(tuple.projected, &mut held, &mut result)? {
                working.push(result[result.len() - 1].clone());
            }
        }
        ec.metrics
            .record_recursive_cte_rows_emitted((result.len() - before) as u64);
    }

    let mut iterations = 0u64;
    while !working.is_empty() {
        ec.check()?;
        if iterations >= max_depth {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "recursive CTE '{name}' did not reach its fixpoint within max_recursive_cte_depth ({max_depth}) iterations"
                ),
            });
        }
        iterations += 1;
        ec.metrics.record_recursive_cte_iteration();
        // The term reads exactly the previous iteration's rows.
        ec.working_tables
            .borrow_mut()
            .insert(node.work_id, Rc::new(std::mem::take(&mut working)));
        let before = result.len();
        let mut op = build_operator(&node.recursive_plan, &RowContext::new(), ec)?;
        let mut next: Vec<Row> = Vec::new();
        let outcome = (|| -> Result<()> {
            while let Some(tuple) = op.next(ec)? {
                ec.check()?;
                if accept(tuple.projected, &mut held, &mut result)? {
                    next.push(result[result.len() - 1].clone());
                }
            }
            Ok(())
        })();
        ec.metrics
            .record_recursive_cte_rows_emitted((result.len() - before) as u64);
        // On an error the statement ends and its context is dropped with the
        // working table in it: there is nothing to clean up.
        outcome?;
        working = next;
    }
    ec.working_tables.borrow_mut().remove(&node.work_id);
    Ok(result)
}

/// Fails closed if one more row would pass the row or byte bound.
fn check_room(
    name: &str,
    held: &mut Held,
    row: &[Option<rubixdb::relational::RelationalValue>],
    max_rows: u64,
    max_bytes: usize,
) -> Result<()> {
    if held.rows >= max_rows {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "recursive CTE '{name}' produced more than max_recursive_cte_rows ({max_rows}) rows"
            ),
        });
    }
    held.bytes = held.bytes.saturating_add(row_bytes(row));
    if held.bytes > max_bytes {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "recursive CTE '{name}' result exceeded max_materialized_bytes ({max_bytes})"
            ),
        });
    }
    Ok(())
}

/// The recursive reference inside a term: the previous iteration's rows.
pub(super) fn open_working<'a>(
    table_ref: u32,
    work_id: u32,
    name: &str,
    outer: &RowContext,
    ec: &ExecCtx<'a>,
) -> Result<Box<dyn Operator<'a> + 'a>> {
    let rows = ec.working_tables.borrow().get(&work_id).cloned();
    match rows {
        Some(rows) => Ok(Box::new(RowsOp::new(rows, table_ref, outer.clone()))),
        None => Err(SqlError::PlanValidation {
            detail: format!(
                "the working table of recursive CTE '{name}' was read outside its recursion"
            ),
        }),
    }
}
