//! Binding of recursive CTEs (SQL-mission-1 Slice D): `WITH RECURSIVE name AS
//! (seed UNION [ALL] recursive-term)`.
//!
//! # What recurses
//!
//! A definition of a `WITH RECURSIVE` list recurses when its body names the
//! definition itself. Such a body must be a set operation whose **outermost**
//! operator is `UNION` or `UNION ALL`: its left operand is the *seed* (the
//! initial working table) and its right operand is the *recursive term*. For
//! `A UNION ALL B UNION ALL C`, which the parser reads as `(A UNION ALL B)
//! UNION ALL C`, the seed is `A UNION ALL B` and the term is `C`; for `A UNION
//! ALL (B UNION ALL C)` the seed is `A` and the term is `B UNION ALL C`. A
//! definition that does not name itself is an ordinary CTE, however the list
//! is written.
//!
//! # Name resolution (the rules the tests pin)
//!
//! * In the **seed** the CTE's own name is an error, and so is any later CTE
//!   of the list.
//! * In the **recursive term** the CTE's own name is the *working table* (the
//!   previous iteration's rows), whatever the catalog holds under that name.
//!   It must appear exactly once, as a table of a `FROM` clause of the term
//!   itself (not inside a subquery, a derived table or a nested `WITH`), and
//!   not on the nullable side of an outer join. A CTE defined **earlier in the
//!   same list** is visible in the seed but not named in the term (the
//!   statement's rule; see the matrix for the note on how other engines
//!   differ); a CTE of an enclosing query, and every catalog table, stay
//!   visible.
//! * Outside the definition the name is the finished CTE, like any other.
//!
//! # Restrictions on a recursive term
//!
//! A `SELECT` block of the term that contains the recursive reference may not
//! aggregate, `GROUP BY`, use `HAVING`, `ORDER BY`, `LIMIT` or `OFFSET`, and
//! may use `DISTINCT` only under `UNION` (which deduplicates anyway). The
//! trailing `ORDER BY` / `LIMIT` of the whole body is refused too. The term's
//! output must have the seed's column types exactly (an untyped `NULL` adopts
//! them). Window functions are not in the engine's grammar at all and are
//! refused by the parser.

use std::cell::Cell;

use crate::ast;
use crate::bind::env::BindEnv;
use crate::bind::scope::{self, Boundary, BoundaryKind, OuterScopes, Scope, WorkingBinding};
use crate::bound::{BoundRecursion, BoundRelation, BoundSelect};
use crate::error::{Result, SqlError};
use crate::limits::SqlLimits;

fn refuse(detail: String) -> SqlError {
    SqlError::InvalidIdentifier { detail }
}

// ---- does a query name a table? ----------------------------------------------

/// `true` iff `q` names a single-part table `name` anywhere: a `FROM` / `JOIN`
/// item, a derived table, a subquery, a nested `WITH` body or a set-operation
/// operand. It decides whether a definition of a `WITH RECURSIVE` list *tries*
/// to recurse (and so must have the recursive shape); the binder, not this
/// scan, decides what the name resolves to.
pub(super) fn mentions_table(q: &ast::Select, name: &str) -> bool {
    if let Some(set) = &q.set_op {
        if mentions_table(&set.left, name) || mentions_table(&set.right, name) {
            return true;
        }
    }
    if q.ctes.iter().any(|c| mentions_table(&c.query, name)) {
        return true;
    }
    if let Some(from) = &q.from {
        for t in std::iter::once(&from.first).chain(from.joins.iter().map(|j| &j.table)) {
            if let Some(body) = &t.derived {
                if mentions_table(body, name) {
                    return true;
                }
            } else if t.name.0.len() == 1 && t.name.0[0].value == name {
                return true;
            }
        }
        if from.joins.iter().any(|j| expr_mentions(&j.on, name)) {
            return true;
        }
    }
    q.projection.iter().any(|item| match item {
        ast::SelectItem::Item(i) => expr_mentions(&i.expr, name),
        _ => false,
    }) || q.selection.as_ref().is_some_and(|e| expr_mentions(e, name))
        || q.group_by.iter().any(|e| expr_mentions(e, name))
        || q.grouping.as_ref().is_some_and(|elements| {
            elements.iter().any(|el| match el {
                ast::GroupElement::Expr(e) => expr_mentions(e, name),
                ast::GroupElement::Rollup(sets)
                | ast::GroupElement::Cube(sets)
                | ast::GroupElement::Sets(sets) => {
                    sets.iter().flatten().any(|e| expr_mentions(e, name))
                }
            })
        })
        || q.having.as_ref().is_some_and(|e| expr_mentions(e, name))
        || q.qualify.as_ref().is_some_and(|e| expr_mentions(e, name))
        || q.named_windows.iter().any(|(_, w)| window_spec_mentions(w, name))
        || q.order_by.iter().any(|o| expr_mentions(&o.expr, name))
        || q.limit.as_ref().is_some_and(|e| expr_mentions(e, name))
        || q.offset.as_ref().is_some_and(|e| expr_mentions(e, name))
}

fn expr_mentions(e: &ast::Expr, name: &str) -> bool {
    use ast::Expr::*;
    match e {
        Literal(_) | Parameter(_) | Column(_) => false,
        UnaryOp { expr, .. } | IsNull { expr, .. } => expr_mentions(expr, name),
        BinaryOp { left, right, .. } => expr_mentions(left, name) || expr_mentions(right, name),
        Between { expr, low, high, .. } => {
            expr_mentions(expr, name) || expr_mentions(low, name) || expr_mentions(high, name)
        }
        InList { expr, list, .. } => {
            expr_mentions(expr, name) || list.iter().any(|x| expr_mentions(x, name))
        }
        Like { expr, pattern, .. } => expr_mentions(expr, name) || expr_mentions(pattern, name),
        Case {
            operand,
            branches,
            else_result,
        } => {
            operand.as_deref().is_some_and(|x| expr_mentions(x, name))
                || branches
                    .iter()
                    .any(|(w, t)| expr_mentions(w, name) || expr_mentions(t, name))
                || else_result.as_deref().is_some_and(|x| expr_mentions(x, name))
        }
        Function { args, .. } => args.iter().any(|x| expr_mentions(x, name)),
        Aggregate { arg, filter, .. } => {
            arg.exprs().iter().any(|x| expr_mentions(x, name))
                || filter.as_deref().is_some_and(|x| expr_mentions(x, name))
        }
        Grouping(args) => args.iter().any(|x| expr_mentions(x, name)),
        Window(call) => {
            (match &call.function {
                ast::WindowFunction::Aggregate { arg, filter, .. } => {
                    arg.exprs().iter().any(|x| expr_mentions(x, name))
                        || filter.as_deref().is_some_and(|x| expr_mentions(x, name))
                }
                ast::WindowFunction::Named { args, .. } => {
                    args.iter().any(|x| expr_mentions(x, name))
                }
            }) || match &call.over {
                ast::WindowOver::Named(_) => false,
                ast::WindowOver::Spec(spec) => window_spec_mentions(spec, name),
            }
        }
        ScalarSubquery(q) => {
            let ast::QueryExpr::Select(s) = q.as_ref();
            mentions_table(s, name)
        }
        Exists { subquery, .. } => {
            let ast::QueryExpr::Select(s) = subquery.as_ref();
            mentions_table(s, name)
        }
        InSubquery { expr, subquery, .. } | Quantified { expr, subquery, .. } => {
            let ast::QueryExpr::Select(s) = subquery.as_ref();
            expr_mentions(expr, name) || mentions_table(s, name)
        }
    }
}

fn window_spec_mentions(spec: &ast::WindowSpec, name: &str) -> bool {
    spec.partition_by.iter().any(|e| expr_mentions(e, name))
        || spec.order_by.iter().any(|o| expr_mentions(&o.expr, name))
        || spec.frame.as_ref().is_some_and(|f| {
            [&f.start, &f.end].into_iter().any(|b| match b {
                ast::FrameBound::Preceding(e) | ast::FrameBound::Following(e) => {
                    expr_mentions(e, name)
                }
                _ => false,
            })
        })
}

// ---- the restrictions on a recursive term --------------------------------------

/// `true` iff a `SELECT` block (or any operand below a set operation) reads the
/// working table `work_id` in its `FROM`.
fn reads_working_table(b: &BoundSelect, work_id: u32) -> bool {
    if let Some(set) = &b.set_op {
        return reads_working_table(&set.left, work_id) || reads_working_table(&set.right, work_id);
    }
    b.from
        .iter()
        .any(|f| matches!(&f.relation, BoundRelation::Working(w) if w.work_id == work_id))
}

fn check_term(b: &BoundSelect, name: &str, work_id: u32, all: bool) -> Result<()> {
    if !reads_working_table(b, work_id) {
        // An operand that never reads the working table is just another seed.
        return Ok(());
    }
    let here = format!("the recursive term of CTE '{name}'");
    if let Some(set) = &b.set_op {
        if !b.order_by.is_empty() || b.limit.is_some() || b.offset.is_some() {
            return Err(refuse(format!(
                "ORDER BY, LIMIT and OFFSET are not allowed on {here} (they would reorder or truncate the recursion)"
            )));
        }
        check_term(&set.left, name, work_id, all)?;
        return check_term(&set.right, name, work_id, all);
    }
    if !b.aggregates.is_empty() || !b.group_by.is_empty() || b.having.is_some() {
        return Err(refuse(format!(
            "aggregate functions, GROUP BY and HAVING are not allowed in {here}; they are allowed in the seed"
        )));
    }
    if b.grouping.is_some() || !b.windows.is_empty() || b.qualify.is_some() {
        return Err(refuse(format!(
            "window functions, QUALIFY and ROLLUP / CUBE / GROUPING SETS are not allowed in {here}; they are allowed in the seed"
        )));
    }
    if b.distinct && all {
        return Err(refuse(format!(
            "SELECT DISTINCT is not allowed in {here} when the recursion is UNION ALL; use UNION, which already removes duplicates"
        )));
    }
    if !b.order_by.is_empty() {
        return Err(refuse(format!("ORDER BY is not allowed in {here}")));
    }
    if b.limit.is_some() || b.offset.is_some() {
        return Err(refuse(format!(
            "LIMIT and OFFSET are not allowed in {here} (they would truncate the recursion)"
        )));
    }
    Ok(())
}

// ---- binding ---------------------------------------------------------------------

/// Binds the body of a definition that names itself, in a `WITH RECURSIVE`
/// list. `earlier` are the names of the list's definitions before this one.
/// Returns the set-operation shell `seed UNION [ALL] term` and, when the term
/// really reads the working table, the recursion.
pub(super) fn bind_recursive_body(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    cte: &ast::Cte,
    scope: &mut Scope,
    outer: Option<&OuterScopes<'_>>,
    earlier: &[String],
    max_parameter: &mut u32,
) -> Result<(BoundSelect, Option<BoundRecursion>)> {
    let name = cte.name.value.clone();
    let body = &cte.query;
    let set = match &body.set_op {
        Some(set) => set,
        None => {
            return Err(refuse(format!(
                "CTE '{name}' refers to itself but its body is a single SELECT; a recursive CTE must be `seed UNION [ALL] recursive-term`"
            )))
        }
    };
    if set.op != ast::SetOperator::Union {
        return Err(refuse(format!(
            "CTE '{name}' refers to itself but its outermost set operator is {}; a recursive CTE must be `seed UNION [ALL] recursive-term`",
            set.op.name()
        )));
    }
    if set.by_name {
        return Err(SqlError::Unsupported {
            detail: format!("UNION BY NAME in the body of recursive CTE '{name}'"),
        });
    }
    if !body.ctes.is_empty() {
        return Err(SqlError::Unsupported {
            detail: format!("a WITH list inside the body of recursive CTE '{name}'"),
        });
    }
    if !body.order_by.is_empty() || body.limit.is_some() || body.offset.is_some() {
        return Err(refuse(format!(
            "ORDER BY, LIMIT and OFFSET after the recursive term of CTE '{name}' are not allowed (they would reorder or truncate the recursion)"
        )));
    }

    env.enter(limits)?;
    let result = bind_parts(env, limits, cte, set, scope, outer, earlier, max_parameter);
    env.leave();
    result
}

#[allow(clippy::too_many_arguments)]
fn bind_parts(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    cte: &ast::Cte,
    set: &ast::SetOperation,
    scope: &mut Scope,
    outer: Option<&OuterScopes<'_>>,
    earlier: &[String],
    max_parameter: &mut u32,
) -> Result<(BoundSelect, Option<BoundRecursion>)> {
    let name = cte.name.value.clone();
    let described = format!("CTE '{name}'");

    // The seed: the CTE's own name is not visible in it.
    scope.seed_phase = true;
    let seed = {
        let link = OuterScopes {
            scope: &*scope,
            parent: outer,
            boundary: Some(Boundary {
                kind: BoundaryKind::Cte,
                name: &name,
            }),
        };
        super::select::bind_select_in(env, limits, &set.left, Some(&link))
    };
    scope.seed_phase = false;
    let seed = seed?;

    // The working table has the seed's columns. They are all nullable: the
    // recursive term may produce a NULL where the seed never does.
    let (_, typed) = super::select::relation_output(&described, &seed, &cte.columns)?;
    let nullable: Vec<_> = typed.iter().map(|(n, t, _)| (n.clone(), *t, true)).collect();
    let columns = scope::relation_columns(&described, &nullable)?;
    let work_id = env.alloc_relation_id();
    scope.working = Some(WorkingBinding {
        name: name.clone(),
        work_id,
        columns,
        depth: env.depth(),
        refs: Cell::new(0),
    });
    scope.hidden_ctes = earlier.to_vec();

    let term = {
        let link = OuterScopes {
            scope: &*scope,
            parent: outer,
            boundary: Some(Boundary {
                kind: BoundaryKind::Cte,
                name: &name,
            }),
        };
        super::select::bind_select_in(env, limits, &set.right, Some(&link))
    };
    let refs = scope.working.take().map_or(0, |w| w.refs.get());
    scope.hidden_ctes.clear();
    let term = term?;
    *max_parameter = (*max_parameter).max(seed.max_parameter).max(term.max_parameter);

    if refs == 0 {
        // The right operand never read the working table: an ordinary UNION
        // of two queries (types unify as for any set operation).
        let shell = super::setop::combine(env, set.op, set.all, seed, term, None)?;
        return Ok((shell, None));
    }

    check_term(&term, &name, work_id, set.all)?;
    for (i, (s, t)) in seed.projection.iter().zip(&term.projection).enumerate() {
        if seed.projection.len() != term.projection.len() {
            break;
        }
        if let (Some(a), Some(b)) = (s.expr.ty, t.expr.ty) {
            if a != b {
                return Err(SqlError::TypeMismatch {
                    detail: format!(
                        "recursive CTE '{name}' column {} has type {} in the seed but {} in the recursive term; the two must be the same type",
                        i + 1,
                        super::setop::type_name(a),
                        super::setop::type_name(b)
                    ),
                });
            }
        }
    }
    let shell = super::setop::combine(env, set.op, set.all, seed, term, None)?;
    Ok((
        shell,
        Some(BoundRecursion {
            work_id,
            all: set.all,
        }),
    ))
}
