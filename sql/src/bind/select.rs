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
use crate::bind::expr::ExprBinder;
use crate::bind::scope::{self, BindContext, Scope, ScopeEntry};
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
    let mut scope = Scope::default();
    let mut from = Vec::new();
    let mut max_parameter = 0u32;

    if let Some(from_clause) = &stmt.from {
        let resolved = scope::resolve_table(
            catalog,
            ctx,
            auth,
            metrics,
            &from_clause.first.name,
            Privilege::Select,
        )?;
        check_alias_not_duplicate(
            &scope,
            from_clause.first.alias.as_ref(),
            &resolved.table.name,
        )?;
        scope.push(resolved, from_clause.first.alias.as_ref(), false);
        from.push(BoundFromItem {
            table: scope
                .entries
                .last()
                .expect("just pushed")
                .to_bound_table_ref(),
            join: None,
        });

        for join in &from_clause.joins {
            let resolved = scope::resolve_table(
                catalog,
                ctx,
                auth,
                metrics,
                &join.table.name,
                Privilege::Select,
            )?;
            check_alias_not_duplicate(&scope, join.table.alias.as_ref(), &resolved.table.name)?;
            let null_extended = matches!(join.kind, ast::JoinKind::Left);
            scope.push(resolved, join.table.alias.as_ref(), null_extended);

            let mut eb = ExprBinder::new(Some(&scope), limits);
            let on = eb.bind(&join.on, Some(RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);

            from.push(BoundFromItem {
                table: scope
                    .entries
                    .last()
                    .expect("just pushed")
                    .to_bound_table_ref(),
                join: Some((join.kind, on)),
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
            let mut eb = ExprBinder::new(Some(&scope), limits);
            let bound = eb.bind(e, Some(RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            Ok(bound)
        })
        .transpose()?;

    // GROUP BY — plain (non-aggregate) expressions against the same
    // scope, item 12: no wildcard/aggregate allowed here.
    let group_by = stmt
        .group_by
        .iter()
        .map(|e| -> Result<BoundExpr> {
            let mut eb = ExprBinder::new(Some(&scope), limits);
            let bound = eb.bind(e, None)?;
            max_parameter = max_parameter.max(eb.max_parameter);
            Ok(bound)
        })
        .collect::<Result<Vec<_>>>()?;

    let projection = bind_projection(&scope, limits, &stmt.projection, &mut max_parameter)?;

    // HAVING — aggregate calls allowed (item 20).
    let having = stmt
        .having
        .as_ref()
        .map(|e| -> Result<BoundExpr> {
            let mut eb = ExprBinder::new(Some(&scope), limits).with_aggregate(true);
            let bound = eb.bind(e, Some(RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            Ok(bound)
        })
        .transpose()?;

    // item 16: a statement is "aggregated" iff it has a GROUP BY, a
    // HAVING clause, or at least one aggregate call in its projection —
    // exactly the three SQL-standard triggers for ungrouped-column
    // validation, never inferred from anything else.
    let is_aggregated = !group_by.is_empty()
        || having.is_some()
        || projection.iter().any(|item| contains_aggregate(&item.expr));

    if is_aggregated {
        // item 17: every projection/HAVING expression must be either
        // group-compatible or fully inside an aggregate call.
        for item in &projection {
            validate_group_compat(&item.expr, &group_by)?;
        }
        if let Some(h) = &having {
            validate_group_compat(h, &group_by)?;
        }
    }

    // ORDER BY — item 22: aggregate calls and grouping expressions
    // allowed only once the statement is already known to be aggregated;
    // validated against the same GROUP BY-compatibility rule.
    let order_by = stmt
        .order_by
        .iter()
        .map(|item| -> Result<BoundOrderByItem> {
            let mut eb = ExprBinder::new(Some(&scope), limits).with_aggregate(is_aggregated);
            let bound = eb.bind(&item.expr, None)?;
            max_parameter = max_parameter.max(eb.max_parameter);
            if is_aggregated {
                validate_group_compat(&bound, &group_by)?;
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
    let order_by: Vec<BoundOrderByItem> = order_by
        .into_iter()
        .map(|item| BoundOrderByItem {
            expr: extract_aggregates(item.expr, &mut aggregates),
            descending: item.descending,
            nulls: item.nulls,
        })
        .collect();

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
        distinct: stmt.distinct,
        from,
        projection,
        selection,
        group_by,
        aggregates,
        having,
        order_by,
        limit,
        offset,
        max_parameter,
    })
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

fn bind_projection(
    scope: &Scope,
    limits: &SqlLimits,
    items: &[ast::SelectItem],
    max_parameter: &mut u32,
) -> Result<Vec<BoundSelectItem>> {
    let mut out = Vec::new();
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
                let mut eb = ExprBinder::new(Some(scope), limits).with_aggregate(true);
                let bound = eb.bind(&item_expr.expr, None)?;
                *max_parameter = (*max_parameter).max(eb.max_parameter);
                let output_name = item_expr
                    .alias
                    .as_ref()
                    .map(|a| a.value.clone())
                    .unwrap_or_else(|| derive_output_name(&item_expr.expr));
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
    Ok(out)
}

fn expand_wildcard(
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

fn derive_output_name(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Column(col) => col.parts.last().expect("non-empty").value.clone(),
        ast::Expr::Function { name, .. } => name.last().value.clone(),
        ast::Expr::Aggregate { func, .. } => func.name().to_string(),
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
    match &expr.kind {
        BoundExprKind::Aggregate(_) => true,
        BoundExprKind::AggregateRef(_)
        | BoundExprKind::Literal(_)
        | BoundExprKind::Parameter { .. }
        | BoundExprKind::Column(_) => false,
        BoundExprKind::UnaryOp { expr, .. } => contains_aggregate(expr),
        BoundExprKind::BinaryOp { left, right, .. } => {
            contains_aggregate(left) || contains_aggregate(right)
        }
        BoundExprKind::IsNull { expr, .. } => contains_aggregate(expr),
        BoundExprKind::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        BoundExprKind::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            contains_aggregate(expr) || contains_aggregate(pattern)
        }
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || branches
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_result.as_deref().is_some_and(contains_aggregate)
        }
        BoundExprKind::Function { args, .. } => args.iter().any(contains_aggregate),
    }
}

/// item 17/22: `expr` is legal in an aggregated statement's projection/
/// `HAVING`/`ORDER BY` iff every leaf is either inside an aggregate call
/// or part of a subtree that exactly (structurally) matches one of
/// `group_by`'s own bound expressions. A bare column reference
/// satisfying neither is rejected — never silently resolved against an
/// arbitrary row of its group (item 17's own core correctness rule).
fn validate_group_compat(expr: &BoundExpr, group_by: &[BoundExpr]) -> Result<()> {
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
        BoundExprKind::Column(_) => Err(SqlError::TypeMismatch {
            detail:
                "column must appear in the GROUP BY clause or be used inside an aggregate function"
                    .to_string(),
        }),
        BoundExprKind::UnaryOp { expr, .. } => validate_group_compat(expr, group_by),
        BoundExprKind::BinaryOp { left, right, .. } => {
            validate_group_compat(left, group_by)?;
            validate_group_compat(right, group_by)
        }
        BoundExprKind::IsNull { expr, .. } => validate_group_compat(expr, group_by),
        BoundExprKind::Between {
            expr, low, high, ..
        } => {
            validate_group_compat(expr, group_by)?;
            validate_group_compat(low, group_by)?;
            validate_group_compat(high, group_by)
        }
        BoundExprKind::InList { expr, list, .. } => {
            validate_group_compat(expr, group_by)?;
            for item in list {
                validate_group_compat(item, group_by)?;
            }
            Ok(())
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            validate_group_compat(expr, group_by)?;
            validate_group_compat(pattern, group_by)
        }
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => {
            if let Some(o) = operand {
                validate_group_compat(o, group_by)?;
            }
            for (w, t) in branches {
                validate_group_compat(w, group_by)?;
                validate_group_compat(t, group_by)?;
            }
            if let Some(e) = else_result {
                validate_group_compat(e, group_by)?;
            }
            Ok(())
        }
        BoundExprKind::Function { args, .. } => {
            for a in args {
                validate_group_compat(a, group_by)?;
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
    };
    BoundExpr { kind, ty, nullable }
}
