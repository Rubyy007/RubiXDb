//! `INSERT`/`UPDATE`/`DELETE` binding — item 26: bound, never executed,
//! but shaped to carry everything a future executor needs to perform
//! the real `TableStore`/index write without reparsing.

use std::collections::HashSet;

use rubixdb::catalog::schema::Privilege;
use rubixdb::catalog::CatalogService;

use crate::ast;
use crate::auth::AuthContext;
use crate::bind::ddl::relational_type_of;
use crate::bind::expr::ExprBinder;
use crate::bind::scope::{self, BindContext, Scope};
use crate::bound::*;
use crate::error::{Result, SqlError};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;

pub fn bind_insert(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    limits: &SqlLimits,
    stmt: &ast::Insert,
) -> Result<BoundInsert> {
    let resolved =
        scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::Insert)?;
    let column_count = resolved.columns.len();

    let target_ordinals: Vec<u16> = match &stmt.columns {
        Some(cols) => {
            let mut seen = HashSet::new();
            cols.iter()
                .map(|ident| {
                    if !seen.insert(ident.value.clone()) {
                        return Err(SqlError::InvalidIdentifier {
                            detail: format!(
                                "duplicate column {:?} in INSERT column list",
                                ident.value
                            ),
                        });
                    }
                    resolved
                        .columns
                        .iter()
                        .find(|c| c.name == ident.value)
                        .map(|c| c.ordinal)
                        .ok_or_else(|| SqlError::UnknownObject {
                            kind: "column",
                            detail: ident.value.clone(),
                        })
                })
                .collect::<Result<Vec<_>>>()?
        }
        None => (0..column_count as u16).collect(),
    };

    let mut max_parameter = 0u32;
    let mut bound_rows = Vec::with_capacity(stmt.rows.len());
    for row in &stmt.rows {
        if row.len() != target_ordinals.len() {
            return Err(SqlError::TypeMismatch {
                detail: format!(
                    "VALUES row has {} value(s), expected {} (matching the column list)",
                    row.len(),
                    target_ordinals.len()
                ),
            });
        }
        let mut full_row: Vec<Option<BoundExpr>> = vec![None; column_count];
        for (value_expr, &ordinal) in row.iter().zip(target_ordinals.iter()) {
            let column = resolved
                .columns
                .iter()
                .find(|c| c.ordinal == ordinal)
                .expect("ordinal came from this table's own column list");
            let ty = relational_type_of(column)?;
            let mut eb = ExprBinder::new(None, limits);
            let bound = eb.bind(value_expr, Some(ty))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            if matches!(bound.kind, BoundExprKind::Literal(None)) && !column.nullable {
                return Err(SqlError::TypeMismatch {
                    detail: format!("column {:?} is NOT NULL", column.name),
                });
            }
            full_row[ordinal as usize] = Some(bound);
        }
        for (i, slot) in full_row.iter_mut().enumerate() {
            if slot.is_none() {
                let column = &resolved.columns[i];
                if !column.nullable && column.default_value.is_none() {
                    return Err(SqlError::TypeMismatch {
                        detail: format!(
                            "column {:?} has no value in this INSERT, is NOT NULL, and has no DEFAULT",
                            column.name
                        ),
                    });
                }
                let ty = relational_type_of(column)?;
                // An *omitted* column with a declared `DEFAULT` must bind
                // to that default's own value here, at bind time -- not
                // to a `NULL` literal indistinguishable from a caller
                // explicitly writing `NULL` (standard SQL: `DEFAULT`
                // applies only when the column is omitted; an explicit
                // `NULL` on a nullable-with-a-default column, handled by
                // the loop above this one, must still bind to `NULL`).
                // `crate::bind::ddl::encode_default_literal`'s own doc
                // comment already named this exact decode step as "any
                // future executor['s]" job -- this is that job, done once
                // here rather than repeated by every future statement
                // kind that inserts a full row (`UPDATE` never needs
                // this: an omitted column in `SET` simply is not
                // reassigned at all, item 17/22 of the write-executor
                // spec).
                *slot = Some(match &column.default_value {
                    Some(bytes) => {
                        let (_, mut values) = rubixdb::relational::value::decode_row(bytes, &[ty])
                            .map_err(|e| SqlError::TypeMismatch {
                                detail: format!(
                                    "column {:?} has a malformed DEFAULT: {e}",
                                    column.name
                                ),
                            })?;
                        BoundExpr {
                            kind: BoundExprKind::Literal(values.remove(0)),
                            ty: Some(ty),
                            nullable: column.nullable,
                        }
                    }
                    None => BoundExpr {
                        kind: BoundExprKind::Literal(None),
                        ty: Some(ty),
                        nullable: true,
                    },
                });
            }
        }
        bound_rows.push(
            full_row
                .into_iter()
                .map(|o| o.expect("every slot filled above"))
                .collect(),
        );
    }

    Ok(BoundInsert {
        database_id: resolved.database_id,
        schema_id: resolved.schema_id,
        table_id: resolved.table_id,
        rows: bound_rows,
        max_parameter,
    })
}

pub fn bind_update(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    limits: &SqlLimits,
    stmt: &ast::Update,
) -> Result<BoundUpdate> {
    let resolved =
        scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::Update)?;
    let pk_ordinals: HashSet<u16> = resolved.table.pk_ordinals.iter().copied().collect();
    let mut scope = Scope::default();
    scope.push(resolved, None, false);
    let table_columns = scope.entries[0].resolved.columns.clone();

    let mut max_parameter = 0u32;
    let mut seen = HashSet::new();
    let assignments = stmt
        .assignments
        .iter()
        .map(|a| {
            if !seen.insert(a.column.value.clone()) {
                return Err(SqlError::InvalidIdentifier {
                    detail: format!("duplicate SET target {:?}", a.column.value),
                });
            }
            let column = table_columns
                .iter()
                .find(|c| c.name == a.column.value)
                .ok_or_else(|| SqlError::UnknownObject {
                    kind: "column",
                    detail: a.column.value.clone(),
                })?;
            if pk_ordinals.contains(&column.ordinal) {
                return Err(SqlError::Unsupported {
                    detail: "UPDATE of a primary-key column is not supported in v1 (use DELETE + INSERT, D6)"
                        .to_string(),
                });
            }
            let ty = relational_type_of(column)?;
            let mut eb = ExprBinder::new(Some(&scope), limits);
            let bound = eb.bind(&a.value, Some(ty))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            if matches!(bound.kind, BoundExprKind::Literal(None)) && !column.nullable {
                return Err(SqlError::TypeMismatch {
                    detail: format!("column {:?} is NOT NULL", column.name),
                });
            }
            Ok(BoundAssignment {
                ordinal: column.ordinal,
                value: bound,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if assignments.is_empty() {
        return Err(SqlError::TypeMismatch {
            detail: "UPDATE requires at least one SET assignment".to_string(),
        });
    }

    let selection = stmt
        .selection
        .as_ref()
        .map(|e| -> Result<BoundExpr> {
            let mut eb = ExprBinder::new(Some(&scope), limits);
            let bound = eb.bind(e, Some(rubixdb::relational::RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            Ok(bound)
        })
        .transpose()?;

    Ok(BoundUpdate {
        table: scope.entries[0].to_bound_table_ref(),
        assignments,
        selection,
        max_parameter,
    })
}

pub fn bind_delete(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    limits: &SqlLimits,
    stmt: &ast::Delete,
) -> Result<BoundDelete> {
    let resolved =
        scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::Delete)?;
    let mut scope = Scope::default();
    scope.push(resolved, None, false);

    let mut max_parameter = 0u32;
    let selection = stmt
        .selection
        .as_ref()
        .map(|e| -> Result<BoundExpr> {
            let mut eb = ExprBinder::new(Some(&scope), limits);
            let bound = eb.bind(e, Some(rubixdb::relational::RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            Ok(bound)
        })
        .transpose()?;

    Ok(BoundDelete {
        table: scope.entries[0].to_bound_table_ref(),
        selection,
        max_parameter,
    })
}
