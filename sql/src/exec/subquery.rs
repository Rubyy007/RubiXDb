//! Subquery expression evaluation (SQL-mission-1): the executor half of
//! `BoundExprKind::Subquery`.
//!
//! One mechanism serves the scalar, `EXISTS` and `IN` forms: the planner
//! attached a `PhysicalPlan` to the expression node, and evaluation runs it
//! as a nested operator tree through the ordinary `build_operator` /
//! `Operator::next` path -- the same operators, limits, cancellation and
//! deadline checks as the enclosing query. What differs per form is only how
//! many inner rows are pulled and what is done with them:
//!
//! | form | rows pulled | result |
//! |---|---|---|
//! | scalar | at most 2 | 0 rows -> `NULL`; 1 row -> its value; 2nd row -> runtime error |
//! | `EXISTS` | at most 1 | `TRUE`/`FALSE`, never `NULL` |
//! | `IN` | all (bounded) | SQL three-valued logic (see `in_tri`) |
//!
//! An *uncorrelated* subquery (no outer references) is evaluated once per
//! execution and cached by id in `ExecCtx::subquery_cache`; correlated
//! subqueries are never cached (they re-execute per outer row, with the
//! outer row's context handed to the inner operator tree).
//!
//! **Correlated subqueries (Slice B)** use that same outer-context handover,
//! nothing more: `build_operator(plan, ctx, ec)` threads the enclosing row
//! down, `AccessOp` emits `outer.with_row(..)` so every inner tuple carries
//! it, and an inner expression that names an outer column finds it in the
//! tuple's `RowContext` exactly as it finds its own. There is no hoisting,
//! no decorrelation and no result reuse across outer rows: the inner query
//! runs once per evaluation, and each run first checks cancellation and the
//! deadline (so a slow inner plan cannot hide behind the outer loop's own
//! checks) and then counts against `max_correlated_reexecutions` /
//! `max_subquery_outer_rows`.

use std::collections::HashSet;
use std::rc::Rc;

use rubixdb::relational::RelationalValue;

use crate::aggregate::GroupingKey;
use crate::bound::{BoundSubqueryExpr, SubqueryKind};
use crate::error::{Result, SqlError};
use crate::exec::expr_eval::{tri_not, tri_to_bool, Tri};
use crate::exec::operators::{build_operator, Operator};
use crate::exec::{ExecCtx, RowContext};

/// A cached uncorrelated subquery result.
pub(super) enum CachedSubquery {
    Scalar(Option<RelationalValue>),
    Exists(bool),
    InSet(InSet),
}

/// The materialized value set of an `IN (SELECT ...)`.
pub(super) struct InSet {
    /// Non-`NULL` members, keyed by `GroupingKey` (canonical equality:
    /// `-0.0 == 0.0`, NaN canonical -- the same equality `=` uses).
    values: HashSet<GroupingKey>,
    /// At least one member is `NULL`.
    has_null: bool,
    /// The subquery produced no rows at all.
    empty: bool,
}

pub(super) fn eval_subquery<'a>(
    sq: &BoundSubqueryExpr,
    ctx: &RowContext,
    ec: &ExecCtx<'a>,
) -> Result<Option<RelationalValue>> {
    let plan = sq
        .plan
        .as_ref()
        .ok_or_else(|| SqlError::UnsupportedExecution {
            detail: "a subquery reached execution without a physical plan (internal planner invariant violation)".to_string(),
        })?;
    let correlated = !sq.outer_refs.is_empty();

    // The enclosing row is evaluated first for `IN` (its operand is an
    // expression of the *outer* query).
    let operand = match &sq.operand {
        Some(e) => Some(crate::exec::expr_eval::eval(e, ctx, ec)?),
        None => None,
    };

    if correlated {
        begin_correlated_evaluation(sq, ec)?;
    }

    let cached = if correlated {
        None
    } else {
        ec.subquery_cache.borrow().get(&sq.id).cloned()
    };
    if cached.is_some() {
        ec.metrics.record_subquery_cache_hit();
    }

    let result = match sq.kind {
        SubqueryKind::Scalar => {
            let value = match cached.as_deref() {
                Some(CachedSubquery::Scalar(v)) => v.clone(),
                _ => {
                    let v = run_scalar(sq, plan, correlated, ctx, ec)?;
                    store(sq, correlated, ec, CachedSubquery::Scalar(v.clone()));
                    v
                }
            };
            return Ok(value);
        }
        SubqueryKind::Exists { negated } => {
            let found = match cached.as_deref() {
                Some(CachedSubquery::Exists(b)) => *b,
                _ => {
                    let b = run_exists(plan, correlated, ctx, ec)?;
                    store(sq, correlated, ec, CachedSubquery::Exists(b));
                    b
                }
            };
            Tri::from_bool(found != negated)
        }
        SubqueryKind::In { negated } => {
            let lhs = operand.expect("binder always sets an operand on an IN subquery");
            let t = match cached.as_deref() {
                Some(CachedSubquery::InSet(set)) => in_tri(&lhs, set),
                _ => {
                    let set = run_in_set(plan, correlated, ctx, ec)?;
                    let t = in_tri(&lhs, &set);
                    store(sq, correlated, ec, CachedSubquery::InSet(set));
                    t
                }
            };
            if negated {
                tri_not(t)
            } else {
                t
            }
        }
    };
    Ok(tri_to_bool(result))
}

fn store(sq: &BoundSubqueryExpr, correlated: bool, ec: &ExecCtx, value: CachedSubquery) {
    if !correlated {
        ec.subquery_cache
            .borrow_mut()
            .insert(sq.id, Rc::new(value));
    }
}

/// Starts one re-execution of a correlated subquery for the current outer
/// row: the deadline and cancellation are checked *here*, between outer rows
/// (not only inside the inner plan), and the evaluation is counted against
/// both correlated-subquery limits before any inner work starts. Returns this
/// subquery's evaluation ordinal (1-based).
fn begin_correlated_evaluation(sq: &BoundSubqueryExpr, ec: &ExecCtx) -> Result<u64> {
    begin_correlated(sq.id, ec)
}

/// The same gate for a `LATERAL` derived table re-run for a row of the items
/// before it (`id` is its relation id, disjoint from every subquery id).
pub(super) fn begin_correlated_relation(id: u32, ec: &ExecCtx) -> Result<u64> {
    begin_correlated(id, ec)
}

fn begin_correlated(id: u32, ec: &ExecCtx) -> Result<u64> {
    ec.check()?;
    let total = ec.correlated_total.get().saturating_add(1);
    if total > ec.limits.max_correlated_reexecutions {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "correlated subqueries were re-executed more than max_correlated_reexecutions ({}) times in one statement",
                ec.limits.max_correlated_reexecutions
            ),
        });
    }
    let mut by_id = ec.correlated_by_subquery.borrow_mut();
    let count = by_id.entry(id).or_insert(0);
    *count = count.saturating_add(1);
    if *count > ec.limits.max_subquery_outer_rows {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "a correlated subquery was evaluated for more than max_subquery_outer_rows ({}) enclosing rows",
                ec.limits.max_subquery_outer_rows
            ),
        });
    }
    ec.correlated_total.set(total);
    ec.metrics.record_correlated_subquery_reexec();
    Ok(*count)
}

/// `(t.name = 'bob', t.id = 2)` -- the enclosing row a correlated subquery
/// was evaluating, named by the columns it reads plus the primary key of the
/// tables they come from, so a runtime error can be traced to a row.
fn describe_outer_row(sq: &BoundSubqueryExpr, ctx: &RowContext) -> String {
    let parts: Vec<String> = sq
        .outer_labels
        .iter()
        .map(|(label, column)| {
            let value = ctx.get(column.table_ref.0, column.ordinal);
            format!("{label} = {}", describe_value(value))
        })
        .collect();
    format!("({})", parts.join(", "))
}

/// A bounded, terminal-safe rendering of one value for an error message.
fn describe_value(value: Option<&RelationalValue>) -> String {
    const MAX_TEXT: usize = 40;
    match value {
        None => "NULL".to_string(),
        Some(RelationalValue::Boolean(b)) => b.to_string(),
        Some(RelationalValue::Integer(n)) => n.to_string(),
        Some(RelationalValue::Bigint(n)) => n.to_string(),
        Some(RelationalValue::Real(n)) => n.to_string(),
        Some(RelationalValue::Double(n)) => n.to_string(),
        Some(RelationalValue::Decimal(n, scale)) => format!("decimal({n}, scale {scale})"),
        Some(RelationalValue::Text(s)) => {
            // `{:?}` escapes control characters and quotes, so a value cannot
            // inject terminal escapes into the message.
            let shown: String = s.chars().take(MAX_TEXT).collect();
            if s.chars().count() > MAX_TEXT {
                format!("{shown:?}...")
            } else {
                format!("{shown:?}")
            }
        }
        Some(RelationalValue::Blob(b)) => format!("<{} byte blob>", b.len()),
        Some(RelationalValue::Date(n)) => format!("date({n})"),
        Some(RelationalValue::Time(n)) => format!("time({n})"),
        Some(RelationalValue::Timestamp(n)) => format!("timestamp({n})"),
    }
}

fn open<'a>(
    plan: &crate::plan::physical::PhysicalPlan,
    correlated: bool,
    ctx: &RowContext,
    ec: &ExecCtx<'a>,
) -> Result<Box<dyn Operator<'a> + 'a>> {
    ec.metrics.record_subquery_execution();
    if correlated {
        build_operator(plan, ctx, ec)
    } else {
        // Nothing in an uncorrelated plan reads the outer row; not handing
        // it over avoids cloning the outer context into the inner scans.
        build_operator(plan, &RowContext::new(), ec)
    }
}

/// The single projected value of an inner row.
fn first_column(tuple: crate::exec::Tuple) -> Option<RelationalValue> {
    tuple.projected.into_iter().next().unwrap_or(None)
}

fn run_scalar<'a>(
    sq: &BoundSubqueryExpr,
    plan: &crate::plan::physical::PhysicalPlan,
    correlated: bool,
    ctx: &RowContext,
    ec: &ExecCtx<'a>,
) -> Result<Option<RelationalValue>> {
    let mut op = open(plan, correlated, ctx, ec)?;
    let Some(first) = op.next(ec)? else {
        return Ok(None);
    };
    if op.next(ec)?.is_some() {
        // SQL standard: a runtime (not parse-time) cardinality violation.
        let detail = if correlated {
            format!(
                "a scalar subquery returned more than one row for the enclosing row {}",
                describe_outer_row(sq, ctx)
            )
        } else {
            "a scalar subquery returned more than one row".to_string()
        };
        return Err(SqlError::ExecutionParameter { detail });
    }
    Ok(first_column(first))
}

fn run_exists<'a>(
    plan: &crate::plan::physical::PhysicalPlan,
    correlated: bool,
    ctx: &RowContext,
    ec: &ExecCtx<'a>,
) -> Result<bool> {
    let mut op = open(plan, correlated, ctx, ec)?;
    Ok(op.next(ec)?.is_some())
}

/// Materializes the subquery's single column into an `InSet`, bounded by
/// `max_materialized_rows` and `max_materialized_bytes` and failing closed
/// (never truncating) when either is exceeded.
fn run_in_set<'a>(
    plan: &crate::plan::physical::PhysicalPlan,
    correlated: bool,
    ctx: &RowContext,
    ec: &ExecCtx<'a>,
) -> Result<InSet> {
    let mut op = open(plan, correlated, ctx, ec)?;
    let mut values: HashSet<GroupingKey> = HashSet::new();
    let mut has_null = false;
    let mut rows = 0usize;
    let mut bytes = 0usize;
    while let Some(tuple) = op.next(ec)? {
        ec.check()?;
        rows += 1;
        if rows > ec.limits.max_materialized_rows {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "an IN subquery produced more than max_materialized_rows ({}) rows",
                    ec.limits.max_materialized_rows
                ),
            });
        }
        match first_column(tuple) {
            None => has_null = true,
            Some(v) => {
                let key = GroupingKey::from_values(&[Some(v)]);
                if !values.contains(&key) {
                    bytes = bytes.saturating_add(key.estimated_bytes() + HASH_ENTRY_OVERHEAD);
                    if bytes > ec.limits.max_materialized_bytes {
                        return Err(SqlError::ResourceLimit {
                            detail: format!(
                                "an IN subquery result exceeded max_materialized_bytes ({})",
                                ec.limits.max_materialized_bytes
                            ),
                        });
                    }
                    values.insert(key);
                }
            }
        }
    }
    Ok(InSet {
        values,
        has_null,
        empty: rows == 0,
    })
}

/// Rough per-entry cost of a `HashSet` slot beyond the key's own bytes.
const HASH_ENTRY_OVERHEAD: usize = 32;

/// `lhs IN (set)` under SQL three-valued logic:
///
/// * empty set -> `FALSE`, even for a `NULL` `lhs` (nothing to be unknown
///   about);
/// * `lhs` is `NULL` -> `UNKNOWN`;
/// * `lhs` is a member -> `TRUE`;
/// * not a member but the set contains `NULL` -> `UNKNOWN` (the unknown
///   member might have been equal);
/// * otherwise `FALSE`.
///
/// `NOT IN` is the three-valued negation of this, which is why
/// `x NOT IN (1, 2, NULL)` is never `TRUE`.
fn in_tri(lhs: &Option<RelationalValue>, set: &InSet) -> Tri {
    if set.empty {
        return Tri::False;
    }
    match lhs {
        None => Tri::Unknown,
        Some(v) => {
            if set
                .values
                .contains(&GroupingKey::from_values(&[Some(v.clone())]))
            {
                Tri::True
            } else if set.has_null {
                Tri::Unknown
            } else {
                Tri::False
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Direct tests of the per-re-execution gate: the deadline / cancellation
    //! check happens *between* outer rows, before any inner work or counting.

    use std::time::Duration;

    use super::*;
    use crate::bound::{BoundExprKind, BoundStatement};
    use crate::exec::{CancellationToken, ExecLimits, ExecMetrics};
    use crate::mission1_support::Db;

    fn correlated_node(db: &Db) -> BoundSubqueryExpr {
        let sql = "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)";
        let BoundStatement::Select(s) = db.try_bind(sql).unwrap() else {
            panic!("expected a SELECT");
        };
        match s.selection.unwrap().kind {
            BoundExprKind::Subquery(sq) => *sq,
            other => panic!("expected a subquery, got {other:?}"),
        }
    }

    #[test]
    fn an_expired_deadline_is_detected_before_the_evaluation_is_counted() {
        let db = Db::new("sq_unit_deadline");
        let sq = correlated_node(&db);
        let txn = db.txm.begin().unwrap();
        let limits = ExecLimits {
            deadline: Some(Duration::ZERO),
            ..ExecLimits::default()
        };
        let metrics = ExecMetrics::default();
        let cancel = CancellationToken::new();
        let ec = ExecCtx::new(&txn, &db.store, &db.builder, &[], &limits, &metrics, &cancel);
        assert_eq!(
            begin_correlated_evaluation(&sq, &ec).unwrap_err(),
            SqlError::DeadlineExceeded
        );
        assert_eq!(metrics.snapshot().correlated_subquery_reexecs, 0);
        assert_eq!(ec.correlated_total.get(), 0);
        let _ = txn.rollback();
        db.cleanup();
    }

    #[test]
    fn a_cancelled_statement_is_detected_before_the_evaluation_is_counted() {
        let db = Db::new("sq_unit_cancel");
        let sq = correlated_node(&db);
        let txn = db.txm.begin().unwrap();
        let limits = ExecLimits::default();
        let metrics = ExecMetrics::default();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ec = ExecCtx::new(&txn, &db.store, &db.builder, &[], &limits, &metrics, &cancel);
        assert_eq!(
            begin_correlated_evaluation(&sq, &ec).unwrap_err(),
            SqlError::Cancelled
        );
        assert_eq!(ec.correlated_total.get(), 0);
        let _ = txn.rollback();
        db.cleanup();
    }

    #[test]
    fn each_admitted_evaluation_is_counted_and_numbered_from_one() {
        let db = Db::new("sq_unit_count");
        let sq = correlated_node(&db);
        let txn = db.txm.begin().unwrap();
        let limits = ExecLimits::default();
        let metrics = ExecMetrics::default();
        let cancel = CancellationToken::new();
        let ec = ExecCtx::new(&txn, &db.store, &db.builder, &[], &limits, &metrics, &cancel);
        for expected in 1..=3u64 {
            assert_eq!(begin_correlated_evaluation(&sq, &ec).unwrap(), expected);
        }
        assert_eq!(metrics.snapshot().correlated_subquery_reexecs, 3);
        let _ = txn.rollback();
        db.cleanup();
    }

    #[test]
    fn a_refused_evaluation_is_not_counted_as_run() {
        let db = Db::new("sq_unit_refused");
        let sq = correlated_node(&db);
        let txn = db.txm.begin().unwrap();
        let limits = ExecLimits {
            max_correlated_reexecutions: 2,
            ..ExecLimits::default()
        };
        let metrics = ExecMetrics::default();
        let cancel = CancellationToken::new();
        let ec = ExecCtx::new(&txn, &db.store, &db.builder, &[], &limits, &metrics, &cancel);
        begin_correlated_evaluation(&sq, &ec).unwrap();
        begin_correlated_evaluation(&sq, &ec).unwrap();
        assert!(matches!(
            begin_correlated_evaluation(&sq, &ec).unwrap_err(),
            SqlError::ResourceLimit { .. }
        ));
        assert_eq!(metrics.snapshot().correlated_subquery_reexecs, 2);
        let _ = txn.rollback();
        db.cleanup();
    }
}
