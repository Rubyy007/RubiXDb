//! `SELECT` binding — item 27/28: `FROM`/`JOIN` scope construction
//! (item 28's join binding, ambiguity resolved safely), wildcard
//! expansion (item 18, using real catalog column ordering, every
//! resulting column already authorized because its source table was),
//! `WHERE`/`ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT` (item 27) — no
//! execution.

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

    let projection = bind_projection(&scope, limits, &stmt.projection, &mut max_parameter)?;

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

    let order_by = stmt
        .order_by
        .iter()
        .map(|item| {
            let mut eb = ExprBinder::new(Some(&scope), limits);
            let bound = eb.bind(&item.expr, None)?;
            max_parameter = max_parameter.max(eb.max_parameter);
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
                let mut eb = ExprBinder::new(Some(scope), limits);
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
        _ => "?column?".to_string(),
    }
}
