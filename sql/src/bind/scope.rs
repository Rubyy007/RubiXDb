//! Table/column resolution — D15's identifier resolution, bound together
//! with D25 authorization in the *same* pass (item 15/16): every
//! function here that resolves a table talks to `CatalogService`
//! directly and checks `crate::auth::is_authorized` before returning,
//! never separately, never via a second metadata structure.

use rubixdb::catalog::schema::{ColumnRow, ObjectKind, Privilege, TableRow};
use rubixdb::catalog::CatalogService;

use crate::ast;
use crate::auth::AuthContext;
use crate::error::{Result, SqlError};

/// "Current database/schema" — item 17's minimum bar ("ensure
/// unqualified names resolve deterministically"), supplied by the
/// caller (a future session layer's job to populate; this crate invents
/// no `search_path` beyond this single default).
#[derive(Debug, Clone, Copy)]
pub struct BindContext {
    pub database_id: u32,
    pub default_schema_id: u32,
}

#[derive(Debug, Clone)]
pub struct ResolvedTable {
    pub database_id: u32,
    pub schema_id: u32,
    pub table_id: u32,
    pub table: TableRow,
    /// Ordinal-ordered (`CatalogService::get_columns` already sorts by
    /// ordinal).
    pub columns: Vec<ColumnRow>,
}

fn unknown_table(name: &ast::ObjectName) -> SqlError {
    SqlError::UnknownObject {
        kind: "table",
        detail: display_object_name(name),
    }
}

fn unknown_schema(name: &str) -> SqlError {
    SqlError::UnknownObject {
        kind: "schema",
        detail: name.to_string(),
    }
}

fn unknown_database(name: &str) -> SqlError {
    SqlError::UnknownObject {
        kind: "database",
        detail: name.to_string(),
    }
}

pub fn display_object_name(name: &ast::ObjectName) -> String {
    name.0
        .iter()
        .map(|i| i.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

fn resolve_schema_by_name(catalog: &CatalogService, database_id: u32, name: &str) -> Result<u32> {
    catalog
        .list_schemas(database_id)?
        .into_iter()
        .find(|s| s.name == name)
        .map(|s| s.schema_id)
        .ok_or_else(|| unknown_schema(name))
}

fn resolve_database_by_name(catalog: &CatalogService, name: &str) -> Result<u32> {
    catalog
        .list_databases()?
        .into_iter()
        .find(|d| d.name == name)
        .map(|d| d.database_id)
        .ok_or_else(|| unknown_database(name))
}

/// Splits a 1–3 part `ObjectName` into `(database_id, schema_id)`,
/// resolving each qualifying part against the catalog by name (never
/// assuming an ID from position — item 33: physical/catalog IDs are
/// never derived from anything but an authoritative catalog lookup).
fn resolve_schema_scope(catalog: &CatalogService, ctx: &BindContext, name: &ast::ObjectName) -> Result<(u32, u32)> {
    match name.0.len() {
        1 => Ok((ctx.database_id, ctx.default_schema_id)),
        2 => {
            let schema_id = resolve_schema_by_name(catalog, ctx.database_id, &name.0[0].value)?;
            Ok((ctx.database_id, schema_id))
        }
        3 => {
            let database_id = resolve_database_by_name(catalog, &name.0[0].value)?;
            let schema_id = resolve_schema_by_name(catalog, database_id, &name.0[1].value)?;
            Ok((database_id, schema_id))
        }
        n => Err(SqlError::InvalidIdentifier {
            detail: format!("table name has {n} qualification parts; at most 3 (db.schema.table) are supported"),
        }),
    }
}

/// Resolves and authorizes a table reference in one call — item 15's
/// "the same binder pass," item 32's "does not exist" and "exists but
/// forbidden" are structurally the *same* returned error here (both
/// `unknown_table`), never distinguishable by the caller.
pub fn resolve_table(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &crate::metrics::SqlMetrics,
    name: &ast::ObjectName,
    privilege: Privilege,
) -> Result<ResolvedTable> {
    let (database_id, schema_id) = resolve_schema_scope(catalog, ctx, name)?;
    let table_ident = name.last();
    let table = catalog
        .get_table_by_name(schema_id, &table_ident.value)?
        .ok_or_else(|| unknown_table(name))?;

    let ancestors = [
        (ObjectKind::Table, table.table_id),
        (ObjectKind::Schema, schema_id),
        (ObjectKind::Database, database_id),
    ];
    if !crate::auth::is_authorized(catalog, auth, privilege, &ancestors)? {
        metrics.record_authorization_denial();
        return Err(unknown_table(name));
    }

    let columns = catalog.get_columns(table.table_id)?;
    Ok(ResolvedTable {
        database_id,
        schema_id,
        table_id: table.table_id,
        table,
        columns,
    })
}

/// Resolves and authorizes the *schema* a `CREATE TABLE`/`CREATE INDEX`-
/// adjacent DDL statement targets — the object being created does not
/// exist yet, so the check is against its parent (D25: a schema-level
/// `Ddl` grant covers every table created within it).
pub fn resolve_schema_for_ddl(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &crate::metrics::SqlMetrics,
    name: &ast::ObjectName,
) -> Result<(u32, u32)> {
    // `resolve_schema_scope` treats `name`'s last part as "the object"
    // (not itself resolved there) and every earlier part as a schema/
    // database qualifier — exactly the shape a `CREATE TABLE`/`CREATE
    // SCHEMA`-target name has too, so it is reused directly rather than
    // duplicated: the object being created does not exist yet, so only
    // its *parent* scope is ever looked up here.
    let (database_id, schema_id) = resolve_schema_scope(catalog, ctx, name)?;
    let ancestors = [(ObjectKind::Schema, schema_id), (ObjectKind::Database, database_id)];
    if !crate::auth::is_authorized(catalog, auth, Privilege::Ddl, &ancestors)? {
        metrics.record_authorization_denial();
        return Err(unknown_schema(&format!("schema for {}", display_object_name(name))));
    }
    Ok((database_id, schema_id))
}

/// The `CREATE SCHEMA [db.]schema`-shaped counterpart: the object being
/// created is itself a schema, so its parent scope is only a database
/// (`[db.]` — 0 or 1 qualifying part), and the `Ddl` check is at the
/// database level.
pub fn resolve_database_for_ddl(
    catalog: &CatalogService,
    ctx: &BindContext,
    auth: &AuthContext,
    metrics: &crate::metrics::SqlMetrics,
    name: &ast::ObjectName,
) -> Result<u32> {
    let database_id = match name.0.len() {
        1 => ctx.database_id,
        2 => resolve_database_by_name(catalog, &name.0[0].value)?,
        n => {
            return Err(SqlError::InvalidIdentifier {
                detail: format!("schema name has {n} qualification parts; at most 2 (db.schema) are supported"),
            })
        }
    };
    let ancestors = [(ObjectKind::Database, database_id)];
    if !crate::auth::is_authorized(catalog, auth, Privilege::Ddl, &ancestors)? {
        metrics.record_authorization_denial();
        return Err(unknown_database(&format!("database for {}", display_object_name(name))));
    }
    Ok(database_id)
}

#[derive(Debug, Clone)]
pub struct ScopeEntry {
    pub table_ref_id: u32,
    pub resolved: ResolvedTable,
    pub effective_name: String,
    pub null_extended: bool,
}

impl ScopeEntry {
    pub fn to_bound_table_ref(&self) -> crate::bound::BoundTableRef {
        crate::bound::BoundTableRef {
            id: crate::bound::TableRefId(self.table_ref_id),
            database_id: self.resolved.database_id,
            schema_id: self.resolved.schema_id,
            table_id: self.resolved.table_id,
            effective_name: self.effective_name.clone(),
            null_extended: self.null_extended,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub entries: Vec<ScopeEntry>,
}

impl Scope {
    pub fn push(&mut self, resolved: ResolvedTable, alias: Option<&ast::Ident>, null_extended: bool) -> u32 {
        let table_ref_id = self.entries.len() as u32;
        let effective_name = alias
            .map(|a| a.value.clone())
            .unwrap_or_else(|| resolved.table.name.clone());
        self.entries.push(ScopeEntry {
            table_ref_id,
            resolved,
            effective_name,
            null_extended,
        });
        table_ref_id
    }

    /// A qualifier (`table.column`) match — exactly one scope entry's
    /// `effective_name` must equal `qualifier`.
    pub fn find_by_qualifier(&self, qualifier: &str) -> Result<&ScopeEntry> {
        let mut matches = self.entries.iter().filter(|e| e.effective_name == qualifier);
        let first = matches.next().ok_or_else(|| SqlError::UnknownObject {
            kind: "table reference",
            detail: qualifier.to_string(),
        })?;
        if matches.next().is_some() {
            // Only possible with a duplicate alias, itself rejected at
            // FROM-building time — defensive, not reachable in practice.
            return Err(SqlError::AmbiguousColumn {
                detail: format!("table qualifier {qualifier:?} matches more than one FROM item"),
            });
        }
        Ok(first)
    }

    pub fn resolve_column(&self, col_ref: &ast::ColumnRef) -> Result<(&ScopeEntry, &ColumnRow)> {
        match col_ref.parts.len() {
            1 => {
                let name = &col_ref.parts[0].value;
                let mut matches = Vec::new();
                for entry in &self.entries {
                    if let Some(c) = entry.resolved.columns.iter().find(|c| &c.name == name) {
                        matches.push((entry, c));
                    }
                }
                match matches.len() {
                    0 => Err(SqlError::UnknownObject {
                        kind: "column",
                        detail: name.clone(),
                    }),
                    1 => Ok(matches[0]),
                    _ => Err(SqlError::AmbiguousColumn { detail: name.clone() }),
                }
            }
            2 => {
                let qualifier = &col_ref.parts[0].value;
                let name = &col_ref.parts[1].value;
                let entry = self.find_by_qualifier(qualifier)?;
                let column = entry
                    .resolved
                    .columns
                    .iter()
                    .find(|c| &c.name == name)
                    .ok_or_else(|| SqlError::UnknownObject {
                        kind: "column",
                        detail: format!("{qualifier}.{name}"),
                    })?;
                Ok((entry, column))
            }
            n => Err(SqlError::InvalidIdentifier {
                detail: format!(
                    "column reference has {n} qualification parts; at most 2 (table.column) are supported"
                ),
            }),
        }
    }
}
