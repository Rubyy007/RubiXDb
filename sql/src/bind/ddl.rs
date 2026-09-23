//! DDL binding — item 25: `CREATE DATABASE`/`CREATE SCHEMA`/`CREATE
//! TABLE`/`DROP TABLE`/`CREATE INDEX`/`DROP INDEX`, resolved and
//! authorized against the real `CatalogService`, never executed. Every
//! bound form is shaped to be the *exact* argument set the corresponding
//! `CatalogService`/`IndexBuilder` method already expects (item 25: "do
//! not bypass `CatalogService`") — a future executor increment should
//! need no further translation.

use std::collections::HashSet;

use rubixdb::catalog::schema::IndexKind;
use rubixdb::catalog::schema::{ColumnRow, Privilege};
use rubixdb::catalog::CatalogService;
use rubixdb::relational::value as rv;
use rubixdb::relational::RelationalType;

use crate::ast;
use crate::auth::AuthContext;
use crate::bind::expr::ExprBinder;
use crate::bind::scope::{self, BindContext};
use crate::bound::*;
use crate::error::{Result, SqlError};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;

pub fn sql_data_type_to_relational(dt: ast::SqlDataType) -> Result<RelationalType> {
    Ok(match dt {
        ast::SqlDataType::Boolean => RelationalType::Boolean,
        ast::SqlDataType::Integer => RelationalType::Integer,
        ast::SqlDataType::Bigint => RelationalType::Bigint,
        ast::SqlDataType::Real => RelationalType::Real,
        ast::SqlDataType::Double => RelationalType::Double,
        ast::SqlDataType::Decimal { scale, .. } => RelationalType::Decimal { scale },
        ast::SqlDataType::Text => RelationalType::Text,
        ast::SqlDataType::Blob => RelationalType::Blob,
        ast::SqlDataType::Date => RelationalType::Date,
        ast::SqlDataType::Time => RelationalType::Time,
        ast::SqlDataType::Timestamp => RelationalType::Timestamp,
    })
}

fn sql_data_type_tag_and_params(dt: ast::SqlDataType) -> (u8, Option<Vec<u8>>) {
    match dt {
        ast::SqlDataType::Boolean => (rv::TYPE_TAG_BOOLEAN, None),
        ast::SqlDataType::Integer => (rv::TYPE_TAG_INTEGER, None),
        ast::SqlDataType::Bigint => (rv::TYPE_TAG_BIGINT, None),
        ast::SqlDataType::Real => (rv::TYPE_TAG_REAL, None),
        ast::SqlDataType::Double => (rv::TYPE_TAG_DOUBLE, None),
        ast::SqlDataType::Decimal { precision, scale } => (rv::TYPE_TAG_DECIMAL, Some(vec![precision, scale])),
        ast::SqlDataType::Text => (rv::TYPE_TAG_TEXT, None),
        ast::SqlDataType::Blob => (rv::TYPE_TAG_BLOB, None),
        ast::SqlDataType::Date => (rv::TYPE_TAG_DATE, None),
        ast::SqlDataType::Time => (rv::TYPE_TAG_TIME, None),
        ast::SqlDataType::Timestamp => (rv::TYPE_TAG_TIMESTAMP, None),
    }
}

/// The exact inverse of `catalog::schema::ColumnRow`'s own `data_type`/
/// `type_params` fields — reimplemented here (not imported: `rubixdb::
/// relational::table_store::relational_type_from_column` is `pub(crate)`
/// to the `rubixdb` crate, not visible from this one) against the same
/// public `TYPE_TAG_*` constants, so both stay mechanically in sync with
/// D4's one closed type-tag set.
pub fn relational_type_of(column: &ColumnRow) -> Result<RelationalType> {
    Ok(match column.data_type {
        rv::TYPE_TAG_BOOLEAN => RelationalType::Boolean,
        rv::TYPE_TAG_INTEGER => RelationalType::Integer,
        rv::TYPE_TAG_BIGINT => RelationalType::Bigint,
        rv::TYPE_TAG_REAL => RelationalType::Real,
        rv::TYPE_TAG_DOUBLE => RelationalType::Double,
        rv::TYPE_TAG_DECIMAL => {
            let params = column.type_params.as_ref().ok_or_else(|| SqlError::Catalog(format!(
                "column {:?} is DECIMAL but has no type_params",
                column.name
            )))?;
            let &[_precision, scale] = params.as_slice() else {
                return Err(SqlError::Catalog(format!(
                    "column {:?}'s DECIMAL type_params must be exactly 2 bytes",
                    column.name
                )));
            };
            RelationalType::Decimal { scale }
        }
        rv::TYPE_TAG_TEXT => RelationalType::Text,
        rv::TYPE_TAG_BLOB => RelationalType::Blob,
        rv::TYPE_TAG_DATE => RelationalType::Date,
        rv::TYPE_TAG_TIME => RelationalType::Time,
        rv::TYPE_TAG_TIMESTAMP => RelationalType::Timestamp,
        other => {
            return Err(SqlError::Catalog(format!(
                "column {:?} has unknown data_type tag {other}",
                column.name
            )))
        }
    })
}

pub fn bind_create_database(
    _catalog: &CatalogService,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    stmt: &ast::CreateDatabase,
) -> Result<BoundCreateDatabase> {
    // No object exists above `Database` in D25's grant hierarchy — only
    // the default-Admin path can authorize this (D1: v1 has exactly one
    // database anyway; this remains forward grammar coverage, never
    // executed).
    if !matches!(auth.default_access, crate::auth::DefaultAccess::Admin) {
        metrics.record_authorization_denial();
        return Err(SqlError::AuthorizationDenied {
            detail: "CREATE DATABASE requires administrative access".to_string(),
        });
    }
    Ok(BoundCreateDatabase {
        name: stmt.name.value.clone(),
        if_not_exists: stmt.if_not_exists,
    })
}

pub fn bind_create_schema(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    stmt: &ast::CreateSchema,
) -> Result<BoundCreateSchema> {
    let database_id = scope::resolve_database_for_ddl(catalog, ctx, auth, metrics, &stmt.name)?;
    Ok(BoundCreateSchema {
        database_id,
        name: stmt.name.last().value.clone(),
        if_not_exists: stmt.if_not_exists,
    })
}

fn encode_default_literal(value: &rubixdb::relational::RelationalValue) -> Vec<u8> {
    // Reuses the existing single-row envelope (`RowValue`, D3) as the
    // storage format for one default value — no new encoding invented;
    // any future executor decodes it with `decode_row(bytes, &[ty])`.
    rubixdb::relational::value::encode_row(1, &[Some(value.clone())])
}

pub fn bind_create_table(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    limits: &SqlLimits,
    stmt: &ast::CreateTable,
) -> Result<BoundCreateTable> {
    let (database_id, schema_id) = scope::resolve_schema_for_ddl(catalog, ctx, auth, metrics, &stmt.name)?;

    if stmt.columns.is_empty() {
        return Err(SqlError::TypeMismatch {
            detail: "CREATE TABLE requires at least one column".to_string(),
        });
    }
    if stmt.columns.len() > limits.max_columns {
        return Err(SqlError::ResourceLimit {
            detail: format!("CREATE TABLE column count exceeds max_columns ({})", limits.max_columns),
        });
    }

    let mut seen_names = HashSet::new();
    let mut bound_columns = Vec::with_capacity(stmt.columns.len());
    let mut inline_pk_ordinals = Vec::new();
    for (ordinal, col) in stmt.columns.iter().enumerate() {
        if !seen_names.insert(col.name.value.clone()) {
            return Err(SqlError::InvalidIdentifier {
                detail: format!("duplicate column name {:?}", col.name.value),
            });
        }
        let ty = sql_data_type_to_relational(col.data_type)?;
        let (data_type, type_params) = sql_data_type_tag_and_params(col.data_type);
        let default_value = match &col.default {
            None => None,
            Some(expr) => {
                let mut binder = ExprBinder::new(None, limits);
                let bound = binder.bind(expr, Some(ty))?;
                match bound.kind {
                    BoundExprKind::Literal(Some(v)) => Some(encode_default_literal(&v)),
                    BoundExprKind::Literal(None) => {
                        return Err(SqlError::Unsupported {
                            detail: "DEFAULT NULL is not supported; omit DEFAULT on a nullable column instead"
                                .to_string(),
                        })
                    }
                    _ => {
                        return Err(SqlError::Unsupported {
                            detail: "DEFAULT must be a literal expression".to_string(),
                        })
                    }
                }
            }
        };
        if col.primary_key {
            inline_pk_ordinals.push(ordinal as u16);
        }
        bound_columns.push(BoundColumnDef {
            name: col.name.value.clone(),
            data_type,
            type_params,
            nullable: col.nullable,
            default_value,
        });
    }

    let pk_ordinals = if !inline_pk_ordinals.is_empty() {
        if stmt.table_primary_key.is_some() {
            return Err(SqlError::InvalidIdentifier {
                detail: "specify PRIMARY KEY either inline on one column or as a table-level constraint, not both"
                    .to_string(),
            });
        }
        if inline_pk_ordinals.len() > 1 {
            return Err(SqlError::InvalidIdentifier {
                detail: "only one column may have an inline PRIMARY KEY; use a table-level PRIMARY KEY(...) for a composite key"
                    .to_string(),
            });
        }
        inline_pk_ordinals
    } else if let Some(pk_cols) = &stmt.table_primary_key {
        if pk_cols.is_empty() {
            return Err(SqlError::InvalidIdentifier {
                detail: "PRIMARY KEY(...) requires at least one column".to_string(),
            });
        }
        pk_cols
            .iter()
            .map(|ident| {
                bound_columns
                    .iter()
                    .position(|c| c.name == ident.value)
                    .map(|i| i as u16)
                    .ok_or_else(|| SqlError::UnknownObject {
                        kind: "column",
                        detail: ident.value.clone(),
                    })
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        return Err(SqlError::TypeMismatch {
            detail: "CREATE TABLE requires a PRIMARY KEY".to_string(),
        });
    };

    Ok(BoundCreateTable {
        database_id,
        schema_id,
        name: stmt.name.last().value.clone(),
        columns: bound_columns,
        pk_ordinals,
        if_not_exists: stmt.if_not_exists,
    })
}

pub fn bind_drop_table(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    stmt: &ast::DropTable,
) -> Result<BoundDropTable> {
    match scope::resolve_table(catalog, ctx, auth, metrics, &stmt.name, Privilege::Ddl) {
        Ok(resolved) => Ok(BoundDropTable {
            table_id: Some(resolved.table_id),
            if_exists: stmt.if_exists,
        }),
        Err(SqlError::UnknownObject { .. }) if stmt.if_exists => Ok(BoundDropTable {
            table_id: None,
            if_exists: true,
        }),
        Err(e) => Err(e),
    }
}

pub fn bind_create_index(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    limits: &SqlLimits,
    stmt: &ast::CreateIndex,
) -> Result<BoundCreateIndex> {
    let resolved = scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::CreateIndex)?;
    if stmt.columns.is_empty() {
        return Err(SqlError::TypeMismatch {
            detail: "CREATE INDEX requires at least one column".to_string(),
        });
    }
    if stmt.columns.len() > limits.max_columns {
        return Err(SqlError::ResourceLimit {
            detail: format!("CREATE INDEX column count exceeds max_columns ({})", limits.max_columns),
        });
    }
    let column_ordinals = stmt
        .columns
        .iter()
        .map(|ident| {
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
        .collect::<Result<Vec<_>>>()?;
    let name = stmt
        .name
        .as_ref()
        .map(|n| n.value.clone())
        .unwrap_or_else(|| format!("{}_{}_idx", resolved.table.name, stmt.columns.iter().map(|c| c.value.as_str()).collect::<Vec<_>>().join("_")));
    Ok(BoundCreateIndex {
        table_id: resolved.table_id,
        name,
        kind: if stmt.unique { IndexKind::Unique } else { IndexKind::NonUnique },
        column_ordinals,
        if_not_exists: stmt.if_not_exists,
    })
}

pub fn bind_drop_index(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &SqlMetrics,
    stmt: &ast::DropIndex,
) -> Result<BoundDropIndex> {
    let resolved = match scope::resolve_table(catalog, ctx, auth, metrics, &stmt.table, Privilege::CreateIndex) {
        Ok(r) => r,
        Err(SqlError::UnknownObject { .. }) if stmt.if_exists => {
            return Ok(BoundDropIndex {
                index_id: None,
                if_exists: true,
            })
        }
        Err(e) => return Err(e),
    };
    let index = catalog
        .list_indexes(resolved.table_id)?
        .into_iter()
        .find(|i| i.name == stmt.name.value);
    match index {
        Some(idx) => Ok(BoundDropIndex {
            index_id: Some(idx.index_id),
            if_exists: stmt.if_exists,
        }),
        None if stmt.if_exists => Ok(BoundDropIndex {
            index_id: None,
            if_exists: true,
        }),
        None => Err(SqlError::UnknownObject {
            kind: "index",
            detail: stmt.name.value.clone(),
        }),
    }
}
