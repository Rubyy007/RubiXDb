//! An in-memory mirror of the catalog (`0x00` namespace) built straight from
//! raw keyspace entries — *not* through `CatalogService` — so the integrity
//! checker and the backup verifier validate what is physically stored with
//! their own decoding, and share one implementation of it.

use std::collections::BTreeMap;

use crate::catalog::encoding::{
    decode_row, CatalogValue, CatalogValueType, SYSTEM_TABLE_COLUMNS, SYSTEM_TABLE_CONSTRAINTS,
    SYSTEM_TABLE_COUNTERS, SYSTEM_TABLE_DATABASES, SYSTEM_TABLE_GRANTS, SYSTEM_TABLE_INDEXES,
    SYSTEM_TABLE_SCHEMAS, SYSTEM_TABLE_TABLES,
};
use crate::catalog::schema::{
    ColumnRow, ConstraintRow, DatabaseRow, GrantRow, IndexRow, SchemaRow, TableRow, COLUMNS_SCHEMA,
    CONSTRAINTS_SCHEMA, DATABASES_SCHEMA, GRANTS_SCHEMA, INDEXES_SCHEMA, SCHEMAS_SCHEMA,
    TABLES_SCHEMA,
};

/// One problem found in the catalog. `code` is from the closed set in
/// `ops::check::finding_codes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogProblem {
    pub code: &'static str,
    pub object: String,
    pub detail: String,
}

fn problem(
    code: &'static str,
    object: impl Into<String>,
    detail: impl Into<String>,
) -> CatalogProblem {
    CatalogProblem {
        code,
        object: object.into(),
        detail: detail.into(),
    }
}

#[derive(Default)]
pub struct CatalogMirror {
    pub databases: BTreeMap<u32, DatabaseRow>,
    pub schemas: BTreeMap<u32, SchemaRow>,
    pub tables: BTreeMap<u32, TableRow>,
    pub columns: BTreeMap<(u32, u16), ColumnRow>,
    pub indexes: BTreeMap<u32, IndexRow>,
    pub constraints: BTreeMap<u32, ConstraintRow>,
    pub grants: BTreeMap<u32, GrantRow>,
    pub counters: BTreeMap<u8, u32>,
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

fn pk_u32(pk: &[u8], what: &str) -> Result<u32, CatalogProblem> {
    if pk.len() != 4 {
        return Err(problem(
            "CATALOG_KEY_MALFORMED",
            what,
            format!("key carries {} id byte(s), expected 4", pk.len()),
        ));
    }
    Ok(be_u32(pk))
}

fn fields(
    schema: &[CatalogValueType],
    value: &[u8],
    what: &str,
) -> Result<Vec<Option<CatalogValue>>, CatalogProblem> {
    decode_row(value, schema)
        .map(|(_, f)| f)
        .map_err(|e| problem("CATALOG_ROW_UNDECODABLE", what, e.to_string()))
}

impl CatalogMirror {
    /// Ingests one raw entry whose key starts with the catalog namespace
    /// byte. An undecodable row is returned as a problem (and not stored).
    pub fn ingest(&mut self, key: &[u8], value: &[u8]) -> Result<(), CatalogProblem> {
        if key.len() < 5 {
            return Err(problem(
                "CATALOG_KEY_MALFORMED",
                "catalog",
                "key shorter than namespace + system table id",
            ));
        }
        let table = be_u32(&key[1..5]);
        let pk = &key[5..];
        let obj = |name: &str, id: u32| format!("{name}[{id}]");
        let bad = |e: crate::catalog::CatalogError, what: &str| {
            problem("CATALOG_ROW_UNDECODABLE", what, e.to_string())
        };
        match table {
            SYSTEM_TABLE_COUNTERS => {
                if pk.len() != 1 || value.len() != 4 {
                    return Err(problem(
                        "CATALOG_ROW_UNDECODABLE",
                        "system.counters",
                        "ID counter has an unexpected key or value length",
                    ));
                }
                self.counters
                    .insert(pk[0], u32::from_le_bytes(value.try_into().unwrap()));
            }
            SYSTEM_TABLE_DATABASES => {
                let id = pk_u32(pk, "system.databases")?;
                let f = fields(&DATABASES_SCHEMA, value, &obj("system.databases", id))?;
                let row = DatabaseRow::from_fields(id, f)
                    .map_err(|e| bad(e, &obj("system.databases", id)))?;
                self.databases.insert(id, row);
            }
            SYSTEM_TABLE_SCHEMAS => {
                let id = pk_u32(pk, "system.schemas")?;
                let f = fields(&SCHEMAS_SCHEMA, value, &obj("system.schemas", id))?;
                let row = SchemaRow::from_fields(id, f)
                    .map_err(|e| bad(e, &obj("system.schemas", id)))?;
                self.schemas.insert(id, row);
            }
            SYSTEM_TABLE_TABLES => {
                let id = pk_u32(pk, "system.tables")?;
                let f = fields(&TABLES_SCHEMA, value, &obj("system.tables", id))?;
                let row =
                    TableRow::from_fields(id, f).map_err(|e| bad(e, &obj("system.tables", id)))?;
                self.tables.insert(id, row);
            }
            SYSTEM_TABLE_COLUMNS => {
                if pk.len() != 6 {
                    return Err(problem(
                        "CATALOG_KEY_MALFORMED",
                        "system.columns",
                        format!("key carries {} byte(s), expected 6", pk.len()),
                    ));
                }
                let tid = be_u32(pk);
                let ord = u16::from_be_bytes(pk[4..6].try_into().unwrap());
                let what = format!("system.columns[{tid},{ord}]");
                let f = fields(&COLUMNS_SCHEMA, value, &what)?;
                let row = ColumnRow::from_fields(tid, ord, f).map_err(|e| bad(e, &what))?;
                self.columns.insert((tid, ord), row);
            }
            SYSTEM_TABLE_INDEXES => {
                let id = pk_u32(pk, "system.indexes")?;
                let f = fields(&INDEXES_SCHEMA, value, &obj("system.indexes", id))?;
                let row =
                    IndexRow::from_fields(id, f).map_err(|e| bad(e, &obj("system.indexes", id)))?;
                self.indexes.insert(id, row);
            }
            SYSTEM_TABLE_CONSTRAINTS => {
                let id = pk_u32(pk, "system.constraints")?;
                let f = fields(&CONSTRAINTS_SCHEMA, value, &obj("system.constraints", id))?;
                let row = ConstraintRow::from_fields(id, f)
                    .map_err(|e| bad(e, &obj("system.constraints", id)))?;
                self.constraints.insert(id, row);
            }
            SYSTEM_TABLE_GRANTS => {
                let id = pk_u32(pk, "system.grants")?;
                let f = fields(&GRANTS_SCHEMA, value, &obj("system.grants", id))?;
                let row =
                    GrantRow::from_fields(id, f).map_err(|e| bad(e, &obj("system.grants", id)))?;
                self.grants.insert(id, row);
            }
            other => {
                return Err(problem(
                    "CATALOG_KEY_MALFORMED",
                    "catalog",
                    format!("unknown system table id {other}"),
                ))
            }
        }
        Ok(())
    }

    pub fn column_count(&self, table_id: u32) -> usize {
        self.columns
            .range((table_id, 0)..=(table_id, u16::MAX))
            .count()
    }

    /// Cross-reference validation of what was ingested.
    pub fn validate(&self) -> Vec<CatalogProblem> {
        let mut out = Vec::new();
        for s in self.schemas.values() {
            if !self.databases.contains_key(&s.database_id) {
                out.push(problem(
                    "CATALOG_DANGLING_REF",
                    format!("schema[{}]", s.schema_id),
                    format!("references missing database {}", s.database_id),
                ));
            }
        }
        for t in self.tables.values() {
            let obj = format!("table[{}]", t.table_id);
            if !self.schemas.contains_key(&t.schema_id) {
                out.push(problem(
                    "CATALOG_DANGLING_REF",
                    &obj,
                    format!("references missing schema {}", t.schema_id),
                ));
            }
            let ncols = self.column_count(t.table_id);
            if ncols == 0 {
                out.push(problem(
                    "CATALOG_COLUMNS_INVALID",
                    &obj,
                    "table has no columns",
                ));
                continue;
            }
            for (i, ((_, ord), _)) in self
                .columns
                .range((t.table_id, 0)..=(t.table_id, u16::MAX))
                .enumerate()
            {
                if usize::from(*ord) != i {
                    out.push(problem(
                        "CATALOG_COLUMNS_INVALID",
                        &obj,
                        "column ordinals are not contiguous from 0",
                    ));
                    break;
                }
            }
            if t.pk_ordinals.is_empty() || t.pk_ordinals.iter().any(|o| usize::from(*o) >= ncols) {
                out.push(problem(
                    "CATALOG_COLUMNS_INVALID",
                    &obj,
                    "primary key is empty or references a missing column",
                ));
            }
        }
        for (tid, _) in self.columns.keys() {
            if !self.tables.contains_key(tid) {
                out.push(problem(
                    "CATALOG_DANGLING_REF",
                    format!("column of table[{tid}]"),
                    "column row belongs to a table that is not in the catalog",
                ));
            }
        }
        for i in self.indexes.values() {
            let obj = format!("index[{}]", i.index_id);
            if !self.tables.contains_key(&i.table_id) {
                out.push(problem(
                    "CATALOG_DANGLING_REF",
                    &obj,
                    format!("references missing table {}", i.table_id),
                ));
                continue;
            }
            let ncols = self.column_count(i.table_id);
            if i.column_ordinals.is_empty()
                || i.column_ordinals.iter().any(|o| usize::from(*o) >= ncols)
            {
                out.push(problem(
                    "CATALOG_COLUMNS_INVALID",
                    &obj,
                    "index column list is empty or references a missing column",
                ));
            }
        }
        for c in self.constraints.values() {
            if !self.tables.contains_key(&c.table_id) {
                out.push(problem(
                    "CATALOG_DANGLING_REF",
                    format!("constraint[{}]", c.constraint_id),
                    format!("references missing table {}", c.table_id),
                ));
            }
        }
        let max_id = |keys: Vec<u32>| keys.into_iter().max().unwrap_or(0);
        for (kind, name, issued) in [
            (
                1u8,
                "database",
                max_id(self.databases.keys().copied().collect()),
            ),
            (2, "schema", max_id(self.schemas.keys().copied().collect())),
            (3, "table", max_id(self.tables.keys().copied().collect())),
            (4, "index", max_id(self.indexes.keys().copied().collect())),
            (
                5,
                "constraint",
                max_id(self.constraints.keys().copied().collect()),
            ),
            (6, "grant", max_id(self.grants.keys().copied().collect())),
        ] {
            let counter = self.counters.get(&kind).copied().unwrap_or(0);
            if issued > 0 && counter < issued {
                out.push(problem(
                    "CATALOG_COUNTER_BEHIND",
                    format!("counter[{name}]"),
                    format!(
                        "counter {counter} is below an issued id {issued}; ids could be re-issued"
                    ),
                ));
            }
        }
        out
    }
}
