//! `CatalogService` — `RELATIONAL ADR AMENDMENT 002`'s implementation of
//! D1's persistent catalog. Every mutation is one `LsmEngine::write_batch`
//! call (D9/D13); every read is an ordinary `get`/`range_scan` — no
//! separate catalog cache, no separate persistence mechanism, no second
//! WAL. `ddl_lock` serializes this process's own catalog-mutating calls
//! (CA.1) — a real, narrowly-scoped fix for the ID-collision/duplicate-
//! creation race `write_batch` alone cannot close without D10's
//! (not-yet-implemented) conflict detection; it is never held across a
//! read-only catalog query.

use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::catalog::encoding::{
    catalog_key, decode_row, encode_row, system_table_prefix_range, system_table_range,
    SYSTEM_TABLE_COLUMNS, SYSTEM_TABLE_CONSTRAINTS, SYSTEM_TABLE_COUNTERS, SYSTEM_TABLE_DATABASES,
    SYSTEM_TABLE_FOREIGN_KEYS, SYSTEM_TABLE_GRANTS, SYSTEM_TABLE_INDEXES, SYSTEM_TABLE_SCHEMAS,
    SYSTEM_TABLE_TABLES,
};
use crate::catalog::error::{CatalogError, Result};
use crate::catalog::schema::{
    ColumnRow, ConstraintKind, ConstraintRow, DatabaseRow, FkAction, ForeignKeyRow, GrantRow,
    IndexKind, IndexRow, IndexState, ObjectKind, Privilege, SchemaRow, TableRow, TableState,
    COLUMNS_SCHEMA, COLUMNS_V1_FIELDS, COLUMN_ROW_VERSION, CONSTRAINTS_SCHEMA, DATABASES_SCHEMA,
    FOREIGN_KEYS_SCHEMA, GRANTS_SCHEMA, INDEXES_SCHEMA, SCHEMAS_SCHEMA, TABLES_SCHEMA,
};
use crate::lsm::{LsmEngine, WriteOp};

/// A column definition supplied to `create_table` — the catalog-only
/// subset `RELATIONAL ADR AMENDMENT 002` CA.2 defines; `data_type` is
/// D4's raw type tag, stored verbatim and never interpreted here (CA.5).
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: u8,
    pub nullable: bool,
    pub default_value: Option<Vec<u8>>,
    /// `RELATIONAL ADR AMENDMENT 003` RA.4: `[precision:u8, scale:u8]`
    /// for `DECIMAL`/`NUMERIC`, `None` otherwise.
    pub type_params: Option<Vec<u8>>,
}

/// A foreign key to define (`CatalogService::create_foreign_key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewForeignKey {
    /// The referencing ("child") table.
    pub table_id: u32,
    pub name: String,
    pub child_ordinals: Vec<u16>,
    pub parent_table_id: u32,
    pub parent_ordinals: Vec<u16>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

/// A foreign key declared in `CREATE TABLE` (`create_table_with_foreign_keys`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTableForeignKey {
    pub name: String,
    pub child_ordinals: Vec<u16>,
    /// The referenced table; `None` = the table being created (a self-reference).
    pub parent_table_id: Option<u32>,
    pub parent_ordinals: Vec<u16>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

/// A `CHECK` constraint declared in `CREATE TABLE` (`create_table_with_foreign_keys`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTableCheck {
    pub name: String,
    /// The columns the expression reads (so a rename of one can be refused).
    pub column_ordinals: Vec<u16>,
    /// The expression's SQL text, as `ConstraintRow::check_expression` stores it.
    pub expression: String,
}

/// Longest `CHECK` expression text accepted.
pub const MAX_CHECK_EXPRESSION_BYTES: usize = 4096;

/// `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §14 (Resource limits) — bounds
/// on `CREATE INDEX` itself, enforced before any allocation/write, so an
/// adversarial or buggy caller cannot grow `system.indexes` or a single
/// index's key width without bound (item 22/36 of the governing
/// directive: "protect against... many concurrent builds... oversized
/// index metadata").
pub const MAX_INDEXES_PER_TABLE: usize = 64;
pub const MAX_COLUMNS_PER_INDEX: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CounterKind {
    Database = 1,
    Schema = 2,
    Table = 3,
    Index = 4,
    Constraint = 5,
    Grant = 6,
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

pub struct CatalogService {
    engine: Arc<LsmEngine>,
    ddl_lock: Mutex<()>,
    /// All foreign keys, rebuilt on demand after any change (`foreign_keys`). The
    /// foreign-key and `CHECK` caches are why a process must hold **exactly one**
    /// `CatalogService` per engine (the server builds one and shares it): a second
    /// instance writing constraints would leave this one's cache stale.
    fk_cache: RwLock<Option<Arc<Vec<ForeignKeyRow>>>>,
    /// Bumped, after the write is visible, by every foreign-key change.
    fk_generation: AtomicU64,
    /// Every `CHECK` constraint, rebuilt on demand after any change (`check_constraints`).
    check_cache: RwLock<Option<Arc<Vec<ConstraintRow>>>>,
    check_generation: AtomicU64,
}

impl CatalogService {
    pub fn new(engine: Arc<LsmEngine>) -> Self {
        CatalogService {
            engine,
            ddl_lock: Mutex::new(()),
            fk_cache: RwLock::new(None),
            fk_generation: AtomicU64::new(0),
            check_cache: RwLock::new(None),
            check_generation: AtomicU64::new(0),
        }
    }

    // -------------------------------------------------------------
    // ID allocation (CA.1) — always called with `ddl_lock` held.
    // -------------------------------------------------------------

    fn counter_key(kind: CounterKind) -> Vec<u8> {
        catalog_key(SYSTEM_TABLE_COUNTERS, &[kind as u8])
    }

    /// Reads the durable counter for `kind` and returns `(next_id,
    /// counter_write_op)` — the caller must include `counter_write_op`
    /// in the *same* `write_batch` as the object it names, so the
    /// counter's advance and the object's creation are atomic together
    /// (CA.1): a crash between them is impossible by construction, not
    /// merely unlikely.
    fn allocate_id(&self, kind: CounterKind) -> Result<(u32, WriteOp)> {
        let key = Self::counter_key(kind);
        let current = match self.engine.get(&key)? {
            Some(bytes) => u32::from_le_bytes(bytes.as_slice().try_into().map_err(|_| {
                CatalogError::InvalidInput {
                    detail: "corrupt ID counter value".to_string(),
                }
            })?),
            None => 0,
        };
        let next = current
            .checked_add(1)
            .ok_or_else(|| CatalogError::InvalidInput {
                detail: format!("{kind:?} ID space exhausted (u32::MAX reached)"),
            })?;
        Ok((
            next,
            WriteOp::Put {
                key,
                value: next.to_le_bytes().to_vec(),
            },
        ))
    }

    // -------------------------------------------------------------
    // Bootstrap (CA.3)
    // -------------------------------------------------------------

    /// Idempotent: a no-op (no `write_batch` call at all) if `system.
    /// databases` already has any row. On a genuinely empty catalog,
    /// creates the single v1 database (`"default"`) and its `"public"`
    /// schema in one atomic batch.
    pub fn bootstrap(&self) -> Result<()> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());

        let (start, end) = system_table_range(SYSTEM_TABLE_DATABASES);
        let already_bootstrapped = self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
            .next()
            .is_some();
        if already_bootstrapped {
            return Ok(());
        }

        let (database_id, database_counter_op) = self.allocate_id(CounterKind::Database)?;
        let (schema_id, schema_counter_op) = self.allocate_id(CounterKind::Schema)?;

        let database = DatabaseRow {
            database_id,
            name: "default".to_string(),
            created_at: now_micros(),
        };
        let schema = SchemaRow {
            schema_id,
            database_id,
            name: "public".to_string(),
            created_at: now_micros(),
        };

        let ops = vec![
            database_counter_op,
            schema_counter_op,
            WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_DATABASES, &database_id.to_be_bytes()),
                value: encode_row(1, &database.to_fields()),
            },
            WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_SCHEMAS, &schema_id.to_be_bytes()),
                value: encode_row(1, &schema.to_fields()),
            },
        ];
        self.engine.write_batch(&ops)?;
        Ok(())
    }

    // -------------------------------------------------------------
    // system.databases / system.schemas — read paths
    // -------------------------------------------------------------

    pub fn get_database(&self, database_id: u32) -> Result<Option<DatabaseRow>> {
        let key = catalog_key(SYSTEM_TABLE_DATABASES, &database_id.to_be_bytes());
        match self.engine.get(&key)? {
            None => Ok(None),
            Some(bytes) => {
                let (_, fields) = decode_row(&bytes, &DATABASES_SCHEMA)?;
                Ok(Some(DatabaseRow::from_fields(database_id, fields)?))
            }
        }
    }

    /// Full `system.databases` scan — v1 has exactly one live row
    /// (bootstrap, CA.3); this is still a real range scan, not a
    /// hardcoded assumption, so it remains correct if that ever changes.
    pub fn list_databases(&self) -> Result<Vec<DatabaseRow>> {
        let (start, end) = system_table_range(SYSTEM_TABLE_DATABASES);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let database_id = pk_tail_u32(&key)?;
            let (_, fields) = decode_row(&value, &DATABASES_SCHEMA)?;
            out.push(DatabaseRow::from_fields(database_id, fields)?);
        }
        Ok(out)
    }

    pub fn create_schema(&self, database_id: u32, name: &str) -> Result<u32> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        validate_name(name, "schema")?;

        if self.get_database(database_id)?.is_none() {
            return Err(CatalogError::NotFound {
                object: format!("database {database_id}"),
            });
        }
        if self
            .list_schemas(database_id)?
            .iter()
            .any(|s| s.name == name)
        {
            return Err(CatalogError::AlreadyExists {
                object: format!("schema {name:?}"),
            });
        }

        let (schema_id, counter_op) = self.allocate_id(CounterKind::Schema)?;
        let schema = SchemaRow {
            schema_id,
            database_id,
            name: name.to_string(),
            created_at: now_micros(),
        };
        let ops = vec![
            counter_op,
            WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_SCHEMAS, &schema_id.to_be_bytes()),
                value: encode_row(1, &schema.to_fields()),
            },
        ];
        self.engine.write_batch(&ops)?;
        Ok(schema_id)
    }

    pub fn get_schema(&self, schema_id: u32) -> Result<Option<SchemaRow>> {
        let key = catalog_key(SYSTEM_TABLE_SCHEMAS, &schema_id.to_be_bytes());
        match self.engine.get(&key)? {
            None => Ok(None),
            Some(bytes) => {
                let (_, fields) = decode_row(&bytes, &SCHEMAS_SCHEMA)?;
                Ok(Some(SchemaRow::from_fields(schema_id, fields)?))
            }
        }
    }

    /// `system.schemas`'s primary key is a surrogate `schema_id`, not
    /// `(database_id, schema_id)` (CA.2) — this is a full-table scan
    /// filtered in memory, not a narrow range scan. Deliberate, honest
    /// tradeoff: v1 has exactly one database, so this scan's real cost
    /// is already bounded by "every schema that exists," which is
    /// exactly what a schema listing needs to visit regardless of key
    /// layout.
    pub fn list_schemas(&self, database_id: u32) -> Result<Vec<SchemaRow>> {
        let (start, end) = system_table_range(SYSTEM_TABLE_SCHEMAS);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let schema_id = pk_tail_u32(&key)?;
            let (_, fields) = decode_row(&value, &SCHEMAS_SCHEMA)?;
            let schema = SchemaRow::from_fields(schema_id, fields)?;
            if schema.database_id == database_id {
                out.push(schema);
            }
        }
        Ok(out)
    }

    pub fn drop_schema(&self, schema_id: u32) -> Result<()> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.get_schema(schema_id)?.is_none() {
            return Err(CatalogError::NotFound {
                object: format!("schema {schema_id}"),
            });
        }
        if !self.list_tables(schema_id)?.is_empty() {
            return Err(CatalogError::InvalidInput {
                detail: format!("schema {schema_id} still has tables; drop them first"),
            });
        }
        let ops = vec![WriteOp::Delete {
            key: catalog_key(SYSTEM_TABLE_SCHEMAS, &schema_id.to_be_bytes()),
        }];
        self.engine.write_batch(&ops)?;
        Ok(())
    }

    // -------------------------------------------------------------
    // system.tables / system.columns / system.indexes
    // -------------------------------------------------------------

    /// D13's own `CREATE TABLE` example: one `system.tables` row, N
    /// `system.columns` rows, and one default `PRIMARY`-kind `system.
    /// indexes` row (catalog/introspection metadata only — D6 stores no
    /// separate physical PK structure), all in one atomic `write_batch`.
    pub fn create_table(
        &self,
        schema_id: u32,
        name: &str,
        columns: &[ColumnDef],
        pk_ordinals: &[u16],
    ) -> Result<u32> {
        self.create_table_with_foreign_keys(schema_id, name, columns, pk_ordinals, &[], &[])
    }

    /// `create_table`, declaring foreign keys and `CHECK` constraints in the same
    /// atomic batch: the table, its columns, its primary-key index and its
    /// constraints all become visible together or not at all.
    pub fn create_table_with_foreign_keys(
        &self,
        schema_id: u32,
        name: &str,
        columns: &[ColumnDef],
        pk_ordinals: &[u16],
        foreign_keys: &[NewTableForeignKey],
        checks: &[NewTableCheck],
    ) -> Result<u32> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        validate_name(name, "table")?;
        if columns.is_empty() {
            return Err(CatalogError::InvalidInput {
                detail: "a table must have at least one column".to_string(),
            });
        }
        if columns.len() > u16::MAX as usize {
            return Err(CatalogError::InvalidInput {
                detail: "too many columns".to_string(),
            });
        }
        if pk_ordinals.is_empty() {
            return Err(CatalogError::InvalidInput {
                detail: "a table must have a primary key".to_string(),
            });
        }
        for &ordinal in pk_ordinals {
            if ordinal as usize >= columns.len() {
                return Err(CatalogError::InvalidInput {
                    detail: format!(
                        "pk_ordinals references column {ordinal}, but only {} columns exist",
                        columns.len()
                    ),
                });
            }
            if columns[ordinal as usize].nullable {
                return Err(CatalogError::InvalidInput {
                    detail: format!(
                        "primary-key column {ordinal} ({:?}) must not be nullable",
                        columns[ordinal as usize].name
                    ),
                });
            }
        }
        {
            let mut seen = std::collections::HashSet::new();
            for c in columns {
                if !seen.insert(c.name.as_str()) {
                    return Err(CatalogError::InvalidInput {
                        detail: format!("duplicate column name {:?}", c.name),
                    });
                }
            }
        }

        if self.get_schema(schema_id)?.is_none() {
            return Err(CatalogError::NotFound {
                object: format!("schema {schema_id}"),
            });
        }
        if self.list_tables(schema_id)?.iter().any(|t| t.name == name) {
            return Err(CatalogError::AlreadyExists {
                object: format!("table {name:?}"),
            });
        }

        let (table_id, table_counter_op) = self.allocate_id(CounterKind::Table)?;
        let (index_id, index_counter_op) = self.allocate_id(CounterKind::Index)?;

        let created_at = now_micros();
        let table = TableRow {
            table_id,
            schema_id,
            name: name.to_string(),
            pk_ordinals: pk_ordinals.to_vec(),
            schema_version: 1,
            state: TableState::Active,
            created_at,
        };
        let pk_index = IndexRow {
            index_id,
            table_id,
            name: format!("{name}_pkey"),
            kind: IndexKind::Primary,
            column_ordinals: pk_ordinals.to_vec(),
            state: IndexState::Ready,
            created_at,
        };

        let mut ops = vec![
            table_counter_op,
            index_counter_op,
            WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_TABLES, &table_id.to_be_bytes()),
                value: encode_row(1, &table.to_fields()),
            },
            WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_INDEXES, &index_id.to_be_bytes()),
                value: encode_row(1, &pk_index.to_fields()),
            },
        ];
        for (ordinal, column) in columns.iter().enumerate() {
            let ordinal = ordinal as u16;
            let row = ColumnRow {
                table_id,
                ordinal,
                name: column.name.clone(),
                data_type: column.data_type,
                nullable: column.nullable,
                default_value: column.default_value.clone(),
                added_in_schema_version: 1,
                type_params: column.type_params.clone(),
                backfill_value: None,
            };
            ops.push(Self::column_put_op(&row));
        }

        // Foreign keys and CHECK constraints: checked against the shape the new table
        // will have, ids taken from one counter advance.
        if !foreign_keys.is_empty() || !checks.is_empty() {
            let child_shape = FkShape {
                table_id,
                name: name.to_string(),
                pk_ordinals: pk_ordinals.to_vec(),
                columns: columns
                    .iter()
                    .enumerate()
                    .map(|(ordinal, c)| ColumnRow {
                        table_id,
                        ordinal: ordinal as u16,
                        name: c.name.clone(),
                        data_type: c.data_type,
                        nullable: c.nullable,
                        default_value: c.default_value.clone(),
                        added_in_schema_version: 1,
                        type_params: c.type_params.clone(),
                        backfill_value: None,
                    })
                    .collect(),
                unique_indexes: Vec::new(),
            };
            let (first_id, _) = self.allocate_id(CounterKind::Constraint)?;
            let last_id = first_id
                .checked_add((foreign_keys.len() + checks.len()) as u32 - 1)
                .ok_or_else(|| CatalogError::InvalidInput {
                    detail: "Constraint ID space exhausted (u32::MAX reached)".to_string(),
                })?;
            let mut seen_names = std::collections::HashSet::new();
            for (i, check) in checks.iter().enumerate() {
                validate_name(&check.name, "constraint")?;
                validate_check_expression(&check.expression)?;
                if !seen_names.insert(check.name.as_str()) {
                    return Err(CatalogError::AlreadyExists {
                        object: format!("constraint {:?}", check.name),
                    });
                }
                if check
                    .column_ordinals
                    .iter()
                    .any(|&o| o as usize >= columns.len())
                {
                    return Err(CatalogError::InvalidInput {
                        detail: "a CHECK constraint names a column that does not exist".to_string(),
                    });
                }
                let row = ConstraintRow {
                    constraint_id: first_id + (foreign_keys.len() + i) as u32,
                    table_id,
                    name: check.name.clone(),
                    kind: ConstraintKind::Check,
                    column_ordinals: check.column_ordinals.clone(),
                    check_expression: Some(check.expression.clone()),
                    added_in_schema_version: 1,
                };
                ops.push(WriteOp::Put {
                    key: catalog_key(SYSTEM_TABLE_CONSTRAINTS, &row.constraint_id.to_be_bytes()),
                    value: encode_row(1, &row.to_fields()),
                });
            }
            for (i, fk) in foreign_keys.iter().enumerate() {
                validate_name(&fk.name, "constraint")?;
                if !seen_names.insert(fk.name.as_str()) {
                    return Err(CatalogError::AlreadyExists {
                        object: format!("constraint {:?}", fk.name),
                    });
                }
                let parent_shape = match fk.parent_table_id {
                    None => child_shape.clone(),
                    Some(id) => {
                        let parent = self.require_active_table(id)?;
                        self.fk_shape(&parent)?
                    }
                };
                let parent_index_id = resolve_foreign_key(
                    &FkDefinition {
                        child_ordinals: &fk.child_ordinals,
                        parent_ordinals: &fk.parent_ordinals,
                        on_delete: fk.on_delete,
                        on_update: fk.on_update,
                    },
                    &child_shape,
                    &parent_shape,
                )?;
                let row = ForeignKeyRow {
                    fk_id: first_id + i as u32,
                    table_id,
                    name: fk.name.clone(),
                    child_ordinals: fk.child_ordinals.clone(),
                    parent_table_id: fk.parent_table_id.unwrap_or(table_id),
                    parent_ordinals: fk.parent_ordinals.clone(),
                    parent_index_id,
                    on_delete: fk.on_delete,
                    on_update: fk.on_update,
                    created_at,
                };
                ops.push(WriteOp::Put {
                    key: catalog_key(SYSTEM_TABLE_FOREIGN_KEYS, &row.fk_id.to_be_bytes()),
                    value: encode_row(1, &row.to_fields()),
                });
            }
            ops.push(WriteOp::Put {
                key: Self::counter_key(CounterKind::Constraint),
                value: last_id.to_le_bytes().to_vec(),
            });
        }

        self.engine.write_batch(&ops)?;
        if !foreign_keys.is_empty() {
            self.invalidate_foreign_keys();
        }
        if !checks.is_empty() {
            self.invalidate_checks();
        }
        Ok(table_id)
    }

    pub fn get_table(&self, table_id: u32) -> Result<Option<TableRow>> {
        let key = catalog_key(SYSTEM_TABLE_TABLES, &table_id.to_be_bytes());
        match self.engine.get(&key)? {
            None => Ok(None),
            Some(bytes) => {
                let (_, fields) = decode_row(&bytes, &TABLES_SCHEMA)?;
                Ok(Some(TableRow::from_fields(table_id, fields)?))
            }
        }
    }

    pub fn get_table_by_name(&self, schema_id: u32, name: &str) -> Result<Option<TableRow>> {
        Ok(self
            .list_tables(schema_id)?
            .into_iter()
            .find(|t| t.name == name))
    }

    /// `system.tables`'s primary key is a surrogate `table_id` (CA.2) —
    /// a full-table scan filtered by `schema_id` in memory, the same
    /// honest tradeoff `list_schemas` documents.
    pub fn list_tables(&self, schema_id: u32) -> Result<Vec<TableRow>> {
        let (start, end) = system_table_range(SYSTEM_TABLE_TABLES);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let table_id = pk_tail_u32(&key)?;
            let (_, fields) = decode_row(&value, &TABLES_SCHEMA)?;
            let table = TableRow::from_fields(table_id, fields)?;
            if table.schema_id == schema_id {
                out.push(table);
            }
        }
        Ok(out)
    }

    /// `system.columns`'s primary key genuinely is `(table_id, ordinal)`
    /// (CA.2 — the one composite key this increment defines, safe
    /// because both components are fixed-width unsigned integers with
    /// no order-preserving-TEXT-in-composite-key problem, CA.2's own
    /// `system.grants` rationale) — a real, narrow, order-preserving
    /// range scan, not a full-table filter.
    pub fn get_columns(&self, table_id: u32) -> Result<Vec<ColumnRow>> {
        let (start, end) = system_table_prefix_range(SYSTEM_TABLE_COLUMNS, &table_id.to_be_bytes());
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let ordinal = column_pk_ordinal(&key)?;
            let (_, fields) = decode_column_row(&value)?;
            out.push(ColumnRow::from_fields(table_id, ordinal, fields)?);
        }
        out.sort_by_key(|c| c.ordinal);
        Ok(out)
    }

    /// `RELATIONAL ADR AMENDMENT 002` CA.4: this increment's own,
    /// explicitly-scoped-down slice of D13 — direct, atomic catalog-row
    /// removal (table + its columns + its indexes + its constraints), no
    /// `DROPPING`-marker/background-sweep phase, because no table-row
    /// storage exists yet for a sweep to have anything to act on.
    pub fn drop_table(&self, table_id: u32) -> Result<()> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.get_table(table_id)?.is_none() {
            return Err(CatalogError::NotFound {
                object: format!("table {table_id}"),
            });
        }

        // A table another table's foreign key references cannot be dropped out from
        // under it; the table's own foreign keys go with it.
        let foreign_keys = self.foreign_keys()?;
        if let Some(fk) = foreign_keys
            .iter()
            .find(|fk| fk.parent_table_id == table_id && fk.table_id != table_id)
        {
            return Err(CatalogError::InvalidInput {
                detail: format!(
                    "table {table_id} is referenced by foreign key {:?} of table {}; drop that constraint first",
                    fk.name, fk.table_id
                ),
            });
        }
        let mut ops = vec![WriteOp::Delete {
            key: catalog_key(SYSTEM_TABLE_TABLES, &table_id.to_be_bytes()),
        }];
        let owns_foreign_keys = foreign_keys.iter().any(|fk| fk.table_id == table_id);
        for fk in foreign_keys.iter().filter(|fk| fk.table_id == table_id) {
            ops.push(WriteOp::Delete {
                key: catalog_key(SYSTEM_TABLE_FOREIGN_KEYS, &fk.fk_id.to_be_bytes()),
            });
        }
        for column in self.get_columns(table_id)? {
            ops.push(WriteOp::Delete {
                key: catalog_key(SYSTEM_TABLE_COLUMNS, &column_pk(table_id, column.ordinal)),
            });
        }
        for index in self.list_indexes(table_id)? {
            ops.push(WriteOp::Delete {
                key: catalog_key(SYSTEM_TABLE_INDEXES, &index.index_id.to_be_bytes()),
            });
        }
        for constraint in self.list_constraints(table_id)? {
            ops.push(WriteOp::Delete {
                key: catalog_key(
                    SYSTEM_TABLE_CONSTRAINTS,
                    &constraint.constraint_id.to_be_bytes(),
                ),
            });
        }

        let owns_checks = self
            .check_constraints()?
            .iter()
            .any(|c| c.table_id == table_id);
        self.engine.write_batch(&ops)?;
        if owns_foreign_keys {
            self.invalidate_foreign_keys();
        }
        if owns_checks {
            self.invalidate_checks();
        }
        Ok(())
    }

    // -------------------------------------------------------------
    // system.indexes — non-PRIMARY (UNIQUE / NON_UNIQUE) creation
    // -------------------------------------------------------------

    /// `UNIQUE`/`NON_UNIQUE` secondary indexes (D7). Backfilling a non-
    /// empty table's existing rows is explicitly **not** implemented
    /// here — no table-row storage exists in this increment for there to
    /// be anything to backfill; `state` is recorded as `Building` and a
    /// future increment that adds table-row storage is responsible for
    /// actually running D7's bounded backfill and flipping it to
    /// `Active`.
    pub fn create_index(
        &self,
        table_id: u32,
        name: &str,
        kind: IndexKind,
        column_ordinals: &[u16],
    ) -> Result<u32> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        validate_name(name, "index")?;
        if kind == IndexKind::Primary {
            return Err(CatalogError::InvalidInput {
                detail: "PRIMARY-kind indexes are created only by create_table".to_string(),
            });
        }
        if column_ordinals.is_empty() {
            return Err(CatalogError::InvalidInput {
                detail: "an index must cover at least one column".to_string(),
            });
        }
        if column_ordinals.len() > MAX_COLUMNS_PER_INDEX {
            return Err(CatalogError::InvalidInput {
                detail: format!(
                    "an index may cover at most {MAX_COLUMNS_PER_INDEX} columns, got {}",
                    column_ordinals.len()
                ),
            });
        }

        let table = self
            .get_table(table_id)?
            .ok_or_else(|| CatalogError::NotFound {
                object: format!("table {table_id}"),
            })?;
        let column_count = self.get_columns(table.table_id)?.len();
        for &ordinal in column_ordinals {
            if ordinal as usize >= column_count {
                return Err(CatalogError::InvalidInput {
                    detail: format!("column ordinal {ordinal} does not exist on table {table_id}"),
                });
            }
        }
        let existing_indexes = self.list_indexes(table_id)?;
        if existing_indexes.iter().any(|i| i.name == name) {
            return Err(CatalogError::AlreadyExists {
                object: format!("index {name:?}"),
            });
        }
        if existing_indexes.len() >= MAX_INDEXES_PER_TABLE {
            return Err(CatalogError::InvalidInput {
                detail: format!(
                    "table {table_id} already has {MAX_INDEXES_PER_TABLE} indexes (the maximum)"
                ),
            });
        }

        let (index_id, counter_op) = self.allocate_id(CounterKind::Index)?;
        let row = IndexRow {
            index_id,
            table_id,
            name: name.to_string(),
            kind,
            column_ordinals: column_ordinals.to_vec(),
            state: IndexState::Building,
            created_at: now_micros(),
        };
        let ops = vec![
            counter_op,
            WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_INDEXES, &index_id.to_be_bytes()),
                value: encode_row(1, &row.to_fields()),
            },
        ];
        self.engine.write_batch(&ops)?;
        Ok(index_id)
    }

    pub fn get_index(&self, index_id: u32) -> Result<Option<IndexRow>> {
        let key = catalog_key(SYSTEM_TABLE_INDEXES, &index_id.to_be_bytes());
        match self.engine.get(&key)? {
            None => Ok(None),
            Some(bytes) => {
                let (_, fields) = decode_row(&bytes, &INDEXES_SCHEMA)?;
                Ok(Some(IndexRow::from_fields(index_id, fields)?))
            }
        }
    }

    /// Increment 17 (F-2): the index catalog row exactly as a reader
    /// pinned at `as_of_seq` sees it -- the catalog rows are ordinary
    /// MVCC-versioned engine keys, so the version visible at a
    /// transaction's snapshot is the authoritative answer to "what state
    /// was this index in when that snapshot was taken" (`Building` before
    /// the `Building -> Ready` promotion's own write, `Ready` after it).
    /// No separate "ready sequence" is stored: the promotion's engine
    /// sequence number *is* the version boundary, and it is durable and
    /// recovered exactly like every other catalog row.
    pub fn get_index_as_of(&self, index_id: u32, as_of_seq: u64) -> Result<Option<IndexRow>> {
        let key = catalog_key(SYSTEM_TABLE_INDEXES, &index_id.to_be_bytes());
        match self.engine.get_as_of(&key, as_of_seq)? {
            None => Ok(None),
            Some(bytes) => {
                let (_, fields) = decode_row(&bytes, &INDEXES_SCHEMA)?;
                Ok(Some(IndexRow::from_fields(index_id, fields)?))
            }
        }
    }

    /// Surrogate `index_id` PK (CA.2) — full-table scan filtered by
    /// `table_id`, the same documented tradeoff as `list_tables`.
    pub fn list_indexes(&self, table_id: u32) -> Result<Vec<IndexRow>> {
        let (start, end) = system_table_range(SYSTEM_TABLE_INDEXES);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let index_id = pk_tail_u32(&key)?;
            let (_, fields) = decode_row(&value, &INDEXES_SCHEMA)?;
            let index = IndexRow::from_fields(index_id, fields)?;
            if index.table_id == table_id {
                out.push(index);
            }
        }
        Ok(out)
    }

    pub fn drop_index(&self, index_id: u32) -> Result<()> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        let index = self
            .get_index(index_id)?
            .ok_or_else(|| CatalogError::NotFound {
                object: format!("index {index_id}"),
            })?;
        if index.kind == IndexKind::Primary {
            return Err(CatalogError::InvalidInput {
                detail: "the PRIMARY index cannot be dropped independently of its table"
                    .to_string(),
            });
        }
        self.refuse_if_foreign_key_uses_index(index_id)?;
        let ops = vec![WriteOp::Delete {
            key: catalog_key(SYSTEM_TABLE_INDEXES, &index_id.to_be_bytes()),
        }];
        self.engine.write_batch(&ops)?;
        Ok(())
    }

    // -------------------------------------------------------------
    // Index lifecycle state transitions (`PHASE_RELATIONAL_INDEX_
    // BACKFILL_ADR.md` §9). Each is a single durable `write_batch` Put
    // of the row with only `state` changed — every other field
    // (`column_ordinals`, `kind`, `name`, `created_at`) is preserved
    // verbatim. `IndexBuilder` (`src/relational/index.rs`) is the only
    // caller; it holds the affected table's epoch lock (write side) for
    // `create_index` (the initial `Building`-state row insert, T0) and
    // for `mark_index_dropping` specifically because *those two*
    // transitions change which indexes
    // `TableStore`'s write path must maintain — `mark_index_ready`/
    // `mark_index_failed` do not change the maintained-index set (a
    // `Building` index is already maintained; `Ready`/`Failed` doesn't
    // stop or start that), so they need no epoch-lock coordination, only
    // `ddl_lock`'s existing catalog-mutation serialization.
    // -------------------------------------------------------------

    fn transition_index_state(
        &self,
        index_id: u32,
        allowed_from: &[IndexState],
        to: IndexState,
    ) -> Result<IndexRow> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut row = self
            .get_index(index_id)?
            .ok_or_else(|| CatalogError::NotFound {
                object: format!("index {index_id}"),
            })?;
        if !allowed_from.contains(&row.state) {
            return Err(CatalogError::InvalidInput {
                detail: format!(
                    "index {index_id} is in state {:?}, cannot transition to {to:?} \
                     (allowed from {allowed_from:?})",
                    row.state
                ),
            });
        }
        row.state = to;
        let ops = vec![WriteOp::Put {
            key: catalog_key(SYSTEM_TABLE_INDEXES, &index_id.to_be_bytes()),
            value: encode_row(1, &row.to_fields()),
        }];
        self.engine.write_batch(&ops)?;
        Ok(row)
    }

    /// `Building` -> `Ready`: the durable, atomic activation boundary
    /// (`PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §7's T5). Only a
    /// `Ready` index is ever query-usable.
    pub fn mark_index_ready(&self, index_id: u32) -> Result<()> {
        self.transition_index_state(index_id, &[IndexState::Building], IndexState::Ready)?;
        Ok(())
    }

    /// `Building` -> `Failed`: a build was aborted (backfill error,
    /// process restart choosing "restart" over "resume" and finding the
    /// prior attempt unrecoverable, etc.) without ever reaching `Ready`.
    /// Terminal — never maintained, never query-usable.
    pub fn mark_index_failed(&self, index_id: u32) -> Result<()> {
        self.transition_index_state(index_id, &[IndexState::Building], IndexState::Failed)?;
        Ok(())
    }

    /// `Building`/`Ready`/`Failed` -> `Dropping`: `DROP INDEX`'s durable
    /// first phase (D13's `DROPPING`-table precedent, applied to
    /// indexes). The instant this commits, `TableStore`'s write path
    /// stops maintaining the index (its own `list_indexes` call will see
    /// `Dropping`, not `Building`/`Ready`) — the caller of this method is
    /// responsible for holding the table's epoch write-lock across this
    /// call so no writer's already-in-flight critical section can
    /// straddle the boundary (see `IndexBuilder::drop_index_online`).
    pub fn mark_index_dropping(&self, index_id: u32) -> Result<()> {
        let row = self
            .get_index(index_id)?
            .ok_or_else(|| CatalogError::NotFound {
                object: format!("index {index_id}"),
            })?;
        if row.kind == IndexKind::Primary {
            return Err(CatalogError::InvalidInput {
                detail: "the PRIMARY index cannot be dropped independently of its table"
                    .to_string(),
            });
        }
        self.refuse_if_foreign_key_uses_index(index_id)?;
        self.transition_index_state(
            index_id,
            &[IndexState::Building, IndexState::Ready, IndexState::Failed],
            IndexState::Dropping,
        )?;
        Ok(())
    }

    /// Final removal of a `Dropping` index's catalog row, once its
    /// physical entry sweep has completed — the D13-mirrored second
    /// phase. Idempotent from the caller's perspective (`NotFound` if
    /// already removed, e.g. by a concurrent/retried sweep after a
    /// crash) rather than requiring the caller to track completion
    /// separately.
    pub fn remove_index_row(&self, index_id: u32) -> Result<()> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        let row = self
            .get_index(index_id)?
            .ok_or_else(|| CatalogError::NotFound {
                object: format!("index {index_id}"),
            })?;
        if row.state != IndexState::Dropping {
            return Err(CatalogError::InvalidInput {
                detail: format!(
                    "index {index_id} is in state {:?}, expected Dropping before final removal",
                    row.state
                ),
            });
        }
        let ops = vec![WriteOp::Delete {
            key: catalog_key(SYSTEM_TABLE_INDEXES, &index_id.to_be_bytes()),
        }];
        self.engine.write_batch(&ops)?;
        Ok(())
    }

    /// Every `system.indexes` row currently in `state`, across every
    /// table — used by crash recovery (`IndexBuilder::recover_
    /// incomplete_builds`/`recover_incomplete_drops`) to find `Building`/
    /// `Dropping` indexes left behind by a process that died mid-build or
    /// mid-sweep. A full scan (surrogate `index_id` PK, CA.2's own
    /// documented tradeoff, same as `list_indexes`) — acceptable, since
    /// this only ever runs once at startup, never on a per-write path.
    pub fn list_indexes_in_state(&self, state: IndexState) -> Result<Vec<IndexRow>> {
        let (start, end) = system_table_range(SYSTEM_TABLE_INDEXES);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let index_id = pk_tail_u32(&key)?;
            let (_, fields) = decode_row(&value, &INDEXES_SCHEMA)?;
            let index = IndexRow::from_fields(index_id, fields)?;
            if index.state == state {
                out.push(index);
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------------
    // system.constraints
    // -------------------------------------------------------------

    pub fn create_constraint(
        &self,
        table_id: u32,
        name: &str,
        kind: ConstraintKind,
        column_ordinals: &[u16],
        check_expression: Option<String>,
    ) -> Result<u32> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        validate_name(name, "constraint")?;
        if self.get_table(table_id)?.is_none() {
            return Err(CatalogError::NotFound {
                object: format!("table {table_id}"),
            });
        }
        if kind == ConstraintKind::Check && check_expression.as_deref().unwrap_or("").is_empty() {
            return Err(CatalogError::InvalidInput {
                detail: "CHECK constraint requires a non-empty expression".to_string(),
            });
        }

        let (constraint_id, counter_op) = self.allocate_id(CounterKind::Constraint)?;
        let row = ConstraintRow {
            constraint_id,
            table_id,
            name: name.to_string(),
            kind,
            column_ordinals: column_ordinals.to_vec(),
            check_expression,
            added_in_schema_version: 1,
        };
        let ops = vec![
            counter_op,
            WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_CONSTRAINTS, &constraint_id.to_be_bytes()),
                value: encode_row(1, &row.to_fields()),
            },
        ];
        self.engine.write_batch(&ops)?;
        Ok(constraint_id)
    }

    pub fn list_constraints(&self, table_id: u32) -> Result<Vec<ConstraintRow>> {
        let (start, end) = system_table_range(SYSTEM_TABLE_CONSTRAINTS);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let constraint_id = pk_tail_u32(&key)?;
            let (_, fields) = decode_row(&value, &CONSTRAINTS_SCHEMA)?;
            let constraint = ConstraintRow::from_fields(constraint_id, fields)?;
            if constraint.table_id == table_id {
                out.push(constraint);
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------------
    // ALTER TABLE primitives (ADR-DDL-FK-01). Each is one `write_batch`
    // under `ddl_lock` that rewrites the affected `system.columns` row
    // **and** the `system.tables` row with its `schema_version` bumped, so
    // the change and the new version become visible together or not at all.
    // Callers that must also coordinate with in-flight row writes (every one
    // that validates existing rows) hold the table's epoch write-lock around
    // the call -- see `relational::alter`.
    // -------------------------------------------------------------

    fn alter_table_row_op(table: &TableRow) -> (TableRow, WriteOp) {
        let mut next = table.clone();
        next.schema_version = table.schema_version.saturating_add(1);
        let op = WriteOp::Put {
            key: catalog_key(SYSTEM_TABLE_TABLES, &next.table_id.to_be_bytes()),
            value: encode_row(1, &next.to_fields()),
        };
        (next, op)
    }

    fn column_put_op(row: &ColumnRow) -> WriteOp {
        WriteOp::Put {
            key: catalog_key(SYSTEM_TABLE_COLUMNS, &column_pk(row.table_id, row.ordinal)),
            value: encode_row(COLUMN_ROW_VERSION, &row.to_fields()),
        }
    }

    fn require_active_table(&self, table_id: u32) -> Result<TableRow> {
        let table = self
            .get_table(table_id)?
            .ok_or_else(|| CatalogError::NotFound {
                object: format!("table {table_id}"),
            })?;
        if table.state != TableState::Active {
            return Err(CatalogError::InvalidInput {
                detail: format!("table {table_id} is being dropped"),
            });
        }
        Ok(table)
    }

    /// `ALTER TABLE ... ADD COLUMN`: appends a column at the next ordinal (every
    /// existing ordinal -- and so every primary-key and index definition -- is
    /// unchanged) and bumps the table's `schema_version`. `backfill_value` is the
    /// value rows written before this call read as for the new column (its
    /// literal `DEFAULT`, or `None` for `NULL`). Returns the new ordinal.
    pub fn add_column(
        &self,
        table_id: u32,
        column: &ColumnDef,
        backfill_value: Option<Vec<u8>>,
    ) -> Result<u16> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        validate_name(&column.name, "column")?;
        let table = self.require_active_table(table_id)?;
        let columns = self.get_columns(table_id)?;
        if columns.iter().any(|c| c.name == column.name) {
            return Err(CatalogError::AlreadyExists {
                object: format!("column {:?}", column.name),
            });
        }
        if columns.len() >= u16::MAX as usize {
            return Err(CatalogError::InvalidInput {
                detail: "too many columns".to_string(),
            });
        }
        let ordinal = columns.len() as u16;
        let (next_table, table_op) = Self::alter_table_row_op(&table);
        let row = ColumnRow {
            table_id,
            ordinal,
            name: column.name.clone(),
            data_type: column.data_type,
            nullable: column.nullable,
            default_value: column.default_value.clone(),
            added_in_schema_version: next_table.schema_version,
            type_params: column.type_params.clone(),
            backfill_value,
        };
        self.engine
            .write_batch(&[table_op, Self::column_put_op(&row)])?;
        Ok(ordinal)
    }

    fn column_for_alter(&self, table_id: u32, ordinal: u16) -> Result<ColumnRow> {
        self.get_columns(table_id)?
            .into_iter()
            .find(|c| c.ordinal == ordinal)
            .ok_or_else(|| CatalogError::NotFound {
                object: format!("column {ordinal} of table {table_id}"),
            })
    }

    /// `ALTER TABLE ... RENAME COLUMN`: only the name changes. Rows, indexes, the
    /// primary key and foreign keys all refer to a column by ordinal, so no stored
    /// byte depends on the name.
    pub fn rename_column(&self, table_id: u32, ordinal: u16, new_name: &str) -> Result<()> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        validate_name(new_name, "column")?;
        let table = self.require_active_table(table_id)?;
        let mut column = self.column_for_alter(table_id, ordinal)?;
        if self
            .get_columns(table_id)?
            .iter()
            .any(|c| c.name == new_name && c.ordinal != ordinal)
        {
            return Err(CatalogError::AlreadyExists {
                object: format!("column {new_name:?}"),
            });
        }
        if let Some(check) = self
            .check_constraints()?
            .iter()
            .find(|c| c.table_id == table_id && c.column_ordinals.contains(&ordinal))
        {
            return Err(CatalogError::InvalidInput {
                detail: format!(
                    "column {:?} is read by CHECK constraint {:?}; drop the constraint, rename the column, and add it again",
                    column.name, check.name
                ),
            });
        }
        column.name = new_name.to_string();
        let (_, table_op) = Self::alter_table_row_op(&table);
        self.engine
            .write_batch(&[table_op, Self::column_put_op(&column)])?;
        Ok(())
    }

    /// `ALTER COLUMN ... SET DEFAULT <literal>` / `DROP DEFAULT`. Governs only rows
    /// written afterwards: the value older rows read as is the column's
    /// `backfill_value`, fixed when the column was added.
    pub fn set_column_default(
        &self,
        table_id: u32,
        ordinal: u16,
        default_value: Option<Vec<u8>>,
    ) -> Result<()> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        let table = self.require_active_table(table_id)?;
        let mut column = self.column_for_alter(table_id, ordinal)?;
        column.default_value = default_value;
        let (_, table_op) = Self::alter_table_row_op(&table);
        self.engine
            .write_batch(&[table_op, Self::column_put_op(&column)])?;
        Ok(())
    }

    /// `ALTER COLUMN ... SET NOT NULL` / `DROP NOT NULL`. The catalog flips the
    /// flag; for `SET NOT NULL` the caller must already hold the table's epoch
    /// write-lock and have verified that no row holds `NULL` (`relational::alter`).
    pub fn set_column_nullable(&self, table_id: u32, ordinal: u16, nullable: bool) -> Result<()> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        let table = self.require_active_table(table_id)?;
        let mut column = self.column_for_alter(table_id, ordinal)?;
        if nullable && table.pk_ordinals.contains(&ordinal) {
            return Err(CatalogError::InvalidInput {
                detail: format!(
                    "column {:?} is part of the primary key and cannot be made nullable",
                    column.name
                ),
            });
        }
        if !nullable {
            // A foreign key whose referential action writes NULL needs its column nullable.
            for fk in self.foreign_keys()?.iter() {
                if fk.table_id == table_id
                    && fk.child_ordinals.contains(&ordinal)
                    && (fk.on_delete == FkAction::SetNull || fk.on_update == FkAction::SetNull)
                {
                    return Err(CatalogError::InvalidInput {
                        detail: format!(
                            "column {:?} cannot be NOT NULL: foreign key {:?} sets it to NULL",
                            column.name, fk.name
                        ),
                    });
                }
            }
        }
        column.nullable = nullable;
        let (_, table_op) = Self::alter_table_row_op(&table);
        self.engine
            .write_batch(&[table_op, Self::column_put_op(&column)])?;
        Ok(())
    }

    // -------------------------------------------------------------
    // system.foreign_keys (ADR-DDL-FK-01)
    // -------------------------------------------------------------

    /// Every foreign key in the catalog, from a cache that is rebuilt after any
    /// change to `system.foreign_keys` (this process is the only writer, so the
    /// cache cannot go stale behind its back; `fk_generation` guards the one race
    /// that remains, a load interleaving with an invalidation). Cheap enough to
    /// call on every commit.
    pub fn foreign_keys(&self) -> Result<Arc<Vec<ForeignKeyRow>>> {
        if let Some(cached) = self
            .fk_cache
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return Ok(Arc::clone(cached));
        }
        let generation = self.fk_generation.load(Ordering::Acquire);
        let (start, end) = system_table_range(SYSTEM_TABLE_FOREIGN_KEYS);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let fk_id = pk_tail_u32(&key)?;
            let (_, fields) = decode_row(&value, &FOREIGN_KEYS_SCHEMA)?;
            out.push(ForeignKeyRow::from_fields(fk_id, fields)?);
        }
        let loaded = Arc::new(out);
        let mut slot = self.fk_cache.write().unwrap_or_else(|p| p.into_inner());
        if self.fk_generation.load(Ordering::Acquire) == generation {
            *slot = Some(Arc::clone(&loaded));
        }
        Ok(loaded)
    }

    /// Changes whenever a foreign key is created or dropped. A committing
    /// transaction compares it before and after taking its table locks, to know
    /// that the set of keys it validates against is the set in force.
    pub fn foreign_key_generation(&self) -> u64 {
        self.fk_generation.load(Ordering::Acquire)
    }

    fn invalidate_foreign_keys(&self) {
        self.fk_generation.fetch_add(1, Ordering::AcqRel);
        *self.fk_cache.write().unwrap_or_else(|p| p.into_inner()) = None;
    }

    pub fn list_foreign_keys_for_child(&self, table_id: u32) -> Result<Vec<ForeignKeyRow>> {
        Ok(self
            .foreign_keys()?
            .iter()
            .filter(|fk| fk.table_id == table_id)
            .cloned()
            .collect())
    }

    pub fn list_foreign_keys_for_parent(&self, table_id: u32) -> Result<Vec<ForeignKeyRow>> {
        Ok(self
            .foreign_keys()?
            .iter()
            .filter(|fk| fk.parent_table_id == table_id)
            .cloned()
            .collect())
    }

    fn fk_shape(&self, table: &TableRow) -> Result<FkShape> {
        Ok(FkShape {
            table_id: table.table_id,
            name: table.name.clone(),
            pk_ordinals: table.pk_ordinals.clone(),
            columns: self.get_columns(table.table_id)?,
            unique_indexes: self
                .list_indexes(table.table_id)?
                .into_iter()
                .filter(|i| i.kind == IndexKind::Unique && i.state == IndexState::Ready)
                .collect(),
        })
    }

    /// Checks everything about a foreign key's *definition* against the catalog
    /// and returns the `UNIQUE` index its referenced columns resolve to (`None` =
    /// the primary key): both tables and all columns exist, equal arity and
    /// identical types, the referenced columns are the primary key or a ready
    /// `UNIQUE` index, the referential actions are possible for the referencing
    /// columns, the name is free. It looks at no row --
    /// `relational::alter::alter_add_foreign_key` validates the existing rows under
    /// the two tables' locks and then records the key.
    pub fn check_foreign_key(&self, new: &NewForeignKey) -> Result<Option<u32>> {
        validate_name(&new.name, "constraint")?;
        let child = self.require_active_table(new.table_id)?;
        let parent = self.require_active_table(new.parent_table_id)?;
        let child_shape = self.fk_shape(&child)?;
        let parent_shape = if parent.table_id == child.table_id {
            child_shape.clone()
        } else {
            self.fk_shape(&parent)?
        };
        let resolved = resolve_foreign_key(
            &FkDefinition {
                child_ordinals: &new.child_ordinals,
                parent_ordinals: &new.parent_ordinals,
                on_delete: new.on_delete,
                on_update: new.on_update,
            },
            &child_shape,
            &parent_shape,
        )?;
        if self
            .list_foreign_keys_for_child(child.table_id)?
            .iter()
            .any(|f| f.name == new.name)
            || self
                .check_constraints()?
                .iter()
                .any(|c| c.table_id == child.table_id && c.name == new.name)
        {
            return Err(CatalogError::AlreadyExists {
                object: format!("constraint {:?}", new.name),
            });
        }
        Ok(resolved)
    }

    /// Records a foreign key (see `check_foreign_key`, which it re-runs under the
    /// DDL lock). Returns its id.
    pub fn create_foreign_key(&self, new: &NewForeignKey) -> Result<u32> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        let parent_index_id = self.check_foreign_key(new)?;
        let child = self.require_active_table(new.table_id)?;
        let (fk_id, counter_op) = self.allocate_id(CounterKind::Constraint)?;
        let row = ForeignKeyRow {
            fk_id,
            table_id: child.table_id,
            name: new.name.clone(),
            child_ordinals: new.child_ordinals.clone(),
            parent_table_id: new.parent_table_id,
            parent_ordinals: new.parent_ordinals.clone(),
            parent_index_id,
            on_delete: new.on_delete,
            on_update: new.on_update,
            created_at: now_micros(),
        };
        // The child's `schema_version` is bumped with the key: a transaction that
        // buffered rows for the child before the key existed must not commit them
        // unchecked (it fails with a conflict instead).
        let (_, child_op) = Self::alter_table_row_op(&child);
        let ops = vec![
            counter_op,
            child_op,
            WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_FOREIGN_KEYS, &fk_id.to_be_bytes()),
                value: encode_row(1, &row.to_fields()),
            },
        ];
        self.engine.write_batch(&ops)?;
        self.invalidate_foreign_keys();
        Ok(fk_id)
    }

    /// Drops a foreign key by id. The child table's `schema_version` is bumped so
    /// an in-flight transaction notices the change.
    pub fn drop_foreign_key(&self, fk_id: u32) -> Result<()> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        let fk = self
            .foreign_keys()?
            .iter()
            .find(|f| f.fk_id == fk_id)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound {
                object: format!("foreign key {fk_id}"),
            })?;
        let mut ops = vec![WriteOp::Delete {
            key: catalog_key(SYSTEM_TABLE_FOREIGN_KEYS, &fk_id.to_be_bytes()),
        }];
        if let Some(child) = self.get_table(fk.table_id)? {
            ops.push(Self::alter_table_row_op(&child).1);
        }
        self.engine.write_batch(&ops)?;
        self.invalidate_foreign_keys();
        Ok(())
    }

    // -------------------------------------------------------------
    // CHECK constraints (system.constraints, kind Check -- the existing row format)
    // -------------------------------------------------------------

    /// Every `CHECK` constraint in the catalog, from a cache rebuilt after any change
    /// (the same arrangement as `foreign_keys`). A write statement asks for the
    /// constraints of the table it writes on every execution, so this is cheap when
    /// there are none.
    pub fn check_constraints(&self) -> Result<Arc<Vec<ConstraintRow>>> {
        if let Some(cached) = self
            .check_cache
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            return Ok(Arc::clone(cached));
        }
        let generation = self.check_generation.load(Ordering::Acquire);
        let (start, end) = system_table_range(SYSTEM_TABLE_CONSTRAINTS);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let constraint_id = pk_tail_u32(&key)?;
            let (_, fields) = decode_row(&value, &CONSTRAINTS_SCHEMA)?;
            let constraint = ConstraintRow::from_fields(constraint_id, fields)?;
            if constraint.kind == ConstraintKind::Check {
                out.push(constraint);
            }
        }
        let loaded = Arc::new(out);
        let mut slot = self.check_cache.write().unwrap_or_else(|p| p.into_inner());
        if self.check_generation.load(Ordering::Acquire) == generation {
            *slot = Some(Arc::clone(&loaded));
        }
        Ok(loaded)
    }

    pub fn check_constraints_for(&self, table_id: u32) -> Result<Vec<ConstraintRow>> {
        Ok(self
            .check_constraints()?
            .iter()
            .filter(|c| c.table_id == table_id)
            .cloned()
            .collect())
    }

    fn invalidate_checks(&self) {
        self.check_generation.fetch_add(1, Ordering::AcqRel);
        *self.check_cache.write().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// Records a `CHECK` constraint whose expression the caller has already bound,
    /// and validated against every existing row, under the table's epoch write-lock
    /// (`relational::alter`). One batch: the constraint row and the bumped table
    /// `schema_version`.
    pub fn create_check_constraint(
        &self,
        table_id: u32,
        name: &str,
        column_ordinals: &[u16],
        expression: &str,
    ) -> Result<u32> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        validate_name(name, "constraint")?;
        validate_check_expression(expression)?;
        let table = self.require_active_table(table_id)?;
        let columns = self.get_columns(table_id)?;
        if column_ordinals
            .iter()
            .any(|&o| !columns.iter().any(|c| c.ordinal == o))
        {
            return Err(CatalogError::InvalidInput {
                detail: "a CHECK constraint names a column that does not exist".to_string(),
            });
        }
        if self
            .check_constraints()?
            .iter()
            .any(|c| c.table_id == table_id && c.name == name)
            || self
                .list_foreign_keys_for_child(table_id)?
                .iter()
                .any(|f| f.name == name)
        {
            return Err(CatalogError::AlreadyExists {
                object: format!("constraint {name:?}"),
            });
        }
        let (constraint_id, counter_op) = self.allocate_id(CounterKind::Constraint)?;
        let (next_table, table_op) = Self::alter_table_row_op(&table);
        let row = ConstraintRow {
            constraint_id,
            table_id,
            name: name.to_string(),
            kind: ConstraintKind::Check,
            column_ordinals: column_ordinals.to_vec(),
            check_expression: Some(expression.to_string()),
            added_in_schema_version: next_table.schema_version,
        };
        self.engine.write_batch(&[
            counter_op,
            table_op,
            WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_CONSTRAINTS, &constraint_id.to_be_bytes()),
                value: encode_row(1, &row.to_fields()),
            },
        ])?;
        self.invalidate_checks();
        Ok(constraint_id)
    }

    /// Drops a `CHECK` constraint; the table `schema_version` is bumped so a
    /// transaction that evaluated it notices.
    pub fn drop_check_constraint(&self, constraint_id: u32) -> Result<()> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        let constraint = self
            .check_constraints()?
            .iter()
            .find(|c| c.constraint_id == constraint_id)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound {
                object: format!("constraint {constraint_id}"),
            })?;
        let mut ops = vec![WriteOp::Delete {
            key: catalog_key(SYSTEM_TABLE_CONSTRAINTS, &constraint_id.to_be_bytes()),
        }];
        if let Some(table) = self.get_table(constraint.table_id)? {
            ops.push(Self::alter_table_row_op(&table).1);
        }
        self.engine.write_batch(&ops)?;
        self.invalidate_checks();
        Ok(())
    }

    fn refuse_if_foreign_key_uses_index(&self, index_id: u32) -> Result<()> {
        if let Some(fk) = self
            .foreign_keys()?
            .iter()
            .find(|fk| fk.parent_index_id == Some(index_id))
        {
            return Err(CatalogError::InvalidInput {
                detail: format!(
                    "index {index_id} is the referenced key of foreign key {:?}; drop that constraint first",
                    fk.name
                ),
            });
        }
        Ok(())
    }

    // -------------------------------------------------------------
    // system.grants (D25 — storage only; no authorization checking
    // exists in this increment, per the review directive's own
    // "preserve the boundary, do not prematurely invent a different
    // model" instruction)
    // -------------------------------------------------------------

    pub fn grant(
        &self,
        principal: &str,
        object_kind: ObjectKind,
        object_id: u32,
        privilege: Privilege,
    ) -> Result<u32> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        if principal.is_empty() {
            return Err(CatalogError::InvalidInput {
                detail: "principal must not be empty".to_string(),
            });
        }
        let duplicate = self
            .list_grants_for_principal(principal)?
            .into_iter()
            .any(|g| {
                g.object_kind == object_kind && g.object_id == object_id && g.privilege == privilege
            });
        if duplicate {
            return Err(CatalogError::AlreadyExists {
                object: format!(
                    "grant({principal:?}, {object_kind:?}, {object_id}, {privilege:?})"
                ),
            });
        }

        let (grant_id, counter_op) = self.allocate_id(CounterKind::Grant)?;
        let row = GrantRow {
            grant_id,
            principal: principal.to_string(),
            object_kind,
            object_id,
            privilege,
            granted_at: now_micros(),
        };
        let ops = vec![
            counter_op,
            WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_GRANTS, &grant_id.to_be_bytes()),
                value: encode_row(1, &row.to_fields()),
            },
        ];
        self.engine.write_batch(&ops)?;
        Ok(grant_id)
    }

    /// Removes every grant row exactly matching the tuple; returns how
    /// many were removed (0 or 1 in practice, since `grant` itself
    /// rejects duplicates — never more than 1 unless a future increment
    /// removes that invariant).
    pub fn revoke(
        &self,
        principal: &str,
        object_kind: ObjectKind,
        object_id: u32,
        privilege: Privilege,
    ) -> Result<usize> {
        let _guard = self.ddl_lock.lock().unwrap_or_else(|p| p.into_inner());
        let matching: Vec<u32> = self
            .list_grants_for_principal(principal)?
            .into_iter()
            .filter(|g| {
                g.object_kind == object_kind && g.object_id == object_id && g.privilege == privilege
            })
            .map(|g| g.grant_id)
            .collect();
        if matching.is_empty() {
            return Ok(0);
        }
        let ops: Vec<WriteOp> = matching
            .iter()
            .map(|&id| WriteOp::Delete {
                key: catalog_key(SYSTEM_TABLE_GRANTS, &id.to_be_bytes()),
            })
            .collect();
        self.engine.write_batch(&ops)?;
        Ok(matching.len())
    }

    /// Surrogate `grant_id` PK (CA.2) — full-table scan filtered by
    /// `principal`. Catalog-scale traffic (D1's own performance-impact
    /// statement), not a hot path.
    pub fn list_grants_for_principal(&self, principal: &str) -> Result<Vec<GrantRow>> {
        let (start, end) = system_table_range(SYSTEM_TABLE_GRANTS);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let grant_id = pk_tail_u32(&key)?;
            let (_, fields) = decode_row(&value, &GRANTS_SCHEMA)?;
            let grant = GrantRow::from_fields(grant_id, fields)?;
            if grant.principal == principal {
                out.push(grant);
            }
        }
        Ok(out)
    }
}

/// Decodes a `system.columns` row by its own envelope version: a version-1 row
/// (written before `ALTER TABLE`) has `COLUMNS_V1_FIELDS` fields, a later one all
/// of `COLUMNS_SCHEMA`.
fn decode_column_row(
    bytes: &[u8],
) -> Result<(u32, Vec<Option<crate::catalog::encoding::CatalogValue>>)> {
    let version = bytes
        .get(1..5)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
        .ok_or_else(|| CatalogError::InvalidInput {
            detail: "system.columns row too short for its envelope header".to_string(),
        })?;
    let schema = if version >= 2 {
        &COLUMNS_SCHEMA[..]
    } else {
        &COLUMNS_SCHEMA[..COLUMNS_V1_FIELDS]
    };
    decode_row(bytes, schema)
}

/// What a foreign-key definition check needs to know about one table.
#[derive(Clone)]
struct FkShape {
    table_id: u32,
    name: String,
    pk_ordinals: Vec<u16>,
    columns: Vec<ColumnRow>,
    /// The table's ready `UNIQUE` indexes.
    unique_indexes: Vec<IndexRow>,
}

struct FkDefinition<'a> {
    child_ordinals: &'a [u16],
    parent_ordinals: &'a [u16],
    on_delete: FkAction,
    on_update: FkAction,
}

/// The rules a foreign-key definition must meet, against the two tables' shapes
/// (so the same check serves `ALTER TABLE ... ADD FOREIGN KEY`, where both tables
/// are in the catalog, and `CREATE TABLE`, where the referencing table is not
/// yet). Returns the `UNIQUE` index the referenced columns resolve to, `None` for
/// the primary key.
fn resolve_foreign_key(
    def: &FkDefinition<'_>,
    child: &FkShape,
    parent: &FkShape,
) -> Result<Option<u32>> {
    let n = def.child_ordinals.len();
    if n == 0 || n != def.parent_ordinals.len() {
        return Err(CatalogError::InvalidInput {
            detail: "a foreign key needs the same, non-zero number of referencing and referenced columns"
                .to_string(),
        });
    }
    if n > MAX_COLUMNS_PER_INDEX {
        return Err(CatalogError::InvalidInput {
            detail: format!("a foreign key may cover at most {MAX_COLUMNS_PER_INDEX} columns"),
        });
    }
    for (list, what) in [
        (def.child_ordinals, "referencing"),
        (def.parent_ordinals, "referenced"),
    ] {
        let mut seen = std::collections::HashSet::new();
        if !list.iter().all(|o| seen.insert(*o)) {
            return Err(CatalogError::InvalidInput {
                detail: format!("a {what} column is listed twice"),
            });
        }
    }
    for (&c, &p) in def.child_ordinals.iter().zip(def.parent_ordinals) {
        let cc = child
            .columns
            .iter()
            .find(|x| x.ordinal == c)
            .ok_or_else(|| CatalogError::InvalidInput {
                detail: format!("referencing column {c} does not exist"),
            })?;
        let pc = parent
            .columns
            .iter()
            .find(|x| x.ordinal == p)
            .ok_or_else(|| CatalogError::InvalidInput {
                detail: format!("referenced column {p} does not exist"),
            })?;
        if cc.data_type != pc.data_type || cc.type_params != pc.type_params {
            return Err(CatalogError::InvalidInput {
                detail: format!(
                    "referencing column {:?} and referenced column {:?} have different types",
                    cc.name, pc.name
                ),
            });
        }
    }

    // The referenced columns must be unique: the primary key, or a ready UNIQUE
    // index, covering exactly the same set of columns.
    let as_set = |v: &[u16]| {
        v.iter()
            .copied()
            .collect::<std::collections::BTreeSet<u16>>()
    };
    let wanted = as_set(def.parent_ordinals);
    let parent_index_id = if as_set(&parent.pk_ordinals) == wanted {
        None
    } else {
        match parent
            .unique_indexes
            .iter()
            .find(|i| as_set(&i.column_ordinals) == wanted)
        {
            Some(i) => Some(i.index_id),
            None => {
                return Err(CatalogError::InvalidInput {
                    detail: format!(
                        "the referenced columns of table {:?} are neither its primary key nor covered by a ready UNIQUE index",
                        parent.name
                    ),
                })
            }
        }
    };

    // A referential action must be possible for the referencing columns.
    for (action, what) in [(def.on_delete, "ON DELETE"), (def.on_update, "ON UPDATE")] {
        for &c in def.child_ordinals {
            let cc = child
                .columns
                .iter()
                .find(|x| x.ordinal == c)
                .expect("checked above");
            match action {
                FkAction::SetNull if !cc.nullable || child.pk_ordinals.contains(&c) => {
                    return Err(CatalogError::InvalidInput {
                        detail: format!(
                            "{what} SET NULL needs column {:?} to be nullable and outside the primary key",
                            cc.name
                        ),
                    })
                }
                FkAction::SetDefault if !cc.nullable && cc.default_value.is_none() => {
                    return Err(CatalogError::InvalidInput {
                        detail: format!(
                            "{what} SET DEFAULT needs column {:?} to be nullable or have a DEFAULT",
                            cc.name
                        ),
                    })
                }
                _ => {}
            }
        }
    }
    let _ = child.table_id;
    Ok(parent_index_id)
}

fn validate_check_expression(expression: &str) -> Result<()> {
    if expression.trim().is_empty() {
        return Err(CatalogError::InvalidInput {
            detail: "CHECK constraint requires a non-empty expression".to_string(),
        });
    }
    if expression.len() > MAX_CHECK_EXPRESSION_BYTES {
        return Err(CatalogError::InvalidInput {
            detail: format!("a CHECK expression may be at most {MAX_CHECK_EXPRESSION_BYTES} bytes"),
        });
    }
    Ok(())
}

fn validate_name(name: &str, kind: &str) -> Result<()> {
    if name.is_empty() {
        return Err(CatalogError::InvalidInput {
            detail: format!("{kind} name must not be empty"),
        });
    }
    if name.len() > 255 {
        return Err(CatalogError::InvalidInput {
            detail: format!("{kind} name exceeds 255 bytes"),
        });
    }
    Ok(())
}

fn column_pk(table_id: u32, ordinal: u16) -> Vec<u8> {
    let mut pk = Vec::with_capacity(6);
    pk.extend_from_slice(&table_id.to_be_bytes());
    pk.extend_from_slice(&ordinal.to_be_bytes());
    pk
}

/// Extracts a surrogate `u32` primary key from the tail of a catalog
/// row's physical key (`0x00 || system_table_id:u32 BE || pk:u32 BE`).
fn pk_tail_u32(key: &[u8]) -> Result<u32> {
    let tail = key.get(5..9).ok_or_else(|| CatalogError::InvalidInput {
        detail: "catalog key too short for a u32 primary key".to_string(),
    })?;
    Ok(u32::from_be_bytes(
        tail.try_into().expect("checked length 4"),
    ))
}

/// Extracts `system.columns`' `ordinal` (the second half of its
/// composite `(table_id, ordinal)` key, after the 4-byte `table_id`).
fn column_pk_ordinal(key: &[u8]) -> Result<u16> {
    let tail = key.get(9..11).ok_or_else(|| CatalogError::InvalidInput {
        detail: "system.columns key too short for an ordinal".to_string(),
    })?;
    Ok(u16::from_be_bytes(
        tail.try_into().expect("checked length 2"),
    ))
}

fn as_bound_ref(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}
