//! `SELECT` binding — item 27/28: `FROM`/`JOIN` scope construction
//! (item 28's join binding, ambiguity resolved safely), wildcard
//! expansion (item 18, using real catalog column ordering, every
//! resulting column already authorized because its source table was),
//! `WHERE`/`ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT` (item 27) — no
//! execution.
//!
//! **Increment 11** extends this with `GROUP BY`/`HAVING`/aggregate
//! binding (`PHASE_RELATIONAL_AGGREGATION_ARCHITECTURE.md`):
//! - `GROUP BY` expressions are bound as plain (non-aggregate) exprs
//!   against the same `FROM` scope `WHERE` uses.
//! - Aggregate function calls are allowed in the projection/`HAVING`/
//!   `ORDER BY` (never `WHERE` — `ExprBinder::allow_aggregate` defaults
//!   `false` and is never set for `WHERE`'s own binder, so an aggregate
//!   there is rejected structurally, not by a special-cased check here;
//!   item 19).
//! - A statement is "aggregated" (item 16/17) iff it has a non-empty
//!   `GROUP BY`, a `HAVING` clause, or at least one aggregate call in its
//!   projection — the same three conditions SQL itself uses to decide
//!   whether ungrouped-column rules apply at all.
//! - For an aggregated statement, every projection/`HAVING`/`ORDER BY`
//!   expression is validated (`validate_group_compat`, item 17/22): each
//!   subexpression must either exactly match a `GROUP BY` expression
//!   (the whole subtree is then constant within a group — SQL's own
//!   "functionally dependent" allowance, applied here by exact structural
//!   match, never approximate reasoning) or be entirely inside an
//!   aggregate call. A bare column reference satisfying neither is
//!   rejected (item 17's "do not silently select an arbitrary row's
//!   non-grouped column").
//! - Every aggregate call surviving validation is then extracted
//!   (`extract_aggregates`) out of the projection/`HAVING`/`ORDER BY`
//!   trees into one shared, deduplicated `BoundSelect::aggregates` list,
//!   with each call site rewritten to `BoundExprKind::AggregateRef(idx)`
//!   — the future `Aggregate` physical operator's own output-column
//!   index space (item 25's "no runtime state in the plan": this
//!   rewrite happens once, at bind time, producing a static index, never
//!   re-resolved by a later pass).

use rubixdb::catalog::schema::Privilege;
use rubixdb::catalog::CatalogService;
use rubixdb::relational::RelationalType;

use crate::ast;
use crate::auth::AuthContext;
use crate::bind::env::BindEnv;
use crate::bind::expr::ExprBinder;
use crate::bind::scope::{
    self, BindContext, Boundary, BoundaryKind, CteBinding, CteLookup, OuterScopes, Scope,
    ScopeEntry,
};
use crate::bind::window::NamedWindows;
use crate::bound::*;
use crate::error::{Result, SqlError};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;

pub fn bind_select(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    limits: &SqlLimits,
    stmt: &ast::Select,
) -> Result<BoundSelect> {
    let env = BindEnv::new(catalog, ctx, auth, metrics);
    bind_select_in(&env, limits, stmt, None)
}

/// Binds one `SELECT` -- the statement itself or (SQL-mission-1) a subquery
/// nested in one -- inside a statement-wide `BindEnv`. `outer` is the chain
/// of enclosing queries (`None` for the statement's outermost query): a
/// column this query's own `FROM` does not define is resolved against it,
/// innermost enclosing query first.
pub fn bind_select_in(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    stmt: &ast::Select,
    outer: Option<&OuterScopes<'_>>,
) -> Result<BoundSelect> {
    bind_select_in_with(env, limits, stmt, outer, &[])
}

/// `bind_select_in` for a query that is an operand of a set operation
/// (SQL-mission-1 Slice E): `inherited` are the CTEs the set operation's own
/// `WITH` list defined, which the operand sees as if it had written them (its
/// own `WITH` list, if any, shadows them).
pub(super) fn bind_select_in_with(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    stmt: &ast::Select,
    outer: Option<&OuterScopes<'_>>,
    inherited: &[CteBinding],
) -> Result<BoundSelect> {
    if stmt.set_op.is_some() {
        return super::setop::bind_set_operation(env, limits, stmt, outer, inherited);
    }
    let mut scope = env.new_scope();
    scope.ctes = inherited.to_vec();
    let mut from = Vec::new();
    let mut max_parameter = 0u32;

    let ctes = bind_with_list(env, limits, &stmt.ctes, &mut scope, outer, &mut max_parameter)?;

    if let Some(from_clause) = &stmt.from {
        let (table, relation) = bind_from_item(
            env,
            limits,
            &mut scope,
            outer,
            &from_clause.first,
            false,
            &mut max_parameter,
        )?;
        from.push(BoundFromItem {
            table,
            join: None,
            relation,
        });

        for join in &from_clause.joins {
            let null_extended = matches!(join.kind, ast::JoinKind::Left);
            let (table, relation) = bind_from_item(
                env,
                limits,
                &mut scope,
                outer,
                &join.table,
                null_extended,
                &mut max_parameter,
            )?;

            let mut eb = ExprBinder::new(Some(&scope), limits)
                .with_capture_env(env)
                .with_outer(outer);
            let on = eb.bind(&join.on, Some(RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);

            from.push(BoundFromItem {
                table,
                join: Some((join.kind, on)),
                relation,
            });
        }
    }

    // WHERE — always the plain (non-aggregate) binder; item 19's "no
    // aggregate expression in WHERE" holds structurally, since this
    // binder never opts into `with_aggregate(true)`.
    let selection = stmt
        .selection
        .as_ref()
        .map(|e| -> Result<BoundExpr> {
            let mut eb = ExprBinder::new(Some(&scope), limits)
                .with_env(env)
                .with_outer(outer);
            let bound = eb.bind(e, Some(RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            Ok(bound)
        })
        .transpose()?;

    // GROUP BY — plain (non-aggregate) expressions against the same
    // scope, item 12: no wildcard/aggregate allowed here. SQL-mission-2:
    // ROLLUP / CUBE / GROUPING SETS elements are interned into the same list.
    let (group_by, grouping) =
        bind_group_by(env, limits, &scope, outer, stmt, &mut max_parameter)?;

    // WINDOW name AS (...) definitions (SQL-mission-2).
    let named_windows = collect_named_windows(&stmt.named_windows)?;

    let (projection, projection_aliases) = bind_projection(
        env,
        &scope,
        outer,
        limits,
        &stmt.projection,
        &named_windows,
        &group_by,
        &mut max_parameter,
    )?;
    // `GROUP BY ALL`: group by every select-list item that is not an aggregate
    // or window function call (Snowflake).
    let group_by = if stmt.group_by_all {
        derive_group_by_all(&projection)
    } else {
        group_by
    };
    // The table references this query's own FROM defines: a column of any
    // other table is an outer-query column, constant within one evaluation
    // of this query (SQL-mission-1).
    let own_refs: std::collections::BTreeSet<u32> =
        scope.entries.iter().map(|e| e.table_ref_id).collect();

    // HAVING — aggregate calls allowed (item 20).
    let having = stmt
        .having
        .as_ref()
        .map(|e| -> Result<BoundExpr> {
            let mut eb = ExprBinder::new(Some(&scope), limits)
                .with_aggregate(true)
                .with_env(env)
                .with_outer(outer)
                .with_group_exprs(&group_by);
            let bound = eb.bind(e, Some(RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            Ok(bound)
        })
        .transpose()?;

    // QUALIFY (SQL-mission-2): a filter over window function results. It may
    // use the select list's aliases (`QUALIFY rn = 1`) where no column of that
    // name exists, and aggregates of an aggregated query.
    let alias_exprs: Vec<(String, BoundExpr)> = projection_aliases
        .iter()
        .map(|(name, idx)| (name.clone(), projection[*idx].expr.clone()))
        .collect();
    let qualify = stmt
        .qualify
        .as_ref()
        .map(|e| -> Result<BoundExpr> {
            let mut eb = ExprBinder::new(Some(&scope), limits)
                .with_aggregate(true)
                .with_window(true)
                .with_env(env)
                .with_outer(outer)
                .with_named_windows(&named_windows)
                .with_group_exprs(&group_by)
                .with_aliases(&alias_exprs);
            let bound = eb.bind(e, Some(RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            Ok(bound)
        })
        .transpose()?;
    if let Some(q) = &qualify {
        // Snowflake / DuckDB semantics: QUALIFY filters on window results, so a
        // statement with no window function anywhere is a mistake (a WHERE or
        // HAVING was meant); refused rather than run as a different clause.
        let is_window = |e: &BoundExpr| matches!(e.kind, BoundExprKind::Window(_));
        if !q.any(&is_window) && !projection.iter().any(|i| i.expr.any(&is_window)) {
            return Err(SqlError::TypeMismatch {
                detail: "QUALIFY requires a window function in the SELECT list or in the QUALIFY condition".to_string(),
            });
        }
    }

    // item 16: a statement is "aggregated" iff it has a GROUP BY, a
    // HAVING clause, or at least one aggregate call in its projection —
    // exactly the three SQL-standard triggers for ungrouped-column
    // validation, never inferred from anything else. (SQL-mission-2: a
    // ROLLUP / CUBE / GROUPING SETS clause is a GROUP BY, and QUALIFY may
    // aggregate like the select list.)
    let is_aggregated = !group_by.is_empty()
        || grouping.is_some()
        || having.is_some()
        || projection.iter().any(|item| contains_aggregate(&item.expr))
        || qualify.as_ref().is_some_and(contains_aggregate);

    if is_aggregated {
        // item 17: every projection/HAVING expression must be either
        // group-compatible or fully inside an aggregate call.
        for item in &projection {
            validate_group_compat(&item.expr, &group_by, &own_refs)?;
        }
        if let Some(h) = &having {
            validate_group_compat(h, &group_by, &own_refs)?;
        }
        if let Some(q) = &qualify {
            validate_group_compat(q, &group_by, &own_refs)?;
        }
    }

    // ORDER BY — item 22: aggregate calls and grouping expressions
    // allowed only once the statement is already known to be aggregated;
    // validated against the same GROUP BY-compatibility rule.
    let order_by = stmt
        .order_by
        .iter()
        .map(|item| -> Result<BoundOrderByItem> {
            // A select-list position (`ORDER BY 2`) or an output alias
            // (`ORDER BY total`) names an item of the select list itself.
            let bound = match resolve_output_reference(&item.expr, &projection, &projection_aliases)? {
                Some(expr) => expr,
                None => {
                    // Any other expression sees the FROM columns first and
                    // falls back to the select list's aliases (`ORDER BY a + 1`).
                    let mut eb = ExprBinder::new(Some(&scope), limits)
                        .with_aggregate(is_aggregated)
                        .with_window(true)
                        .with_env(env)
                        .with_outer(outer)
                        .with_named_windows(&named_windows)
                        .with_group_exprs(&group_by)
                        .with_aliases(&alias_exprs);
                    let bound = eb.bind(&item.expr, None)?;
                    max_parameter = max_parameter.max(eb.max_parameter);
                    bound
                }
            };
            if is_aggregated {
                validate_group_compat(&bound, &group_by, &own_refs)?;
            }
            // D5 default: NULLS LAST ascending / NULLS FIRST descending.
            let nulls = match item.nulls_first {
                Some(true) => NullsOrder::First,
                Some(false) => NullsOrder::Last,
                None if item.descending => NullsOrder::First,
                None => NullsOrder::Last,
            };
            Ok(BoundOrderByItem {
                expr: bound,
                descending: item.descending,
                nulls,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // item 25: extract every aggregate call out of projection/HAVING/
    // ORDER BY into one shared, deduplicated list, rewriting each call
    // site to a stable positional reference into it.
    let mut aggregates: Vec<BoundAggregateExpr> = Vec::new();
    let projection: Vec<BoundSelectItem> = projection
        .into_iter()
        .map(|item| BoundSelectItem {
            expr: extract_aggregates(item.expr, &mut aggregates),
            output_name: item.output_name,
        })
        .collect();
    let having = having.map(|h| extract_aggregates(h, &mut aggregates));
    let qualify = qualify.map(|q| extract_aggregates(q, &mut aggregates));
    let order_by: Vec<BoundOrderByItem> = order_by
        .into_iter()
        .map(|item| BoundOrderByItem {
            expr: extract_aggregates(item.expr, &mut aggregates),
            descending: item.descending,
            nulls: item.nulls,
        })
        .collect();

    // SQL-mission-2: with ROLLUP / CUBE / GROUPING SETS a grouping column is
    // NULL in the rows of a set that leaves it out, so its occurrences are
    // rewritten to read the group's key (`GroupKeyRef`) instead of the
    // representative input row -- which would still hold the column's value.
    //
    // A correlated subquery reads the group's representative row, not these
    // rewritten trees: `validate_group_compat` already limits it to columns that
    // are bare grouping expressions, and `AggregateOp` makes such a column `NULL`
    // in the representative row of every set that leaves it out.
    let (projection, having, qualify, order_by) = if grouping.is_some() {
        (
            projection
                .into_iter()
                .map(|item| BoundSelectItem {
                    expr: rewrite_group_keys(item.expr, &group_by),
                    output_name: item.output_name,
                })
                .collect::<Vec<_>>(),
            having.map(|h| rewrite_group_keys(h, &group_by)),
            qualify.map(|q| rewrite_group_keys(q, &group_by)),
            order_by
                .into_iter()
                .map(|item| BoundOrderByItem {
                    expr: rewrite_group_keys(item.expr, &group_by),
                    descending: item.descending,
                    nulls: item.nulls,
                })
                .collect::<Vec<_>>(),
        )
    } else {
        (projection, having, qualify, order_by)
    };

    // SQL-mission-2: collect the window function calls out of the SELECT list,
    // QUALIFY and ORDER BY into one list (identical calls share a slot); those
    // trees now read each result through `WindowRef`.
    let mut windows: Vec<BoundWindowCall> = Vec::new();
    let projection: Vec<BoundSelectItem> = projection
        .into_iter()
        .map(|item| BoundSelectItem {
            expr: extract_windows(item.expr, &mut windows),
            output_name: item.output_name,
        })
        .collect();
    let qualify = qualify.map(|q| extract_windows(q, &mut windows));
    let order_by: Vec<BoundOrderByItem> = order_by
        .into_iter()
        .map(|item| BoundOrderByItem {
            expr: extract_windows(item.expr, &mut windows),
            descending: item.descending,
            nulls: item.nulls,
        })
        .collect();
    if windows.len() > limits.max_columns {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "statement uses {} distinct window function calls; max_columns is {}",
                windows.len(),
                limits.max_columns
            ),
        });
    }

    // item 49/74: bound the number of distinct aggregate expressions one
    // statement may declare — reusing `SqlLimits::max_columns` (the same
    // "most output expressions one SELECT may declare" bound already
    // applied to the projection itself) rather than inventing a second,
    // independently-maintained constant for a structurally identical
    // resource shape.
    if aggregates.len() > limits.max_columns {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "statement references {} distinct aggregate expression(s); max_columns is {}",
                aggregates.len(),
                limits.max_columns
            ),
        });
    }

    let mut int_binder = ExprBinder::new(None, limits);
    let limit = stmt
        .limit
        .as_ref()
        .map(|e| int_binder.bind(e, Some(RelationalType::Bigint)))
        .transpose()?;
    let offset = stmt
        .offset
        .as_ref()
        .map(|e| int_binder.bind(e, Some(RelationalType::Bigint)))
        .transpose()?;
    max_parameter = max_parameter.max(int_binder.max_parameter);

    Ok(BoundSelect {
        ctes,
        set_op: None,
        distinct: stmt.distinct,
        from,
        projection,
        selection,
        group_by,
        grouping,
        aggregates,
        having,
        windows,
        qualify,
        order_by,
        limit,
        offset,
        max_parameter,
    })
}

/// `WINDOW name AS (...)` definitions by name; a name defined twice is an error.
fn collect_named_windows(defs: &[(ast::Ident, ast::WindowSpec)]) -> Result<NamedWindows> {
    let mut out = NamedWindows::new();
    for (name, spec) in defs {
        if out.insert(name.value.clone(), spec.clone()).is_some() {
            return Err(SqlError::InvalidIdentifier {
                detail: format!("window '{}' is defined more than once", name.value),
            });
        }
    }
    Ok(out)
}

/// Binds the `GROUP BY` clause. Without grouping extensions it is the plain
/// list of expressions. With `ROLLUP` / `CUBE` / `GROUPING SETS` (SQL-mission-2)
/// every expression the clause names is interned into one list of distinct
/// grouping expressions (structurally equal ones are the same grouping column),
/// and the clause's structure is kept as indexes into it.
fn bind_group_by(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    scope: &Scope,
    outer: Option<&OuterScopes<'_>>,
    stmt: &ast::Select,
    max_parameter: &mut u32,
) -> Result<(Vec<BoundExpr>, Option<BoundGrouping>)> {
    fn bind_one(
        env: &BindEnv<'_>,
        limits: &SqlLimits,
        scope: &Scope,
        outer: Option<&OuterScopes<'_>>,
        e: &ast::Expr,
        max_parameter: &mut u32,
    ) -> Result<BoundExpr> {
        let mut eb = ExprBinder::new(Some(scope), limits)
            .with_capture_env(env)
            .with_outer(outer);
        let bound = eb.bind(e, None)?;
        *max_parameter = (*max_parameter).max(eb.max_parameter);
        Ok(bound)
    }

    let Some(elements) = &stmt.grouping else {
        let plain = stmt
            .group_by
            .iter()
            .map(|e| bind_one(env, limits, scope, outer, e, max_parameter))
            .collect::<Result<Vec<_>>>()?;
        return Ok((plain, None));
    };

    // The GROUPING() bit string and the executor's key bookkeeping hold one
    // bit per grouping expression.
    const MAX_GROUPING_EXPRS: usize = 64;
    let mut exprs: Vec<BoundExpr> = Vec::new();
    let mut intern = |e: &ast::Expr, max_parameter: &mut u32| -> Result<usize> {
        let bound = bind_one(env, limits, scope, outer, e, max_parameter)?;
        if let Some(i) = exprs.iter().position(|g| *g == bound) {
            return Ok(i);
        }
        if exprs.len() >= MAX_GROUPING_EXPRS {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "a GROUP BY with ROLLUP, CUBE or GROUPING SETS supports at most {MAX_GROUPING_EXPRS} distinct grouping expressions"
                ),
            });
        }
        exprs.push(bound);
        Ok(exprs.len() - 1)
    };
    if elements.len() > limits.max_columns {
        return Err(SqlError::ResourceLimit {
            detail: format!("GROUP BY clause exceeds max_columns ({})", limits.max_columns),
        });
    }
    let mut members = |sets: &[Vec<ast::Expr>], max_parameter: &mut u32| -> Result<Vec<Vec<usize>>> {
        if sets.len() > limits.max_columns {
            return Err(SqlError::ResourceLimit {
                detail: format!("GROUP BY clause exceeds max_columns ({})", limits.max_columns),
            });
        }
        sets.iter()
            .map(|set| {
                set.iter()
                    .map(|e| intern(e, max_parameter))
                    .collect::<Result<Vec<_>>>()
            })
            .collect()
    };
    let mut bound_elements = Vec::with_capacity(elements.len());
    for element in elements {
        bound_elements.push(match element {
            ast::GroupElement::Expr(e) => {
                let one = std::slice::from_ref(e);
                let sets = members(&[one.to_vec()], max_parameter)?;
                BoundGroupElement::Expr(sets[0][0])
            }
            ast::GroupElement::Rollup(sets) => BoundGroupElement::Rollup(members(sets, max_parameter)?),
            ast::GroupElement::Cube(sets) => BoundGroupElement::Cube(members(sets, max_parameter)?),
            ast::GroupElement::Sets(sets) => BoundGroupElement::Sets(members(sets, max_parameter)?),
        });
    }
    Ok((
        exprs,
        Some(BoundGrouping {
            elements: bound_elements,
        }),
    ))
}

/// Binds a `WITH` list into `scope` (SQL-mission-1 Slice C) and returns the
/// bound definitions. Each body is bound across a **boundary**: it sees the
/// CTEs defined before it (and those of enclosing queries) but no column of
/// any enclosing query, and not its own name or any later one (a forward or
/// self reference is an error unless a catalog table of that name exists).
/// Used by `SELECT`, `UPDATE`, `DELETE` and `INSERT` alike.
pub fn bind_with_list(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    ctes: &[ast::Cte],
    scope: &mut Scope,
    outer: Option<&OuterScopes<'_>>,
    max_parameter: &mut u32,
) -> Result<Vec<BoundCte>> {
    if ctes.is_empty() {
        return Ok(Vec::new());
    }
    for (i, cte) in ctes.iter().enumerate() {
        if ctes[..i].iter().any(|c| c.name.value == cte.name.value) {
            return Err(SqlError::InvalidIdentifier {
                detail: format!("CTE name '{}' is defined more than once in one WITH list", cte.name.value),
            });
        }
    }
    scope.pending_cte_names = ctes.iter().map(|c| c.name.value.clone()).collect();
    scope.recursive_keyword = ctes.iter().any(|c| c.recursive);
    let mut out: Vec<BoundCte> = Vec::with_capacity(ctes.len());
    for cte in ctes {
        let name = cte.name.value.clone();
        scope.defining_cte = Some(name.clone());
        // SQL-mission-1 Slice D: a definition of a `WITH RECURSIVE` list that
        // names itself is bound as `seed UNION [ALL] recursive-term`.
        let recursive = cte.recursive && super::recursive::mentions_table(&cte.query, &name);
        let (body, recursion) = if recursive {
            let earlier: Vec<String> = out.iter().map(|c| c.name.clone()).collect();
            let r = super::recursive::bind_recursive_body(
                env,
                limits,
                cte,
                scope,
                outer,
                &earlier,
                max_parameter,
            );
            scope.seed_phase = false;
            scope.working = None;
            scope.hidden_ctes.clear();
            r?
        } else {
            env.enter(limits)?;
            let link = OuterScopes {
                scope: &*scope,
                parent: outer,
                boundary: Some(Boundary {
                    kind: BoundaryKind::Cte,
                    name: &name,
                }),
            };
            let body = bind_select_in(env, limits, &cte.query, Some(&link));
            env.leave();
            (body?, None)
        };
        scope.defining_cte = None;
        *max_parameter = (*max_parameter).max(body.max_parameter);

        let described = format!("CTE '{name}'");
        let (names, typed) = relation_output(&described, &body, &cte.columns)?;
        let columns = scope::relation_columns(&described, &typed)?;
        let id = env.alloc_relation_id();
        if let Some(pos) = scope.pending_cte_names.iter().position(|n| n == &name) {
            scope.pending_cte_names.remove(pos);
        }
        scope.ctes.push(CteBinding { name: name.clone(), id, columns });
        out.push(BoundCte {
            id,
            name,
            columns: names,
            query: body,
            recursion,
        });
    }
    scope.pending_cte_names.clear();
    scope.recursive_keyword = false;
    Ok(out)
}

/// A relation's output columns (names, types, nullability) from its body's
/// projection, with an optional `AS alias(a, b, ...)` column-name list that
/// must name exactly as many columns as the body produces.
/// `(name, type, nullable)` of one column of a derived table or CTE.
type RelationColumn = (String, Option<RelationalType>, bool);

pub(super) fn relation_output(
    described: &str,
    body: &BoundSelect,
    column_names: &[ast::Ident],
) -> Result<(Vec<String>, Vec<RelationColumn>)> {
    if !column_names.is_empty() && column_names.len() != body.projection.len() {
        return Err(SqlError::InvalidIdentifier {
            detail: format!(
                "{described} produces {} column(s) but {} column name(s) were given",
                body.projection.len(),
                column_names.len()
            ),
        });
    }
    let names: Vec<String> = if column_names.is_empty() {
        body.projection.iter().map(|i| i.output_name.clone()).collect()
    } else {
        column_names.iter().map(|i| i.value.clone()).collect()
    };
    let typed = names
        .iter()
        .zip(&body.projection)
        .map(|(n, item)| (n.clone(), item.expr.ty, item.expr.nullable))
        .collect();
    Ok((names, typed))
}

/// Binds one `FROM` / `JOIN` item: a catalog table, a CTE reference or a
/// derived table (SQL-mission-1 Slice C), pushing its columns into `scope`.
fn bind_from_item(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    scope: &mut Scope,
    outer: Option<&OuterScopes<'_>>,
    tref: &ast::TableRef,
    null_extended: bool,
    max_parameter: &mut u32,
) -> Result<(BoundTableRef, BoundRelation)> {
    let (catalog, ctx, auth, metrics) = (env.catalog, env.ctx, env.auth, env.metrics);

    if let Some(body_ast) = &tref.derived {
        let alias = tref.alias.as_ref().ok_or_else(|| SqlError::InvalidIdentifier {
            detail: "every derived table must have an alias, e.g. `(SELECT ...) AS name`".to_string(),
        })?;
        check_alias_not_duplicate(scope, Some(alias), &alias.value)?;
        let id = env.alloc_relation_id();
        env.enter(limits)?;
        if tref.lateral {
            env.push_frame();
        }
        let link = OuterScopes {
            scope: &*scope,
            parent: outer,
            // An ordinary derived table cannot read the query it sits in or
            // anything above it; a LATERAL one can read the items before it.
            boundary: (!tref.lateral).then_some(Boundary {
                kind: BoundaryKind::Derived,
                name: &alias.value,
            }),
        };
        let bound = bind_select_in(env, limits, body_ast, Some(&link));
        let captures = if tref.lateral { env.pop_frame() } else { Vec::new() };
        env.leave();
        let query = bound?;
        *max_parameter = (*max_parameter).max(query.max_parameter);

        let described = format!("derived table '{}'", alias.value);
        let (_, typed) = relation_output(&described, &query, &tref.column_aliases)?;
        let columns = scope::relation_columns(&described, &typed)?;
        scope.push(scope::synthetic_resolved(&alias.value, columns), Some(alias), null_extended);
        let table = scope.entries.last().expect("just pushed").to_bound_table_ref();
        let outer_refs = captures.iter().filter(|c| c.correlating).map(|c| c.column).collect();
        return Ok((
            table,
            BoundRelation::Derived(Box::new(BoundDerived {
                id,
                query,
                lateral: tref.lateral,
                outer_refs,
                plan: None,
            })),
        ));
    }

    if tref.name.0.len() == 1 {
        let name = &tref.name.0[0].value;
        let cte = match OuterScopes::lookup_cte(scope, outer, name) {
            CteLookup::Found(binding) => Some(binding),
            CteLookup::NotACte => None,
            CteLookup::Working(w) => {
                return bind_working_reference(env, scope, tref, w, null_extended);
            }
            CteLookup::HiddenInRecursiveTerm => {
                return Err(SqlError::InvalidIdentifier {
                    detail: format!(
                        "CTE '{name}' is defined earlier in the same WITH RECURSIVE list and is not visible inside a recursive term; it is visible in the seed, in the CTEs after it and in the main query"
                    ),
                });
            }
            CteLookup::SelfInSeed => {
                return Err(SqlError::InvalidIdentifier {
                    detail: format!(
                        "recursive reference '{name}' must not appear in the seed (the non-recursive term) of CTE '{name}'; it belongs in the recursive term, after UNION [ALL]"
                    ),
                });
            }
            CteLookup::SelfNotShaped => {
                return Err(SqlError::InvalidIdentifier {
                    detail: format!(
                        "CTE '{name}' refers to itself but its body is not of the form `seed UNION [ALL] recursive-term`"
                    ),
                });
            }
            CteLookup::ForwardInRecursiveList => {
                return Err(SqlError::InvalidIdentifier {
                    detail: format!(
                        "CTE '{name}' is referenced before it is defined; a CTE can only use CTEs defined earlier in the same WITH list (recursive CTEs that refer to each other are not supported)"
                    ),
                });
            }
            CteLookup::SelfReference | CteLookup::ForwardReference => {
                // Not visible as a CTE here -- but a catalog table of that
                // name is (SQL scoping): only when there is none is it an
                // error, and then a precise one.
                let forward = matches!(
                    OuterScopes::lookup_cte(scope, outer, name),
                    CteLookup::ForwardReference
                );
                match scope::resolve_table(catalog, ctx, auth, metrics, &tref.name, Privilege::Select) {
                    Err(SqlError::UnknownObject { kind: "table", .. }) => {
                        return Err(SqlError::InvalidIdentifier {
                            detail: if forward {
                                format!("CTE '{name}' is referenced before it is defined; a CTE can only use CTEs defined earlier in the same WITH list")
                            } else {
                                format!("CTE '{name}' refers to itself; use WITH RECURSIVE")
                            },
                        });
                    }
                    Err(other) => return Err(other),
                    Ok(_) => None,
                }
            }
        };
        if let Some(binding) = cte {
            let effective = tref.alias.as_ref().map(|a| a.value.as_str()).unwrap_or(name);
            let described = format!("CTE '{name}'");
            if !tref.column_aliases.is_empty() && tref.column_aliases.len() != binding.columns.len() {
                return Err(SqlError::InvalidIdentifier {
                    detail: format!(
                        "{described} produces {} column(s) but {} column name(s) were given",
                        binding.columns.len(),
                        tref.column_aliases.len()
                    ),
                });
            }
            check_alias_not_duplicate(scope, tref.alias.as_ref(), name)?;
            let mut columns = binding.columns.clone();
            for (col, alias) in columns.iter_mut().zip(&tref.column_aliases) {
                col.name = alias.value.clone();
            }
            scope.push(scope::synthetic_resolved(effective, columns), tref.alias.as_ref(), null_extended);
            let table = scope.entries.last().expect("just pushed").to_bound_table_ref();
            return Ok((
                table,
                BoundRelation::Cte(BoundCteRef {
                    cte_id: binding.id,
                    name: name.clone(),
                    plan: None,
                }),
            ));
        }
    }

    if !tref.column_aliases.is_empty() {
        return Err(SqlError::Unsupported {
            detail: "a column-name list on a base-table alias".to_string(),
        });
    }
    let resolved = scope::resolve_table(catalog, ctx, auth, metrics, &tref.name, Privilege::Select)?;
    check_alias_not_duplicate(scope, tref.alias.as_ref(), &resolved.table.name)?;
    scope.push(resolved, tref.alias.as_ref(), null_extended);
    let table = scope.entries.last().expect("just pushed").to_bound_table_ref();
    Ok((table, BoundRelation::Table))
}

/// The reference to a recursive CTE inside its own recursive term: the working
/// table. It must be the only one, directly in the term's `FROM`, and not on the
/// nullable side of an outer join.
fn bind_working_reference(
    env: &BindEnv<'_>,
    scope: &mut Scope,
    tref: &ast::TableRef,
    w: scope::WorkingRef,
    null_extended: bool,
) -> Result<(BoundTableRef, BoundRelation)> {
    let name = &w.name;
    if w.ordinal > 1 {
        return Err(SqlError::InvalidIdentifier {
            detail: format!(
                "recursive reference '{name}' must appear exactly once in the recursive term"
            ),
        });
    }
    if env.depth() != w.depth {
        return Err(SqlError::InvalidIdentifier {
            detail: format!(
                "recursive reference '{name}' must be a table of the FROM clause of the recursive term itself, not inside a subquery, a derived table or a nested WITH"
            ),
        });
    }
    if null_extended {
        return Err(SqlError::InvalidIdentifier {
            detail: format!(
                "recursive reference '{name}' must not be on the nullable side of an outer join"
            ),
        });
    }
    if !tref.column_aliases.is_empty() && tref.column_aliases.len() != w.columns.len() {
        return Err(SqlError::InvalidIdentifier {
            detail: format!(
                "CTE '{name}' produces {} column(s) but {} column name(s) were given",
                w.columns.len(),
                tref.column_aliases.len()
            ),
        });
    }
    let effective = tref.alias.as_ref().map(|a| a.value.as_str()).unwrap_or(name);
    check_alias_not_duplicate(scope, tref.alias.as_ref(), name)?;
    let mut columns = w.columns.clone();
    for (col, alias) in columns.iter_mut().zip(&tref.column_aliases) {
        col.name = alias.value.clone();
    }
    scope.push(scope::synthetic_resolved(effective, columns), tref.alias.as_ref(), null_extended);
    let table = scope.entries.last().expect("just pushed").to_bound_table_ref();
    Ok((
        table,
        BoundRelation::Working(BoundWorkingRef {
            work_id: w.work_id,
            name: w.name.clone(),
        }),
    ))
}

fn check_alias_not_duplicate(
    scope: &Scope,
    alias: Option<&ast::Ident>,
    table_name: &str,
) -> Result<()> {
    let effective = alias.map(|a| a.value.as_str()).unwrap_or(table_name);
    if scope.entries.iter().any(|e| e.effective_name == effective) {
        return Err(SqlError::InvalidIdentifier {
            detail: format!("duplicate table name/alias {effective:?} in FROM"),
        });
    }
    Ok(())
}

/// The explicit aliases of a select list and the position each names.
type ProjectionAliases = Vec<(String, usize)>;

#[allow(clippy::too_many_arguments)]
fn bind_projection(
    env: &BindEnv<'_>,
    scope: &Scope,
    outer: Option<&OuterScopes<'_>>,
    limits: &SqlLimits,
    items: &[ast::SelectItem],
    named_windows: &NamedWindows,
    group_by: &[BoundExpr],
    max_parameter: &mut u32,
) -> Result<(Vec<BoundSelectItem>, ProjectionAliases)> {
    let mut out = Vec::new();
    // Explicit aliases and the select-list position they name (QUALIFY may use them).
    let mut aliases: Vec<(String, usize)> = Vec::new();
    for item in items {
        match item {
            ast::SelectItem::Item(item_expr) => {
                // Aggregate calls are always allowed to *bind* here;
                // whether they are actually legal for this statement
                // (item 16/17: only once GROUP BY/HAVING/another
                // aggregate call makes the statement "aggregated," and
                // only alongside group-compatible non-aggregate
                // expressions) is decided afterward by `bind_select`'s
                // own `is_aggregated`/`validate_group_compat` pass —
                // binding an aggregate call is never by itself illegal,
                // matching plain SQL ("SELECT COUNT(*) FROM t" needs no
                // GROUP BY).
                let mut eb = ExprBinder::new(Some(scope), limits)
                    .with_aggregate(true)
                    .with_window(true)
                    .with_env(env)
                    .with_outer(outer)
                    .with_named_windows(named_windows)
                    .with_group_exprs(group_by);
                let bound = eb.bind(&item_expr.expr, None)?;
                *max_parameter = (*max_parameter).max(eb.max_parameter);
                let output_name = item_expr
                    .alias
                    .as_ref()
                    .map(|a| a.value.clone())
                    .unwrap_or_else(|| derive_output_name(&item_expr.expr));
                if let Some(alias) = &item_expr.alias {
                    aliases.push((alias.value.clone(), out.len()));
                }
                out.push(BoundSelectItem {
                    expr: bound,
                    output_name,
                });
                if out.len() > limits.max_columns {
                    return Err(SqlError::ResourceLimit {
                        detail: format!(
                            "SELECT projection exceeds max_columns ({})",
                            limits.max_columns
                        ),
                    });
                }
            }
            ast::SelectItem::Wildcard => {
                if scope.entries.is_empty() {
                    return Err(SqlError::TypeMismatch {
                        detail: "SELECT * requires a FROM clause".to_string(),
                    });
                }
                for entry in &scope.entries {
                    expand_wildcard(entry, &mut out, limits)?;
                }
            }
            ast::SelectItem::QualifiedWildcard(name) => {
                if name.0.len() != 1 {
                    return Err(SqlError::Unsupported {
                        detail: "schema-qualified wildcard (schema.table.*)".to_string(),
                    });
                }
                let entry = scope.find_by_qualifier(&name.0[0].value)?;
                expand_wildcard(entry, &mut out, limits)?;
            }
        }
    }
    if out.is_empty() {
        return Err(SqlError::TypeMismatch {
            detail: "SELECT must project at least one column".to_string(),
        });
    }
    Ok((out, aliases))
}

pub(super) fn expand_wildcard(
    entry: &ScopeEntry,
    out: &mut Vec<BoundSelectItem>,
    limits: &SqlLimits,
) -> Result<()> {
    // item 18: every column of an already-`Select`-authorized table is
    // itself authorized by construction (D25 v1 is table-level
    // granularity only) — no second per-column grant lookup here.
    for column in &entry.resolved.columns {
        let ty = crate::bind::ddl::relational_type_of(column)?;
        out.push(BoundSelectItem {
            expr: BoundExpr {
                kind: BoundExprKind::Column(ColumnRef {
                    table_ref: TableRefId(entry.table_ref_id),
                    table_id: entry.resolved.table_id,
                    ordinal: column.ordinal,
                }),
                ty: Some(ty),
                nullable: column.nullable || entry.null_extended,
            },
            output_name: column.name.clone(),
        });
        if out.len() > limits.max_columns {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "SELECT projection exceeds max_columns ({})",
                    limits.max_columns
                ),
            });
        }
    }
    Ok(())
}

pub(super) fn derive_output_name(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Column(col) => col.parts.last().expect("non-empty").value.clone(),
        ast::Expr::Function { name, .. } => name.last().value.clone(),
        ast::Expr::Aggregate { func, .. } => func.name().to_string(),
        ast::Expr::Window(call) => match &call.function {
            ast::WindowFunction::Aggregate { func, .. } => func.name().to_string(),
            ast::WindowFunction::Named { name, .. } => name.clone(),
        },
        ast::Expr::Grouping(_) => "grouping".to_string(),
        _ => "?column?".to_string(),
    }
}

// =======================================================================
// Aggregate binding support (Increment 11) — item 6/17/25.
// =======================================================================

/// `true` iff `expr` contains an (unextracted) aggregate call anywhere in
/// its tree — used only to decide `is_aggregated` (item 16); run before
/// `extract_aggregates`, so every aggregate call is still a
/// `BoundExprKind::Aggregate` node here, never yet an `AggregateRef`.
fn contains_aggregate(expr: &BoundExpr) -> bool {
    // `any` descends through every child of a node -- including, since
    // SQL-mission-2, the arguments, `PARTITION BY` and `ORDER BY` of a window
    // call (`SUM(SUM(x)) OVER ()` aggregates) -- but not into a subquery, whose
    // own aggregates belong to it.
    expr.any(&|e| matches!(e.kind, BoundExprKind::Aggregate(_)))
}

/// item 17/22: `expr` is legal in an aggregated statement's projection/
/// `HAVING`/`ORDER BY` iff every leaf is either inside an aggregate call
/// or part of a subtree that exactly (structurally) matches one of
/// `group_by`'s own bound expressions. A bare column reference
/// satisfying neither is rejected — never silently resolved against an
/// arbitrary row of its group (item 17's own core correctness rule).
fn validate_group_compat(
    expr: &BoundExpr,
    group_by: &[BoundExpr],
    own_refs: &std::collections::BTreeSet<u32>,
) -> Result<()> {
    if group_by.iter().any(|g| g == expr) {
        return Ok(());
    }
    match &expr.kind {
        // The aggregate's own argument is evaluated once per *input* row,
        // before grouping — not a group-dependent value at all, so it is
        // never checked against `group_by` here (nested aggregates are
        // already rejected earlier, at aggregate-binding time).
        BoundExprKind::Aggregate(_) | BoundExprKind::AggregateRef(_) => Ok(()),
        BoundExprKind::Literal(_) | BoundExprKind::Parameter { .. } => Ok(()),
        // SQL-mission-2: a window function runs over the *groups*, so
        // everything it evaluates per row (arguments, PARTITION BY, ORDER BY)
        // must itself be group-compatible. Its frame offsets are constants.
        BoundExprKind::Window(call) => {
            for e in call.exprs() {
                validate_group_compat(e, group_by, own_refs)?;
            }
            Ok(())
        }
        BoundExprKind::WindowRef(_)
        | BoundExprKind::GroupKeyRef(_)
        | BoundExprKind::Grouping(_) => Ok(()),
        // A column of an enclosing query is a constant for one evaluation of
        // this (sub)query, so it needs no grouping.
        BoundExprKind::Column(c) if !own_refs.contains(&c.table_ref.0) => Ok(()),
        BoundExprKind::Column(_) => Err(SqlError::TypeMismatch {
            detail:
                "column must appear in the GROUP BY clause or be used inside an aggregate function"
                    .to_string(),
        }),
        BoundExprKind::UnaryOp { expr, .. } => validate_group_compat(expr, group_by, own_refs),
        BoundExprKind::BinaryOp { left, right, .. } => {
            validate_group_compat(left, group_by, own_refs)?;
            validate_group_compat(right, group_by, own_refs)
        }
        BoundExprKind::IsNull { expr, .. } => validate_group_compat(expr, group_by, own_refs),
        BoundExprKind::Between {
            expr, low, high, ..
        } => {
            validate_group_compat(expr, group_by, own_refs)?;
            validate_group_compat(low, group_by, own_refs)?;
            validate_group_compat(high, group_by, own_refs)
        }
        BoundExprKind::InList { expr, list, .. } => {
            validate_group_compat(expr, group_by, own_refs)?;
            for item in list {
                validate_group_compat(item, group_by, own_refs)?;
            }
            Ok(())
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            validate_group_compat(expr, group_by, own_refs)?;
            validate_group_compat(pattern, group_by, own_refs)
        }
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => {
            if let Some(o) = operand {
                validate_group_compat(o, group_by, own_refs)?;
            }
            for (w, t) in branches {
                validate_group_compat(w, group_by, own_refs)?;
                validate_group_compat(t, group_by, own_refs)?;
            }
            if let Some(e) = else_result {
                validate_group_compat(e, group_by, own_refs)?;
            }
            Ok(())
        }
        BoundExprKind::Function { args, .. } => {
            for a in args {
                validate_group_compat(a, group_by, own_refs)?;
            }
            Ok(())
        }
        // An uncorrelated subquery is a constant per statement. A correlated
        // one is evaluated once per group, so every column of *this* query
        // it reads must be a grouping column (a column of a still-outer
        // query is constant and needs nothing). Its `IN` operand is an
        // expression of this query.
        BoundExprKind::Subquery(sq) => {
            if let Some(operand) = &sq.operand {
                validate_group_compat(operand, group_by, own_refs)?;
            }
            for c in &sq.outer_refs {
                if own_refs.contains(&c.table_ref.0)
                    && !group_by
                        .iter()
                        .any(|g| matches!(&g.kind, BoundExprKind::Column(gc) if gc == c))
                {
                    return Err(SqlError::TypeMismatch {
                        detail: "a column read by a correlated subquery must appear in the GROUP BY clause or be used inside an aggregate function".to_string(),
                    });
                }
            }
            Ok(())
        }
    }
}

/// item 25: rewrites every `BoundExprKind::Aggregate` node in `expr` to a
/// `BoundExprKind::AggregateRef(idx)` positional reference into `out`,
/// pushing a new entry only for a call not already present
/// (`BoundAggregateExpr`'s own `PartialEq` — two syntactically identical
/// aggregate calls in one statement, e.g. `SELECT SUM(x), SUM(x) + 1
/// ...`, share one physical aggregate-state slot rather than computing
/// the same accumulation twice). Never recurses into an `Aggregate`
/// node's own argument — that argument belongs to a different evaluation
/// phase entirely (per input row, before grouping), not this tree.
fn extract_aggregates(expr: BoundExpr, out: &mut Vec<BoundAggregateExpr>) -> BoundExpr {
    let BoundExpr { kind, ty, nullable } = expr;
    let kind = match kind {
        BoundExprKind::Aggregate(agg) => {
            let idx = match out.iter().position(|a| *a == agg) {
                Some(i) => i,
                None => {
                    out.push(agg);
                    out.len() - 1
                }
            };
            BoundExprKind::AggregateRef(idx)
        }
        already @ BoundExprKind::AggregateRef(_) => already,
        already @ (BoundExprKind::WindowRef(_)
        | BoundExprKind::GroupKeyRef(_)
        | BoundExprKind::Grouping(_)) => already,
        // SQL-mission-2: aggregates nested in a window call (`SUM(SUM(x))
        // OVER ()`, `RANK() OVER (ORDER BY SUM(x))`) belong to the query's
        // aggregation, so they are extracted here too.
        BoundExprKind::Window(call) => {
            BoundExprKind::Window(Box::new((*call).map_exprs(&mut |e| extract_aggregates(e, out))))
        }
        already @ BoundExprKind::Literal(_) => already,
        already @ BoundExprKind::Parameter { .. } => already,
        already @ BoundExprKind::Column(_) => already,
        BoundExprKind::UnaryOp { op, expr } => BoundExprKind::UnaryOp {
            op,
            expr: Box::new(extract_aggregates(*expr, out)),
        },
        BoundExprKind::BinaryOp { left, op, right } => BoundExprKind::BinaryOp {
            left: Box::new(extract_aggregates(*left, out)),
            op,
            right: Box::new(extract_aggregates(*right, out)),
        },
        BoundExprKind::IsNull { expr, negated } => BoundExprKind::IsNull {
            expr: Box::new(extract_aggregates(*expr, out)),
            negated,
        },
        BoundExprKind::Between {
            expr,
            negated,
            low,
            high,
        } => BoundExprKind::Between {
            expr: Box::new(extract_aggregates(*expr, out)),
            negated,
            low: Box::new(extract_aggregates(*low, out)),
            high: Box::new(extract_aggregates(*high, out)),
        },
        BoundExprKind::InList {
            expr,
            list,
            negated,
        } => BoundExprKind::InList {
            expr: Box::new(extract_aggregates(*expr, out)),
            list: list
                .into_iter()
                .map(|e| extract_aggregates(e, out))
                .collect(),
            negated,
        },
        BoundExprKind::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => BoundExprKind::Like {
            expr: Box::new(extract_aggregates(*expr, out)),
            pattern: Box::new(extract_aggregates(*pattern, out)),
            negated,
            case_insensitive,
        },
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => BoundExprKind::Case {
            operand: operand.map(|o| Box::new(extract_aggregates(*o, out))),
            branches: branches
                .into_iter()
                .map(|(w, t)| (extract_aggregates(w, out), extract_aggregates(t, out)))
                .collect(),
            else_result: else_result.map(|e| Box::new(extract_aggregates(*e, out))),
        },
        BoundExprKind::Function { name, args } => BoundExprKind::Function {
            name,
            args: args
                .into_iter()
                .map(|a| extract_aggregates(a, out))
                .collect(),
        },
        BoundExprKind::Subquery(mut sq) => {
            // The subquery's own tree was fully bound (aggregates already
            // extracted) in its own `bind_select_in`; only the `IN` operand
            // belongs to this query's aggregate namespace.
            sq.operand = sq.operand.take().map(|e| extract_aggregates(e, out));
            BoundExprKind::Subquery(sq)
        }
    };
    BoundExpr { kind, ty, nullable }
}

/// SQL-mission-2: rewrites every subtree equal to a grouping expression into a
/// read of the group's key (`GroupKeyRef`), so that an output row of a grouping
/// set that leaves the expression out sees `NULL` there. Marked nullable: the
/// value is `NULL` in the rows of any set that omits it (conservatively assumed
/// for every grouping expression of a ROLLUP / CUBE / GROUPING SETS query).
fn rewrite_group_keys(expr: BoundExpr, group_by: &[BoundExpr]) -> BoundExpr {
    if let Some(i) = group_by.iter().position(|g| *g == expr) {
        return BoundExpr {
            kind: BoundExprKind::GroupKeyRef(i),
            ty: expr.ty,
            nullable: true,
        };
    }
    expr.map_children(&mut |c| rewrite_group_keys(c, group_by))
}

/// SQL-mission-2: replaces each window function call by a `WindowRef` to its
/// slot in `out`. Identical calls (same function, arguments and window) share a
/// slot, so a call written twice is evaluated once. Window calls cannot nest
/// (the binder refuses that), so a `Window` node is replaced whole.
fn extract_windows(expr: BoundExpr, out: &mut Vec<BoundWindowCall>) -> BoundExpr {
    if let BoundExprKind::Window(call) = expr.kind {
        let call = *call;
        let idx = match out.iter().position(|c| *c == call) {
            Some(i) => i,
            None => {
                out.push(call);
                out.len() - 1
            }
        };
        return BoundExpr {
            kind: BoundExprKind::WindowRef(idx),
            ty: expr.ty,
            nullable: expr.nullable,
        };
    }
    expr.map_children(&mut |c| extract_windows(c, out))
}

/// `ORDER BY` items that name an item of the select list rather than an
/// expression over the `FROM` columns (SQL standard; PostgreSQL; Snowflake's
/// `ORDER BY` documents "a column alias appearing in the query block's SELECT
/// list" and "the position of an expression in the SELECT list"):
///
/// * an **unsigned integer literal** is a 1-based position in the select list
///   (after `*` expansion); one outside `1..=n` is a bind error, never a
///   constant sort key. A parameter (`$1`) and any other constant stay
///   ordinary expressions.
/// * a **bare identifier equal to an explicit select-list alias** is that
///   item, *even when a `FROM` column has the same name* (the standard's and
///   PostgreSQL's rule: output names win in a bare `ORDER BY` name). An alias
///   given to two items is ambiguous and refused.
///
/// `None` means "bind as an ordinary expression". Qualified names
/// (`t.total`) never name an alias.
fn resolve_output_reference(
    expr: &ast::Expr,
    projection: &[BoundSelectItem],
    aliases: &[(String, usize)],
) -> Result<Option<BoundExpr>> {
    match expr {
        ast::Expr::Literal(ast::Literal::Number { text, is_integer: true }) => {
            let position = text.parse::<i64>().ok();
            match position {
                Some(n) if n >= 1 && (n as u64) <= projection.len() as u64 => {
                    Ok(Some(projection[(n - 1) as usize].expr.clone()))
                }
                _ => Err(SqlError::InvalidIdentifier {
                    detail: format!(
                        "ORDER BY position {text} is not in the select list (it has {} item(s))",
                        projection.len()
                    ),
                }),
            }
        }
        ast::Expr::Column(c) if c.parts.len() == 1 => {
            let name = &c.parts[0].value;
            let mut hits = aliases.iter().filter(|(alias, _)| alias == name);
            let Some((_, idx)) = hits.next() else {
                return Ok(None);
            };
            if hits.next().is_some() {
                return Err(SqlError::InvalidIdentifier {
                    detail: format!("ORDER BY '{name}' is ambiguous: two select-list items have that alias"),
                });
            }
            Ok(Some(projection[*idx].expr.clone()))
        }
        _ => Ok(None),
    }
}

/// The grouping expressions of `GROUP BY ALL`: the select-list items (in order,
/// each distinct expression once) that contain no aggregate call and no window
/// function call. (`GROUPING(...)` in the select list never gets here: it names
/// expressions of a written `GROUP BY` list, and the select list is bound while
/// there is none, which refuses it.)
fn derive_group_by_all(projection: &[BoundSelectItem]) -> Vec<BoundExpr> {
    let is_window = |e: &BoundExpr| matches!(e.kind, BoundExprKind::Window(_));
    let mut out: Vec<BoundExpr> = Vec::new();
    for item in projection {
        // A constant is no grouping key (grouping by one would turn the global
        // aggregate of an empty input -- one row -- into zero groups).
        let constant = matches!(
            item.expr.kind,
            BoundExprKind::Literal(_) | BoundExprKind::Parameter { .. }
        );
        if constant || contains_aggregate(&item.expr) || item.expr.any(&is_window) {
            continue;
        }
        if !out.contains(&item.expr) {
            out.push(item.expr.clone());
        }
    }
    out
}
