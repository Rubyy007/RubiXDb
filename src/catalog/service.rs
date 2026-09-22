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
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::catalog::encoding::{
    catalog_key, decode_row, encode_row, system_table_prefix_range, system_table_range,
    SYSTEM_TABLE_COLUMNS, SYSTEM_TABLE_CONSTRAINTS, SYSTEM_TABLE_COUNTERS, SYSTEM_TABLE_DATABASES,
    SYSTEM_TABLE_GRANTS, SYSTEM_TABLE_INDEXES, SYSTEM_TABLE_SCHEMAS, SYSTEM_TABLE_TABLES,
};
use crate::catalog::error::{CatalogError, Result};
use crate::catalog::schema::{
    ColumnRow, ConstraintKind, ConstraintRow, DatabaseRow, GrantRow, IndexKind, IndexRow,
    IndexState, ObjectKind, Privilege, SchemaRow, TableRow, TableState, COLUMNS_SCHEMA,
    CONSTRAINTS_SCHEMA, DATABASES_SCHEMA, GRANTS_SCHEMA, INDEXES_SCHEMA, SCHEMAS_SCHEMA,
    TABLES_SCHEMA,
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
}

impl CatalogService {
    pub fn new(engine: Arc<LsmEngine>) -> Self {
        CatalogService {
            engine,
            ddl_lock: Mutex::new(()),
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
            };
            ops.push(WriteOp::Put {
                key: catalog_key(SYSTEM_TABLE_COLUMNS, &column_pk(table_id, ordinal)),
                value: encode_row(1, &row.to_fields()),
            });
        }

        self.engine.write_batch(&ops)?;
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
            let (_, fields) = decode_row(&value, &COLUMNS_SCHEMA)?;
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

        let mut ops = vec![WriteOp::Delete {
            key: catalog_key(SYSTEM_TABLE_TABLES, &table_id.to_be_bytes()),
        }];
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

        self.engine.write_batch(&ops)?;
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
