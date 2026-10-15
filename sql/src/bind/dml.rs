//! `INSERT`/`UPDATE`/`DELETE` binding — item 26: bound, never executed,
//! but shaped to carry everything a future executor needs to perform
//! the real `TableStore`/index write without reparsing.

use std::collections::HashSet;

use rubixdb::catalog::schema::{IndexKind, IndexState, Privilege};
use rubixdb::catalog::CatalogService;
use rubixdb::relational::RelationalType;

use crate::ast;
use crate::auth::AuthContext;
use crate::bind::ddl::relational_type_of;
use crate::bind::env::BindEnv;
use crate::bind::expr::ExprBinder;
use crate::bind::scope::{self, BindContext};
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
    // `ON CONFLICT` reads the table to find the conflicting row, `DO UPDATE`
    // changes it and `RETURNING` hands rows back: each needs the privilege of
    // the matching read or write, so no clause is a way round a missing grant.
    // (A denial is the same existence-hiding `UnknownObject` as everywhere.)
    if stmt.on_conflict.is_some() || stmt.returning.is_some() {
        scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::Select)?;
    }
    if matches!(
        stmt.on_conflict.as_ref().map(|o| &o.action),
        Some(ast::ConflictAction::DoUpdate { .. })
    ) {
        scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::Update)?;
    }
    let env = BindEnv::new(catalog, ctx, auth, metrics);
    let mut with_scope = env.new_scope();
    let mut max_parameter = 0u32;
    // The WITH list is visible to an `INSERT ... SELECT`; a `VALUES` list cannot
    // contain a subquery, so for it the list can never be referenced -- it is
    // still bound (a bad or forbidden definition is an error).
    let ctes = crate::bind::select::bind_with_list(
        &env,
        limits,
        &stmt.ctes,
        &mut with_scope,
        None,
        &mut max_parameter,
    )?;
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

    let select = match &stmt.query {
        Some(q) => Some(Box::new(bind_insert_select(
            &env,
            limits,
            &with_scope,
            &resolved,
            &target_ordinals,
            q,
            &mut max_parameter,
        )?)),
        None => None,
    };
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

    let on_conflict = match &stmt.on_conflict {
        Some(oc) => Some(bind_on_conflict(
            catalog,
            &env,
            limits,
            &resolved,
            stmt.table_alias.as_ref(),
            oc,
            &mut max_parameter,
        )?),
        None => None,
    };
    let returning = match &stmt.returning {
        Some(items) => Some(bind_returning(
            &env,
            limits,
            &resolved,
            stmt.table_alias.as_ref(),
            items,
            &mut max_parameter,
        )?),
        None => None,
    };

    Ok(BoundInsert {
        ctes,
        database_id: resolved.database_id,
        schema_id: resolved.schema_id,
        table_id: resolved.table_id,
        rows: bound_rows,
        select,
        on_conflict,
        returning,
        defined_ctes: 0,
        max_parameter,
    })
}

/// The source query of an `INSERT ... SELECT`: bound as the ordinary query it is
/// (every table it reads is authorized with `SELECT`), then matched to the
/// target columns -- one query column per target column, each value either of
/// the column's type or one that widens to it without loss (the numeric widening
/// of `UNION`: `INTEGER` to `BIGINT` / `DECIMAL` / `DOUBLE`, ...; nothing else
/// converts, there being no `CAST`).
fn bind_insert_select(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    with_scope: &scope::Scope,
    resolved: &scope::ResolvedTable,
    target_ordinals: &[u16],
    q: &ast::Select,
    max_parameter: &mut u32,
) -> Result<BoundInsertSelect> {
    let query = crate::bind::select::bind_select_in_with(
        env,
        limits,
        q,
        None,
        &with_scope.ctes,
    )?;
    *max_parameter = (*max_parameter).max(query.max_parameter);
    if query.projection.len() != target_ordinals.len() {
        return Err(SqlError::TypeMismatch {
            detail: format!(
                "INSERT names {} target column(s) but the query returns {}",
                target_ordinals.len(),
                query.projection.len()
            ),
        });
    }
    let mut targets = Vec::with_capacity(target_ordinals.len());
    for (item, &ordinal) in query.projection.iter().zip(target_ordinals) {
        let column = resolved
            .columns
            .iter()
            .find(|c| c.ordinal == ordinal)
            .expect("ordinal came from this table's own column list");
        let want = relational_type_of(column)?;
        let widen_to = match item.expr.ty {
            // An untyped NULL column.
            None => {
                if !column.nullable && matches!(item.expr.kind, BoundExprKind::Literal(None)) {
                    return Err(SqlError::TypeMismatch {
                        detail: format!("column {:?} is NOT NULL", column.name),
                    });
                }
                None
            }
            Some(got) if got == want => None,
            Some(got) => {
                // Store assignment: lossless widening, plus BIGINT into INTEGER
                // with a per-value range check (a `SUM` or `COUNT` is a BIGINT;
                // with no `CAST` there would be no other way to store one in an
                // INTEGER column).
                let narrowing = got == RelationalType::Bigint && want == RelationalType::Integer;
                if narrowing || crate::bind::setop::unify_types(got, want) == Some(want) {
                    Some(want)
                } else {
                    return Err(SqlError::TypeMismatch {
                        detail: format!(
                            "column {:?} is {}, but the query gives {} for it (only lossless numeric widening, and BIGINT into INTEGER with a range check, convert)",
                            column.name,
                            crate::bind::setop::type_name(want),
                            crate::bind::setop::type_name(got)
                        ),
                    });
                }
            }
        };
        targets.push(InsertSelectTarget {
            ordinal,
            convert_to: widen_to,
        });
    }
    // The columns the statement does not name: their DEFAULT, or NULL when the
    // column is nullable; NOT NULL without a default cannot be filled.
    let mut defaults = Vec::new();
    for column in &resolved.columns {
        if target_ordinals.contains(&column.ordinal) {
            continue;
        }
        if !column.nullable && column.default_value.is_none() {
            return Err(SqlError::TypeMismatch {
                detail: format!(
                    "column {:?} has no value in this INSERT, is NOT NULL, and has no DEFAULT",
                    column.name
                ),
            });
        }
        let ty = relational_type_of(column)?;
        let value = match &column.default_value {
            Some(bytes) => {
                let (_, mut values) = rubixdb::relational::value::decode_row(bytes, &[ty])
                    .map_err(|e| SqlError::TypeMismatch {
                        detail: format!("column {:?} has a malformed DEFAULT: {e}", column.name),
                    })?;
                values.remove(0)
            }
            None => None,
        };
        defaults.push((column.ordinal, value));
    }
    Ok(BoundInsertSelect {
        query,
        plan: None,
        targets,
        defaults,
    })
}

fn same_columns(a: &[u16], b: &[u16]) -> bool {
    let (mut a, mut b) = (a.to_vec(), b.to_vec());
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

/// `ON CONFLICT [target] DO NOTHING | DO UPDATE SET ... [WHERE ...]`.
///
/// The target must name exactly the columns of the primary key or of a ready
/// unique index (in any order), or one of them by name (`ON CONSTRAINT`): a
/// conflict can only be recognized where the engine enforces uniqueness.
/// `DO UPDATE` requires a target (PostgreSQL); `DO NOTHING` without one
/// arbitrates on the primary key and every ready unique index.
///
/// `DO UPDATE` is bound against two relations of the same table: the existing
/// row, under the table's name (or its alias), and the proposed row, `EXCLUDED`.
/// A bare column name is therefore ambiguous there, as in PostgreSQL. No
/// subquery is accepted in it.
fn bind_on_conflict(
    catalog: &CatalogService,
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    resolved: &scope::ResolvedTable,
    alias: Option<&ast::Ident>,
    oc: &ast::OnConflict,
    max_parameter: &mut u32,
) -> Result<BoundOnConflict> {
    let indexes = catalog.list_indexes(resolved.table_id)?;
    let pk = &resolved.table.pk_ordinals;
    let pk_arbiter = BoundArbiter {
        index_id: None,
        ordinals: pk.clone(),
    };
    let unique_arbiter = |index: &rubixdb::catalog::schema::IndexRow| BoundArbiter {
        index_id: Some(index.index_id),
        ordinals: index.column_ordinals.clone(),
    };
    let ready_unique = |i: &&rubixdb::catalog::schema::IndexRow| {
        i.kind == IndexKind::Unique && i.state == IndexState::Ready
    };
    let no_match = || SqlError::TypeMismatch {
        detail: "there is no primary key or unique index matching the ON CONFLICT target"
            .to_string(),
    };

    let arbiters = match &oc.target {
        Some(ast::ConflictTarget::Columns(cols)) => {
            let mut ordinals = Vec::with_capacity(cols.len());
            for c in cols {
                let column = resolved
                    .columns
                    .iter()
                    .find(|col| col.name == c.value)
                    .ok_or_else(|| SqlError::UnknownObject {
                        kind: "column",
                        detail: c.value.clone(),
                    })?;
                if ordinals.contains(&column.ordinal) {
                    return Err(SqlError::InvalidIdentifier {
                        detail: format!("column {:?} appears twice in the ON CONFLICT target", c.value),
                    });
                }
                ordinals.push(column.ordinal);
            }
            if same_columns(&ordinals, pk) {
                vec![pk_arbiter]
            } else {
                let hit = indexes
                    .iter()
                    .filter(ready_unique)
                    .find(|i| same_columns(&i.column_ordinals, &ordinals))
                    .ok_or_else(no_match)?;
                vec![unique_arbiter(hit)]
            }
        }
        Some(ast::ConflictTarget::Constraint(name)) => {
            if let Some(index) = indexes.iter().find(|i| i.name == name.value) {
                match index.kind {
                    IndexKind::Primary => vec![pk_arbiter],
                    IndexKind::Unique if index.state == IndexState::Ready => {
                        vec![unique_arbiter(index)]
                    }
                    _ => return Err(no_match()),
                }
            } else {
                return Err(SqlError::InvalidIdentifier {
                    detail: format!("constraint {:?} does not exist on this table", name.value),
                });
            }
        }
        None => {
            if matches!(oc.action, ast::ConflictAction::DoUpdate { .. }) {
                return Err(SqlError::TypeMismatch {
                    detail: "ON CONFLICT DO UPDATE requires a conflict target: name the columns of the primary key or of a unique index"
                        .to_string(),
                });
            }
            let mut all = vec![pk_arbiter];
            all.extend(indexes.iter().filter(ready_unique).map(unique_arbiter));
            all
        }
    };

    let mut scope = env.new_scope();
    let existing_ref = scope.push(resolved.clone(), alias, false);
    let excluded_name = ast::Ident::new("excluded", false);
    let excluded_ref = scope.push(resolved.clone(), Some(&excluded_name), false);

    let action = match &oc.action {
        ast::ConflictAction::DoNothing => BoundConflictAction::DoNothing,
        ast::ConflictAction::DoUpdate {
            assignments,
            selection,
        } => {
            if assignments.is_empty() {
                return Err(SqlError::TypeMismatch {
                    detail: "ON CONFLICT DO UPDATE requires at least one SET assignment"
                        .to_string(),
                });
            }
            let pk_set: HashSet<u16> = pk.iter().copied().collect();
            let mut seen = HashSet::new();
            let mut bound = Vec::with_capacity(assignments.len());
            for a in assignments {
                if !seen.insert(a.column.value.clone()) {
                    return Err(SqlError::InvalidIdentifier {
                        detail: format!("duplicate SET target {:?}", a.column.value),
                    });
                }
                let column = resolved
                    .columns
                    .iter()
                    .find(|c| c.name == a.column.value)
                    .ok_or_else(|| SqlError::UnknownObject {
                        kind: "column",
                        detail: a.column.value.clone(),
                    })?;
                if pk_set.contains(&column.ordinal) {
                    return Err(SqlError::Unsupported {
                        detail: "UPDATE of a primary-key column is not supported in v1 (use DELETE + INSERT, D6)"
                            .to_string(),
                    });
                }
                let ty = relational_type_of(column)?;
                let mut eb = ExprBinder::new(Some(&scope), limits);
                let value = eb.bind(&a.value, Some(ty))?;
                *max_parameter = (*max_parameter).max(eb.max_parameter);
                if matches!(value.kind, BoundExprKind::Literal(None)) && !column.nullable {
                    return Err(SqlError::TypeMismatch {
                        detail: format!("column {:?} is NOT NULL", column.name),
                    });
                }
                bound.push(BoundAssignment {
                    ordinal: column.ordinal,
                    value,
                });
            }
            let selection = selection
                .as_ref()
                .map(|e| -> Result<BoundExpr> {
                    let mut eb = ExprBinder::new(Some(&scope), limits);
                    let b = eb.bind(e, Some(RelationalType::Boolean))?;
                    *max_parameter = (*max_parameter).max(eb.max_parameter);
                    Ok(b)
                })
                .transpose()?;
            BoundConflictAction::DoUpdate {
                assignments: bound,
                selection,
            }
        }
    };
    Ok(BoundOnConflict {
        arbiters,
        existing_ref,
        excluded_ref,
        action,
    })
}

/// `RETURNING <select list>`, bound against the target table alone (`EXCLUDED`
/// is not visible here). Aggregates, window functions and subqueries are not
/// accepted: the list is evaluated once for every changed row.
fn bind_returning(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    resolved: &scope::ResolvedTable,
    alias: Option<&ast::Ident>,
    items: &[ast::SelectItem],
    max_parameter: &mut u32,
) -> Result<BoundReturning> {
    let mut scope = env.new_scope();
    scope.push(resolved.clone(), alias, false);
    bind_returning_in(&scope, limits, items, max_parameter)
}

/// `RETURNING` bound in an existing single-table scope (`UPDATE` / `DELETE`).
fn bind_returning_in(
    scope: &scope::Scope,
    limits: &SqlLimits,
    items: &[ast::SelectItem],
    max_parameter: &mut u32,
) -> Result<BoundReturning> {
    let entry = &scope.entries[0];
    let mut out: Vec<BoundSelectItem> = Vec::new();
    for item in items {
        match item {
            ast::SelectItem::Item(i) => {
                let mut eb = ExprBinder::new(Some(scope), limits);
                let bound = eb.bind(&i.expr, None)?;
                *max_parameter = (*max_parameter).max(eb.max_parameter);
                let output_name = i
                    .alias
                    .as_ref()
                    .map(|a| a.value.clone())
                    .unwrap_or_else(|| crate::bind::select::derive_output_name(&i.expr));
                out.push(BoundSelectItem {
                    expr: bound,
                    output_name,
                });
            }
            ast::SelectItem::Wildcard => {
                crate::bind::select::expand_wildcard(entry, &mut out, limits)?;
            }
            ast::SelectItem::QualifiedWildcard(name) => {
                if name.0.len() != 1 {
                    return Err(SqlError::Unsupported {
                        detail: "schema-qualified wildcard (schema.table.*)".to_string(),
                    });
                }
                let e = scope.find_by_qualifier(&name.0[0].value)?;
                crate::bind::select::expand_wildcard(e, &mut out, limits)?;
            }
        }
        if out.len() > limits.max_columns {
            return Err(SqlError::ResourceLimit {
                detail: format!("RETURNING exceeds max_columns ({})", limits.max_columns),
            });
        }
    }
    if out.is_empty() {
        return Err(SqlError::TypeMismatch {
            detail: "RETURNING must list at least one column".to_string(),
        });
    }
    Ok(BoundReturning {
        table_ref: entry.table_ref_id,
        items: out,
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
    let env = BindEnv::new(catalog, ctx, auth, metrics);
    let resolved =
        scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::Update)?;
    let mut scope = env.new_scope();
    let mut max_parameter = 0u32;
    let ctes = crate::bind::select::bind_with_list(
        &env,
        limits,
        &stmt.ctes,
        &mut scope,
        None,
        &mut max_parameter,
    )?;
    if stmt.returning.is_some() {
        scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::Select)?;
    }
    scope.push(resolved, None, false);
    let table_columns = scope.entries[0].resolved.columns.clone();

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
            // A `SET` of a primary-key column is allowed (it changes the row's key; see
            // `exec::write::execute_update` and `Transaction::apply_row_updates`).
            let ty = relational_type_of(column)?;
            // A `SET` expression may contain a subquery (SQL-mission-1 Slice
            // B); it is evaluated against the row's pre-update values and the
            // pre-statement table state (`exec::write::execute_update`).
            let mut eb = ExprBinder::new(Some(&scope), limits).with_env(&env);
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
            let mut eb = ExprBinder::new(Some(&scope), limits).with_env(&env);
            let bound = eb.bind(e, Some(rubixdb::relational::RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            Ok(bound)
        })
        .transpose()?;

    let returning = stmt
        .returning
        .as_ref()
        .map(|items| bind_returning_in(&scope, limits, items, &mut max_parameter))
        .transpose()?;

    Ok(BoundUpdate {
        ctes,
        table: scope.entries[0].to_bound_table_ref(),
        assignments,
        selection,
        returning,
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
    let env = BindEnv::new(catalog, ctx, auth, metrics);
    let resolved =
        scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::Delete)?;
    let mut scope = env.new_scope();
    let mut max_parameter = 0u32;
    let ctes = crate::bind::select::bind_with_list(
        &env,
        limits,
        &stmt.ctes,
        &mut scope,
        None,
        &mut max_parameter,
    )?;
    if stmt.returning.is_some() {
        scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::Select)?;
    }
    scope.push(resolved, None, false);

    let selection = stmt
        .selection
        .as_ref()
        .map(|e| -> Result<BoundExpr> {
            let mut eb = ExprBinder::new(Some(&scope), limits).with_env(&env);
            let bound = eb.bind(e, Some(rubixdb::relational::RelationalType::Boolean))?;
            max_parameter = max_parameter.max(eb.max_parameter);
            Ok(bound)
        })
        .transpose()?;

    let returning = stmt
        .returning
        .as_ref()
        .map(|items| bind_returning_in(&scope, limits, items, &mut max_parameter))
        .transpose()?;

    Ok(BoundDelete {
        ctes,
        table: scope.entries[0].to_bound_table_ref(),
        selection,
        returning,
        max_parameter,
    })
}
