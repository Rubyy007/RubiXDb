//! Table/column resolution — D15's identifier resolution, bound together
//! with D25 authorization in the *same* pass (item 15/16): every
//! function here that resolves a table talks to `CatalogService`
//! directly and checks `crate::auth::is_authorized` before returning,
//! never separately, never via a second metadata structure.

use std::cell::Cell;
use std::rc::Rc;

use rubixdb::catalog::schema::{ColumnRow, ObjectKind, Privilege, TableRow, TableState};
use rubixdb::catalog::CatalogService;
use rubixdb::relational::value as rv;
use rubixdb::relational::RelationalType;

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

/// A schema of `database_id` by exact name (the schema a client selected earlier with `USE SCHEMA`).
pub fn resolve_schema_in_database(
    catalog: &CatalogService,
    database_id: u32,
    name: &str,
) -> Result<u32> {
    resolve_schema_by_name(catalog, database_id, name)
}

/// `USE SCHEMA [db.]schema`: the schema id and its stored name. Unqualified names resolve in the
/// context's database; a database qualifier must name an existing database. A missing schema is the
/// ordinary `UnknownObject` (HTTP 404).
pub fn resolve_use_schema(
    catalog: &CatalogService,
    ctx: &BindContext,
    name: &ast::ObjectName,
) -> Result<(u32, String)> {
    let (database_id, schema_name) = match name.0.len() {
        1 => (ctx.database_id, &name.0[0].value),
        2 => (
            resolve_database_by_name(catalog, &name.0[0].value)?,
            &name.0[1].value,
        ),
        n => {
            return Err(SqlError::InvalidIdentifier {
                detail: format!("USE SCHEMA name has {n} parts; use schema or default.schema"),
            })
        }
    };
    let schema_id = resolve_schema_by_name(catalog, database_id, schema_name)?;
    Ok((schema_id, schema_name.clone()))
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
pub(crate) fn resolve_schema_scope(
    catalog: &CatalogService,
    ctx: &BindContext,
    name: &ast::ObjectName,
) -> Result<(u32, u32)> {
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
            detail: format!(
                "table name has {n} qualification parts; at most 3 (db.schema.table) are supported"
            ),
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
    let ancestors = [
        (ObjectKind::Schema, schema_id),
        (ObjectKind::Database, database_id),
    ];
    if !crate::auth::is_authorized(catalog, auth, Privilege::Ddl, &ancestors)? {
        metrics.record_authorization_denial();
        return Err(unknown_schema(&format!(
            "schema for {}",
            display_object_name(name)
        )));
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
                detail: format!(
                    "schema name has {n} qualification parts; at most 2 (db.schema) are supported"
                ),
            })
        }
    };
    let ancestors = [(ObjectKind::Database, database_id)];
    if !crate::auth::is_authorized(catalog, auth, Privilege::Ddl, &ancestors)? {
        metrics.record_authorization_denial();
        return Err(unknown_database(&format!(
            "database for {}",
            display_object_name(name)
        )));
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

/// A CTE visible by name (SQL-mission-1 Slice C): its relation id and the
/// columns it exposes (names after any column-name list, types from the body).
#[derive(Debug, Clone)]
pub struct CteBinding {
    pub name: String,
    pub id: u32,
    pub columns: Vec<ColumnRow>,
}

/// The working table of a recursive CTE while its recursive term is being
/// bound (SQL-mission-1 Slice D): `FROM name` there means the rows of the
/// previous iteration, whatever the catalog holds under that name.
#[derive(Debug, Clone)]
pub struct WorkingBinding {
    pub name: String,
    pub work_id: u32,
    pub columns: Vec<ColumnRow>,
    /// The binder's nesting depth at which a reference is legal: directly in
    /// the `FROM` of the recursive term, not inside a subquery or a derived
    /// table.
    pub depth: usize,
    /// How many references to the working table have been bound.
    pub refs: Cell<u32>,
}

/// A resolved reference to a working table.
#[derive(Debug, Clone)]
pub struct WorkingRef {
    pub name: String,
    pub work_id: u32,
    pub columns: Vec<ColumnRow>,
    pub depth: usize,
    /// This reference's number: 1 for the first, 2 for a second (an error).
    pub ordinal: u32,
}

/// What a bare `FROM name` means when the CTE lists in scope are consulted.
#[derive(Debug, Clone)]
pub enum CteLookup {
    /// `name` is a CTE visible here.
    Found(CteBinding),
    /// `name` is the CTE currently being defined (its own body).
    SelfReference,
    /// `name` is a CTE defined later in the same `WITH` list.
    ForwardReference,
    /// Not a CTE name at all: resolve it against the catalog.
    NotACte,
    /// Slice D: the working table of the recursive CTE whose recursive term is
    /// being bound. Wins over a catalog table of the same name.
    Working(WorkingRef),
    /// Slice D: a CTE defined earlier in the same `WITH RECURSIVE` list, named
    /// inside a recursive term (visible in the seed, not in the term).
    HiddenInRecursiveTerm,
    /// Slice D: the CTE's own name inside its seed of a `WITH RECURSIVE` list.
    SelfInSeed,
    /// Slice D: the CTE's own name inside a body of a `WITH RECURSIVE` list
    /// that is not of the form `seed UNION [ALL] recursive-term`.
    SelfNotShaped,
    /// Slice D: a later CTE of a `WITH RECURSIVE` list (forward or mutual
    /// reference).
    ForwardInRecursiveList,
}

#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// CTEs defined by this query's own `WITH` list, in definition order
    /// (only the ones already bound while the list is being bound).
    pub ctes: Vec<CteBinding>,
    /// Names of the CTEs of this query's `WITH` list that are not defined
    /// yet (used to tell a forward reference from an unknown table).
    pub pending_cte_names: Vec<String>,
    /// The CTE whose body is being bound (its name is not visible there).
    pub defining_cte: Option<String>,
    /// Slice D: the list was written `WITH RECURSIVE`.
    pub recursive_keyword: bool,
    /// Slice D: the seed of the CTE `defining_cte` is being bound.
    pub seed_phase: bool,
    /// Slice D: the working table of the recursive term being bound.
    pub working: Option<WorkingBinding>,
    /// Slice D: names of the CTEs defined before the recursive one in the same
    /// list, which a recursive term must not name.
    pub hidden_ctes: Vec<String>,
    pub entries: Vec<ScopeEntry>,
    /// Source of `ScopeEntry::table_ref_id`. A standalone scope owns a
    /// private counter (ids 0, 1, ... in push order -- exactly the previous
    /// numbering); a scope created through `BindEnv::new_scope` shares the
    /// statement-wide counter, so ids are unique across a statement's
    /// subqueries (SQL-mission-1).
    ref_counter: Rc<Cell<u32>>,
}

impl Scope {
    pub fn with_counter(ref_counter: Rc<Cell<u32>>) -> Scope {
        Scope {
            ctes: Vec::new(),
            pending_cte_names: Vec::new(),
            defining_cte: None,
            recursive_keyword: false,
            seed_phase: false,
            working: None,
            hidden_ctes: Vec::new(),
            entries: Vec::new(),
            ref_counter,
        }
    }

    pub fn push(
        &mut self,
        resolved: ResolvedTable,
        alias: Option<&ast::Ident>,
        null_extended: bool,
    ) -> u32 {
        let table_ref_id = self.ref_counter.get();
        self.ref_counter.set(table_ref_id + 1);
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
        let mut matches = self
            .entries
            .iter()
            .filter(|e| e.effective_name == qualifier);
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
        match self.lookup_column(col_ref)? {
            Some(found) => Ok(found),
            None => Err(match col_ref.parts.len() {
                1 => SqlError::UnknownObject {
                    kind: "column",
                    detail: col_ref.parts[0].value.clone(),
                },
                _ => SqlError::UnknownObject {
                    kind: "table reference",
                    detail: col_ref.parts[0].value.clone(),
                },
            }),
        }
    }

    /// Resolves a column reference against *this* scope level only.
    ///
    /// `Ok(None)` means "this level does not define the name" -- an unqualified
    /// name no FROM item has, or a qualifier no FROM item is called -- which
    /// is the one case where a subquery's binder may go on to ask the next
    /// enclosing level (SQL-mission-1 correlation: the innermost binding wins).
    /// Anything else is a definitive answer for this level and must not fall
    /// through to an outer one: an ambiguous unqualified name, or a qualifier
    /// that names a FROM item here which lacks the column. Letting those
    /// slide outward would let an inner typo silently bind to an outer column.
    pub fn lookup_column(
        &self,
        col_ref: &ast::ColumnRef,
    ) -> Result<Option<(&ScopeEntry, &ColumnRow)>> {
        match col_ref.parts.len() {
            1 => {
                let name = &col_ref.parts[0].value;
                let mut matches = Vec::new();
                for entry in &self.entries {
                    if let Some(c) = find_in_entry(entry, name)? {
                        matches.push((entry, c));
                    }
                }
                match matches.len() {
                    0 => Ok(None),
                    1 => Ok(Some(matches[0])),
                    _ => Err(SqlError::AmbiguousColumn { detail: name.clone() }),
                }
            }
            2 => {
                let qualifier = &col_ref.parts[0].value;
                let name = &col_ref.parts[1].value;
                if !self.entries.iter().any(|e| &e.effective_name == qualifier) {
                    return Ok(None);
                }
                let entry = self.find_by_qualifier(qualifier)?;
                let column = find_in_entry(entry, name)?.ok_or_else(|| SqlError::UnknownObject {
                    kind: "column",
                    detail: format!("{qualifier}.{name}"),
                })?;
                Ok(Some((entry, column)))
            }
            n => Err(SqlError::InvalidIdentifier {
                detail: format!(
                    "column reference has {n} qualification parts; at most 2 (table.column) are supported"
                ),
            }),
        }
    }
}

/// The enclosing queries of the query being bound, innermost first
/// (SQL-mission-1 correlation). A subquery's column reference resolves
/// against its own `Scope` first; only when that level does not define the
/// name does the search move to `scope` here (the immediately enclosing
/// query), then to `parent`, and so on up to the statement's outermost
/// query. It is a linked chain of borrows -- each link lives on the binder's
/// stack for exactly as long as the nested query is being bound -- so
/// nothing is cloned and no flat "outer" slot can lose a level.
#[derive(Clone, Copy)]
pub struct OuterScopes<'a> {
    pub scope: &'a Scope,
    pub parent: Option<&'a OuterScopes<'a>>,
    /// `Some` marks a **boundary**: the relation bound *beneath* this link
    /// (a non-`LATERAL` derived table, or a CTE body) cannot read the columns
    /// of this scope or of anything above it. Such a name still resolves for
    /// the purpose of a clear error, never as a value; CTE *names* stay
    /// visible through a boundary.
    pub boundary: Option<Boundary<'a>>,
}

#[derive(Clone, Copy)]
pub struct Boundary<'a> {
    pub kind: BoundaryKind,
    pub name: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    Derived,
    Cte,
}

impl Boundary<'_> {
    fn refusal(&self, column: &str) -> SqlError {
        match self.kind {
            BoundaryKind::Derived => SqlError::InvalidIdentifier {
                detail: format!(
                    "derived table '{}' cannot reference outer column '{column}' without LATERAL",
                    self.name
                ),
            },
            BoundaryKind::Cte => SqlError::InvalidIdentifier {
                detail: format!(
                    "CTE '{}' cannot reference outer column '{column}'",
                    self.name
                ),
            },
        }
    }
}

impl<'a> OuterScopes<'a> {
    /// Number of enclosing queries in the chain.
    pub fn len(&self) -> usize {
        1 + self.parent.map_or(0, |p| p.len())
    }

    /// Always `false`: a chain has at least the enclosing query.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Resolves `col_ref` against the enclosing levels, innermost first.
    /// Returns the level index (0 = the immediately enclosing query) with the
    /// entry and column found there, or `None` if no level defines the name.
    pub fn lookup(
        &self,
        col_ref: &ast::ColumnRef,
    ) -> Result<Option<(usize, &'a ScopeEntry, &'a ColumnRow)>> {
        let mut level = 0usize;
        let mut link = Some(self);
        let mut blocked: Option<Boundary<'a>> = None;
        while let Some(l) = link {
            if blocked.is_none() {
                blocked = l.boundary;
            }
            if let Some((entry, column)) = l.scope.lookup_column(col_ref)? {
                return match blocked {
                    None => Ok(Some((level, entry, column))),
                    Some(b) => Err(b.refusal(&display_column(col_ref))),
                };
            }
            level += 1;
            link = l.parent;
        }
        Ok(None)
    }

    /// Classifies a bare relation name against every CTE list in scope: this
    /// query's own (passed as `scope`), then each enclosing query's, nearest
    /// first. A CTE found anywhere wins over the catalog; self- and
    /// forward-references are reported only when no CTE of that name is
    /// visible (so `WITH t AS (SELECT * FROM t)` over a real table `t` reads
    /// the table, exactly as the SQL standard's scoping rules say, while an
    /// unknown self-reference gets a precise message).
    pub fn lookup_cte(scope: &Scope, outer: Option<&OuterScopes<'_>>, name: &str) -> CteLookup {
        let mut scopes: Vec<&Scope> = vec![scope];
        let mut link = outer;
        while let Some(l) = link {
            scopes.push(l.scope);
            link = l.parent;
        }
        for s in &scopes {
            // The working table of a recursive term, if this scope's recursive
            // CTE has this name: structural, so it wins over everything else.
            // (A hit is counted: a second one is an error for the caller.)
            if let Some(w) = &s.working {
                if w.name == name {
                    let ordinal = w.refs.get() + 1;
                    w.refs.set(ordinal);
                    return CteLookup::Working(WorkingRef {
                        name: w.name.clone(),
                        work_id: w.work_id,
                        columns: w.columns.clone(),
                        depth: w.depth,
                        ordinal,
                    });
                }
            }
            if s.hidden_ctes.iter().any(|n| n == name) {
                return CteLookup::HiddenInRecursiveTerm;
            }
            // Latest first: an operand's own `WITH` shadows the CTEs it inherits
            // from the set operation around it (both live in one list).
            if let Some(c) = s.ctes.iter().rev().find(|c| c.name == name) {
                return CteLookup::Found(c.clone());
            }
        }
        if let Some(s) = scopes
            .iter()
            .find(|s| s.defining_cte.as_deref() == Some(name))
        {
            return if !s.recursive_keyword {
                CteLookup::SelfReference
            } else if s.seed_phase {
                CteLookup::SelfInSeed
            } else {
                CteLookup::SelfNotShaped
            };
        }
        if let Some(s) = scopes
            .iter()
            .find(|s| s.pending_cte_names.iter().any(|n| n == name))
        {
            return if s.recursive_keyword {
                CteLookup::ForwardInRecursiveList
            } else {
                CteLookup::ForwardReference
            };
        }
        CteLookup::NotACte
    }
}

fn display_column(col_ref: &ast::ColumnRef) -> String {
    col_ref
        .parts
        .iter()
        .map(|p| p.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// The column of `entry` called `name`. A derived table or CTE can expose two
/// columns with the same name (`SELECT a, a FROM t`); referring to such a name
/// is ambiguous. (A catalog table never has duplicate names, so this changes
/// nothing for one.)
fn find_in_entry<'e>(entry: &'e ScopeEntry, name: &str) -> Result<Option<&'e ColumnRow>> {
    let mut found = entry.resolved.columns.iter().filter(|c| c.name == name);
    let first = found.next();
    if first.is_some() && found.next().is_some() {
        return Err(SqlError::AmbiguousColumn {
            detail: format!("{}.{name}", entry.effective_name),
        });
    }
    Ok(first)
}

/// The type tag (and `DECIMAL` parameters) a relation column of type `ty`
/// carries -- the inverse of `bind::ddl::relational_type_of`.
fn tag_and_params(ty: RelationalType) -> (u8, Option<Vec<u8>>) {
    match ty {
        RelationalType::Boolean => (rv::TYPE_TAG_BOOLEAN, None),
        RelationalType::Integer => (rv::TYPE_TAG_INTEGER, None),
        RelationalType::Bigint => (rv::TYPE_TAG_BIGINT, None),
        RelationalType::Real => (rv::TYPE_TAG_REAL, None),
        RelationalType::Double => (rv::TYPE_TAG_DOUBLE, None),
        // The precision byte is never read back (`relational_type_of` takes
        // only the scale); 38 is the widest `DECIMAL` the engine encodes.
        RelationalType::Decimal { scale } => (rv::TYPE_TAG_DECIMAL, Some(vec![38, scale])),
        RelationalType::Text => (rv::TYPE_TAG_TEXT, None),
        RelationalType::Blob => (rv::TYPE_TAG_BLOB, None),
        RelationalType::Date => (rv::TYPE_TAG_DATE, None),
        RelationalType::Time => (rv::TYPE_TAG_TIME, None),
        RelationalType::Timestamp => (rv::TYPE_TAG_TIMESTAMP, None),
    }
}

/// The columns a derived table or CTE exposes, built from its body's output
/// (SQL-mission-1 Slice C). `names` are the output names (already replaced by
/// a column-name list if one was written); a column whose type cannot be
/// determined (a bare `NULL`) is refused rather than guessed.
pub fn relation_columns(
    relation: &str,
    columns: &[(String, Option<RelationalType>, bool)],
) -> Result<Vec<ColumnRow>> {
    columns
        .iter()
        .enumerate()
        .map(|(ordinal, (name, ty, nullable))| {
            let ty = ty.ok_or_else(|| SqlError::TypeMismatch {
                detail: format!(
                    "column '{name}' of '{relation}' has no determinable type (a bare NULL); \
                     select a typed expression instead"
                ),
            })?;
            let (data_type, type_params) = tag_and_params(ty);
            Ok(ColumnRow {
                table_id: crate::bound::SYNTHETIC_TABLE_ID,
                ordinal: ordinal as u16,
                name: name.clone(),
                data_type,
                nullable: *nullable,
                default_value: None,
                added_in_schema_version: 0,
                type_params,
                backfill_value: None,
            })
        })
        .collect()
}

/// A `ResolvedTable` for a relation that is not in the catalog (a derived
/// table or a CTE reference): no primary key, a sentinel table id.
pub fn synthetic_resolved(name: &str, columns: Vec<ColumnRow>) -> ResolvedTable {
    ResolvedTable {
        database_id: 0,
        schema_id: 0,
        table_id: crate::bound::SYNTHETIC_TABLE_ID,
        table: TableRow {
            table_id: crate::bound::SYNTHETIC_TABLE_ID,
            schema_id: 0,
            name: name.to_string(),
            pk_ordinals: Vec::new(),
            schema_version: 0,
            state: TableState::Active,
            created_at: 0,
        },
        columns,
    }
}
