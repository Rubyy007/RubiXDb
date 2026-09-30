//! Read-only catalog metadata routes — `PHASE_RELATIONAL_SQL_API_
//! ARCHITECTURE.md` §6 (item 57's own "the architecture's primary SQL
//! surface remains `POST /v1/sql`" — these exist *alongside* it, not
//! instead of it, because `system.*` catalog objects are deliberately
//! **not** queryable through ordinary SQL `SELECT` at all: `crate::
//! bind::scope::resolve_table` only ever resolves a name through
//! `CatalogService::get_table_by_name` against a schema's own *user*
//! tables — `system.databases`/`schemas`/`tables`/`columns`/`indexes`/
//! `grants` are never in that resolution space (item 15's own "prevent
//! generic SQL DML from directly mutating `system.*`" cuts both ways:
//! there is no SQL-level read path to them either). A CLI/frontend
//! metadata command therefore has no `POST /v1/sql` query it could send
//! for `\lt`/`\ls`/`\d`/`\di` — this is the "architecture genuinely
//! requires a dedicated endpoint" case item 57 itself anticipates,
//! proven by inspection here, not assumed.
//!
//! Every route below reuses the **identical** authorization boundary
//! `POST /v1/sql` does — `rubixdb_sql::auth::is_authorized` with
//! `Privilege::Select`, via the same `to_sql_auth_context` mapping —
//! never a second, competing metadata-specific access-control system
//! (item 105's "there must be ONE authorization boundary").

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::Json;
use rubixdb::catalog::schema::{ObjectKind, Privilege};
use serde::{Deserialize, Serialize};

use crate::auth::{to_sql_auth_context, Principal};
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct DatabaseInfo {
    pub database_id: u32,
    pub name: String,
}

/// `\l` — every database `CatalogService` actually knows about. Today
/// this is always exactly one row (`"default"`) — `CREATE DATABASE` has
/// no execution primitive (Increment 10's own already-documented
/// boundary, unchanged) — and this handler makes no attempt to disguise
/// that as a richer multi-database product than actually exists (item
/// 29/53/98's own explicit "do not fake it").
pub async fn databases(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<DatabaseInfo>>, ApiError> {
    // Ensures the catalog has been bootstrapped (lazily, on first use of
    // any SQL/catalog surface -- `SqlContext::bind_context`'s own doc
    // comment) even though `\l` itself needs none of the resolved ids.
    state.sql.bind_context()?;
    let rows = state
        .sql
        .catalog
        .list_databases()
        .map_err(rubixdb_sql::SqlError::from)?;
    Ok(Json(
        rows.into_iter()
            .map(|d| DatabaseInfo {
                database_id: d.database_id,
                name: d.name,
            })
            .collect(),
    ))
}

#[derive(Debug, Serialize)]
pub struct SchemaInfo {
    pub schema_id: u32,
    pub database_id: u32,
    pub name: String,
}

/// `\ls` — every schema in the one bootstrapped database (`CREATE
/// SCHEMA` *is* implemented, so this can genuinely return more than
/// just `"public"` once a caller has created one — never hardcoded,
/// item 54).
pub async fn schemas(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<SchemaInfo>>, ApiError> {
    let bind_context = state.sql.bind_context()?;
    let rows = state
        .sql
        .catalog
        .list_schemas(bind_context.database_id)
        .map_err(rubixdb_sql::SqlError::from)?;
    Ok(Json(
        rows.into_iter()
            .map(|s| SchemaInfo {
                schema_id: s.schema_id,
                database_id: s.database_id,
                name: s.name,
            })
            .collect(),
    ))
}

#[derive(Debug, Serialize)]
pub struct TableInfo {
    pub table_id: u32,
    pub schema_id: u32,
    pub name: String,
}

/// `\lt` — every table in the current schema this principal is
/// authorized to `SELECT` from (item 55: never includes `system.*`
/// catalog objects themselves — `CatalogService::list_tables` only ever
/// enumerates rows from `system.tables`, which never contains an entry
/// for itself or any other `system.*` object; those are compiled-in
/// constants, `catalog::schema::SYSTEM_TABLE_*`, never rows).
pub async fn tables(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Vec<TableInfo>>, ApiError> {
    let auth_ctx = to_sql_auth_context(&principal);
    let bind_context = state.sql.bind_context()?;
    let rows = state
        .sql
        .catalog
        .list_tables(bind_context.default_schema_id)
        .map_err(rubixdb_sql::SqlError::from)?;
    let mut out = Vec::with_capacity(rows.len());
    for t in rows {
        let authorized = rubixdb_sql::auth::is_authorized(
            &state.sql.catalog,
            &auth_ctx,
            Privilege::Select,
            &[
                (ObjectKind::Table, t.table_id),
                (ObjectKind::Schema, t.schema_id),
                (ObjectKind::Database, bind_context.database_id),
            ],
        )?;
        if authorized {
            out.push(TableInfo {
                table_id: t.table_id,
                schema_id: t.schema_id,
                name: t.name,
            });
        }
    }
    Ok(Json(out))
}

#[derive(Debug, Serialize)]
pub struct ColumnInfo {
    pub ordinal: u16,
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
}

#[derive(Debug, Serialize)]
pub struct TableDescription {
    pub table_id: u32,
    pub name: String,
    pub columns: Vec<ColumnInfo>,
}

fn data_type_name(tag: u8) -> &'static str {
    use rubixdb::relational::value::*;
    match tag {
        TYPE_TAG_BOOLEAN => "boolean",
        TYPE_TAG_INTEGER => "integer",
        TYPE_TAG_BIGINT => "bigint",
        TYPE_TAG_REAL => "real",
        TYPE_TAG_DOUBLE => "double",
        TYPE_TAG_DECIMAL => "decimal",
        TYPE_TAG_TEXT => "text",
        TYPE_TAG_BLOB => "blob",
        TYPE_TAG_DATE => "date",
        TYPE_TAG_TIME => "time",
        TYPE_TAG_TIMESTAMP => "timestamp",
        _ => "unknown",
    }
}

/// `\d table_name` — item 30/56: column name/type/nullable/primary key,
/// exactly what `CatalogService` actually stores; never fabricates
/// foreign keys, defaults display, or `CHECK` constraint text the
/// current architecture does not expose through this read path.
pub async fn describe_table(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
) -> Result<Json<TableDescription>, ApiError> {
    let bind_context = state.sql.bind_context()?;
    let table = state
        .sql
        .catalog
        .get_table_by_name(bind_context.default_schema_id, &name)
        .map_err(rubixdb_sql::SqlError::from)?
        .ok_or_else(not_found)?;

    let auth_ctx = to_sql_auth_context(&principal);
    let authorized = rubixdb_sql::auth::is_authorized(
        &state.sql.catalog,
        &auth_ctx,
        Privilege::Select,
        &[
            (ObjectKind::Table, table.table_id),
            (ObjectKind::Schema, table.schema_id),
            (ObjectKind::Database, bind_context.database_id),
        ],
    )?;
    if !authorized {
        // item 17: identical to "table not found" -- never reveals
        // existence to an unauthorized caller.
        return Err(not_found());
    }

    let pk_ordinals: std::collections::HashSet<u16> = table.pk_ordinals.iter().copied().collect();
    let columns = state
        .sql
        .catalog
        .get_columns(table.table_id)
        .map_err(rubixdb_sql::SqlError::from)?
        .into_iter()
        .map(|c| ColumnInfo {
            ordinal: c.ordinal,
            name: c.name,
            data_type: data_type_name(c.data_type).to_string(),
            nullable: c.nullable,
            primary_key: pk_ordinals.contains(&c.ordinal),
        })
        .collect();

    Ok(Json(TableDescription {
        table_id: table.table_id,
        name: table.name,
        columns,
    }))
}

#[derive(Debug, Serialize)]
pub struct IndexInfo {
    pub index_id: u32,
    pub table_id: u32,
    pub table_name: Option<String>,
    pub name: String,
    pub kind: &'static str,
    pub unique: bool,
    pub state: &'static str,
    pub columns: Vec<String>,
}

/// `\di` — real `system.indexes` rows for every table this principal
/// can see in the current schema (item 31: no fake rows; an index whose
/// owning table the principal cannot `SELECT` is simply omitted, the
/// same existence-hiding discipline `tables` above applies).
pub async fn indexes(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Vec<IndexInfo>>, ApiError> {
    let auth_ctx = to_sql_auth_context(&principal);
    let bind_context = state.sql.bind_context()?;
    let all_tables = state
        .sql
        .catalog
        .list_tables(bind_context.default_schema_id)
        .map_err(rubixdb_sql::SqlError::from)?;
    let mut out = Vec::new();
    for t in &all_tables {
        let authorized = rubixdb_sql::auth::is_authorized(
            &state.sql.catalog,
            &auth_ctx,
            Privilege::Select,
            &[
                (ObjectKind::Table, t.table_id),
                (ObjectKind::Schema, t.schema_id),
                (ObjectKind::Database, bind_context.database_id),
            ],
        )?;
        if !authorized {
            continue;
        }
        let columns = state
            .sql
            .catalog
            .get_columns(t.table_id)
            .map_err(rubixdb_sql::SqlError::from)?;
        let idx_rows = state
            .sql
            .catalog
            .list_indexes(t.table_id)
            .map_err(rubixdb_sql::SqlError::from)?;
        for idx in idx_rows {
            let column_names = idx
                .column_ordinals
                .iter()
                .map(|&ord| {
                    columns
                        .iter()
                        .find(|c| c.ordinal == ord)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| format!("<ordinal {ord}>"))
                })
                .collect();
            out.push(IndexInfo {
                index_id: idx.index_id,
                table_id: idx.table_id,
                table_name: Some(t.name.clone()),
                name: idx.name,
                kind: match idx.kind {
                    rubixdb::catalog::schema::IndexKind::Primary => "primary",
                    rubixdb::catalog::schema::IndexKind::Unique => "unique",
                    rubixdb::catalog::schema::IndexKind::NonUnique => "non_unique",
                },
                unique: matches!(
                    idx.kind,
                    rubixdb::catalog::schema::IndexKind::Primary
                        | rubixdb::catalog::schema::IndexKind::Unique
                ),
                state: match idx.state {
                    rubixdb::catalog::schema::IndexState::Ready => "ready",
                    rubixdb::catalog::schema::IndexState::Building => "building",
                    rubixdb::catalog::schema::IndexState::Failed => "failed",
                    rubixdb::catalog::schema::IndexState::Dropping => "dropping",
                },
                columns: column_names,
            });
        }
    }
    Ok(Json(out))
}

#[derive(Debug, Serialize)]
pub struct GrantInfo {
    pub object_kind: &'static str,
    pub object_id: u32,
    pub privilege: &'static str,
}

#[derive(Debug, Serialize)]
pub struct AuthzInfo {
    pub principal: String,
    pub role: &'static str,
    /// item 32: "safe authorization metadata... only what the current
    /// principal is authorized to see" — RubiXDB has no separate user/
    /// role directory (API keys are server-configured, `system.grants`
    /// rows are keyed by arbitrary principal-name strings), so this is
    /// honestly scoped to the *caller's own* grants, never a fabricated
    /// full user list (item 32's own "no fake rows" extended to `\du`).
    pub explicit_grants: Vec<GrantInfo>,
}

pub async fn authz(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<AuthzInfo>, ApiError> {
    let grants = state
        .sql
        .catalog
        .list_grants_for_principal(&principal.name)
        .map_err(rubixdb_sql::SqlError::from)?;
    Ok(Json(AuthzInfo {
        principal: principal.name.clone(),
        role: match principal.role {
            crate::config::Role::Admin => "admin",
            crate::config::Role::Reader => "reader",
        },
        explicit_grants: grants
            .into_iter()
            .map(|g| GrantInfo {
                object_kind: match g.object_kind {
                    ObjectKind::Database => "database",
                    ObjectKind::Schema => "schema",
                    ObjectKind::Table => "table",
                },
                object_id: g.object_id,
                privilege: match g.privilege {
                    Privilege::Select => "select",
                    Privilege::Insert => "insert",
                    Privilege::Update => "update",
                    Privilege::Delete => "delete",
                    Privilege::Ddl => "ddl",
                    Privilege::CreateIndex => "create_index",
                },
            })
            .collect(),
    }))
}

// =======================================================================
// Delete safety — Increment 14, Blocker 12. Every route below requires
// the caller to supply the object's *current* identity (name, plus its
// parent's current name where applicable) in the request body; the
// handler re-reads that identity fresh from the live catalog and
// rejects on any mismatch (wrong/partial/empty confirmation, or a
// stale UI that cached a since-deleted object's id — a fresh `get_*`
// against that id simply returns `NotFound`, which this handler never
// converts into a delete). This is the same authorization boundary
// (`Privilege::Ddl`, `is_authorized`) every SQL DDL statement already
// uses, and calls the exact same catalog/index-builder primitives
// `DROP TABLE`/`DROP INDEX` already execute (`sql/src/exec/write.rs`)
// — no new SQL grammar, no direct filesystem/index-file manipulation.
// =======================================================================

#[derive(Debug, Deserialize)]
pub struct ConfirmName {
    pub confirm_name: String,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmTable {
    pub schema_name: String,
    pub table_name: String,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmIndex {
    pub schema_name: String,
    pub table_name: String,
    pub index_name: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteResult {
    pub deleted: bool,
}

fn confirmation_mismatch() -> ApiError {
    ApiError::Validation(
        "confirmation does not match the object's current name -- refused".to_string(),
    )
}

/// `DELETE /v1/catalog/schemas/:schema_id` — exact-name confirmation.
/// Reuses `CatalogService::drop_schema` verbatim (already rejects a
/// non-empty schema rather than inventing `CASCADE`).
pub async fn delete_schema(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
    Path(schema_id): Path<u32>,
    Json(body): Json<ConfirmName>,
) -> Result<Json<DeleteResult>, ApiError> {
    let bind_context = state.sql.bind_context()?;
    let schema = state
        .sql
        .catalog
        .get_schema(schema_id)
        .map_err(rubixdb_sql::SqlError::from)?
        .ok_or_else(not_found)?;

    let auth_ctx = to_sql_auth_context(&principal);
    let authorized = rubixdb_sql::auth::is_authorized(
        &state.sql.catalog,
        &auth_ctx,
        Privilege::Ddl,
        &[
            (ObjectKind::Schema, schema_id),
            (ObjectKind::Database, bind_context.database_id),
        ],
    )?;
    if !authorized {
        return Err(not_found());
    }

    if body.confirm_name.is_empty() || body.confirm_name != schema.name {
        return Err(confirmation_mismatch());
    }

    state
        .sql
        .catalog
        .drop_schema(schema_id)
        .map_err(rubixdb_sql::SqlError::from)?;
    Ok(Json(DeleteResult { deleted: true }))
}

/// `DELETE /v1/catalog/tables/:table_id` — confirmation must name both
/// the table's current schema and its current table name (item: "table
/// delete requires explicit confirmation showing schema.table", never
/// the application name). Calls `CatalogService::drop_table` — the
/// identical primitive `DROP TABLE` already executes.
pub async fn delete_table(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
    Path(table_id): Path<u32>,
    Json(body): Json<ConfirmTable>,
) -> Result<Json<DeleteResult>, ApiError> {
    let bind_context = state.sql.bind_context()?;
    let table = state
        .sql
        .catalog
        .get_table(table_id)
        .map_err(rubixdb_sql::SqlError::from)?
        .ok_or_else(not_found)?;
    let schema = state
        .sql
        .catalog
        .get_schema(table.schema_id)
        .map_err(rubixdb_sql::SqlError::from)?
        .ok_or_else(not_found)?;

    let auth_ctx = to_sql_auth_context(&principal);
    let authorized = rubixdb_sql::auth::is_authorized(
        &state.sql.catalog,
        &auth_ctx,
        Privilege::Ddl,
        &[
            (ObjectKind::Table, table_id),
            (ObjectKind::Schema, table.schema_id),
            (ObjectKind::Database, bind_context.database_id),
        ],
    )?;
    if !authorized {
        return Err(not_found());
    }

    if body.schema_name.is_empty()
        || body.table_name.is_empty()
        || body.schema_name != schema.name
        || body.table_name != table.name
    {
        return Err(confirmation_mismatch());
    }

    state
        .sql
        .catalog
        .drop_table(table_id)
        .map_err(rubixdb_sql::SqlError::from)?;
    Ok(Json(DeleteResult { deleted: true }))
}

/// `DELETE /v1/catalog/indexes/:index_id` — confirmation must name the
/// index's current schema, table, and index name. Calls `IndexBuilder::
/// drop_index_online` — the identical certified online-drop protocol
/// `DROP INDEX` already executes (never `CatalogService::drop_index`
/// directly, which would skip the physical entry sweep).
pub async fn delete_index(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
    Path(index_id): Path<u32>,
    Json(body): Json<ConfirmIndex>,
) -> Result<Json<DeleteResult>, ApiError> {
    let bind_context = state.sql.bind_context()?;
    let index = state
        .sql
        .catalog
        .get_index(index_id)
        .map_err(rubixdb_sql::SqlError::from)?
        .ok_or_else(not_found)?;
    let table = state
        .sql
        .catalog
        .get_table(index.table_id)
        .map_err(rubixdb_sql::SqlError::from)?
        .ok_or_else(not_found)?;
    let schema = state
        .sql
        .catalog
        .get_schema(table.schema_id)
        .map_err(rubixdb_sql::SqlError::from)?
        .ok_or_else(not_found)?;

    let auth_ctx = to_sql_auth_context(&principal);
    let authorized = rubixdb_sql::auth::is_authorized(
        &state.sql.catalog,
        &auth_ctx,
        Privilege::Ddl,
        &[
            (ObjectKind::Table, table.table_id),
            (ObjectKind::Schema, table.schema_id),
            (ObjectKind::Database, bind_context.database_id),
        ],
    )?;
    if !authorized {
        return Err(not_found());
    }

    if body.schema_name.is_empty()
        || body.table_name.is_empty()
        || body.index_name.is_empty()
        || body.schema_name != schema.name
        || body.table_name != table.name
        || body.index_name != index.name
    {
        return Err(confirmation_mismatch());
    }

    state
        .sql
        .index_builder
        .drop_index_online(index_id)
        .map_err(rubixdb_sql::SqlError::from)?;
    Ok(Json(DeleteResult { deleted: true }))
}

fn not_found() -> ApiError {
    ApiError::Sql(rubixdb_sql::SqlError::UnknownObject {
        kind: "table",
        detail: String::new(),
    })
}
