//! The seven system-table row structs and their `RowValue` schemas —
//! `RELATIONAL ADR AMENDMENT 002` CA.2. Each struct's primary-key fields
//! come from the physical key (D3: never redundantly stored in the
//! value); `to_fields`/`from_fields` convert the remaining columns
//! to/from `encoding::CatalogValue` in schema-column-ordinal order.

use crate::catalog::encoding::{decode_u16_list, encode_u16_list, CatalogValue, CatalogValueType};
use crate::catalog::error::{CatalogError, Result};

fn expect_u32(fields: &mut std::vec::IntoIter<Option<CatalogValue>>, what: &str) -> Result<u32> {
    match fields.next() {
        Some(Some(CatalogValue::U32(v))) => Ok(v),
        _ => Err(CatalogError::InvalidInput {
            detail: format!("expected non-null U32 for {what}"),
        }),
    }
}

fn expect_i64(fields: &mut std::vec::IntoIter<Option<CatalogValue>>, what: &str) -> Result<i64> {
    match fields.next() {
        Some(Some(CatalogValue::I64(v))) => Ok(v),
        _ => Err(CatalogError::InvalidInput {
            detail: format!("expected non-null I64 for {what}"),
        }),
    }
}

fn expect_bool(fields: &mut std::vec::IntoIter<Option<CatalogValue>>, what: &str) -> Result<bool> {
    match fields.next() {
        Some(Some(CatalogValue::Bool(v))) => Ok(v),
        _ => Err(CatalogError::InvalidInput {
            detail: format!("expected non-null BOOLEAN for {what}"),
        }),
    }
}

fn expect_u8(fields: &mut std::vec::IntoIter<Option<CatalogValue>>, what: &str) -> Result<u8> {
    match fields.next() {
        Some(Some(CatalogValue::U8(v))) => Ok(v),
        _ => Err(CatalogError::InvalidInput {
            detail: format!("expected non-null U8 for {what}"),
        }),
    }
}

fn expect_text(
    fields: &mut std::vec::IntoIter<Option<CatalogValue>>,
    what: &str,
) -> Result<String> {
    match fields.next() {
        Some(Some(CatalogValue::Text(v))) => Ok(v),
        _ => Err(CatalogError::InvalidInput {
            detail: format!("expected non-null TEXT for {what}"),
        }),
    }
}

fn expect_blob(
    fields: &mut std::vec::IntoIter<Option<CatalogValue>>,
    what: &str,
) -> Result<Vec<u8>> {
    match fields.next() {
        Some(Some(CatalogValue::Blob(v))) => Ok(v),
        _ => Err(CatalogError::InvalidInput {
            detail: format!("expected non-null BLOB for {what}"),
        }),
    }
}

/// An optional `TEXT` field (`system.constraints.check_expression`,
/// `system.columns.default_value` companions) — `None` means the row's
/// `NULL` bitmap bit was set for this column, a real, distinct catalog
/// state from `Some(String::new())`.
fn optional_text(fields: &mut std::vec::IntoIter<Option<CatalogValue>>) -> Result<Option<String>> {
    match fields.next() {
        Some(Some(CatalogValue::Text(v))) => Ok(Some(v)),
        Some(None) => Ok(None),
        _ => Err(CatalogError::InvalidInput {
            detail: "expected TEXT or NULL".to_string(),
        }),
    }
}

fn optional_blob(fields: &mut std::vec::IntoIter<Option<CatalogValue>>) -> Result<Option<Vec<u8>>> {
    match fields.next() {
        Some(Some(CatalogValue::Blob(v))) => Ok(Some(v)),
        Some(None) => Ok(None),
        _ => Err(CatalogError::InvalidInput {
            detail: "expected BLOB or NULL".to_string(),
        }),
    }
}

// ---------------------------------------------------------------------
// system.databases
// ---------------------------------------------------------------------

pub const DATABASES_SCHEMA: [CatalogValueType; 2] = [CatalogValueType::Text, CatalogValueType::I64];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseRow {
    pub database_id: u32,
    pub name: String,
    pub created_at: i64,
}

impl DatabaseRow {
    pub fn to_fields(&self) -> Vec<Option<CatalogValue>> {
        vec![
            Some(CatalogValue::Text(self.name.clone())),
            Some(CatalogValue::I64(self.created_at)),
        ]
    }

    pub fn from_fields(database_id: u32, fields: Vec<Option<CatalogValue>>) -> Result<Self> {
        let mut fields = fields.into_iter();
        Ok(DatabaseRow {
            database_id,
            name: expect_text(&mut fields, "system.databases.name")?,
            created_at: expect_i64(&mut fields, "system.databases.created_at")?,
        })
    }
}

// ---------------------------------------------------------------------
// system.schemas
// ---------------------------------------------------------------------

pub const SCHEMAS_SCHEMA: [CatalogValueType; 3] = [
    CatalogValueType::U32,
    CatalogValueType::Text,
    CatalogValueType::I64,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaRow {
    pub schema_id: u32,
    pub database_id: u32,
    pub name: String,
    pub created_at: i64,
}

impl SchemaRow {
    pub fn to_fields(&self) -> Vec<Option<CatalogValue>> {
        vec![
            Some(CatalogValue::U32(self.database_id)),
            Some(CatalogValue::Text(self.name.clone())),
            Some(CatalogValue::I64(self.created_at)),
        ]
    }

    pub fn from_fields(schema_id: u32, fields: Vec<Option<CatalogValue>>) -> Result<Self> {
        let mut fields = fields.into_iter();
        Ok(SchemaRow {
            schema_id,
            database_id: expect_u32(&mut fields, "system.schemas.database_id")?,
            name: expect_text(&mut fields, "system.schemas.name")?,
            created_at: expect_i64(&mut fields, "system.schemas.created_at")?,
        })
    }
}

// ---------------------------------------------------------------------
// system.tables
// ---------------------------------------------------------------------

pub const TABLES_SCHEMA: [CatalogValueType; 6] = [
    CatalogValueType::U32,
    CatalogValueType::Text,
    CatalogValueType::Blob,
    CatalogValueType::U32,
    CatalogValueType::U8,
    CatalogValueType::I64,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableState {
    Active = 0,
    Dropping = 1,
}

impl TableState {
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(TableState::Active),
            1 => Ok(TableState::Dropping),
            other => Err(CatalogError::InvalidInput {
                detail: format!("unknown TableState {other}"),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    pub table_id: u32,
    pub schema_id: u32,
    pub name: String,
    pub pk_ordinals: Vec<u16>,
    pub schema_version: u32,
    pub state: TableState,
    pub created_at: i64,
}

impl TableRow {
    pub fn to_fields(&self) -> Vec<Option<CatalogValue>> {
        vec![
            Some(CatalogValue::U32(self.schema_id)),
            Some(CatalogValue::Text(self.name.clone())),
            Some(CatalogValue::Blob(encode_u16_list(&self.pk_ordinals))),
            Some(CatalogValue::U32(self.schema_version)),
            Some(CatalogValue::U8(self.state as u8)),
            Some(CatalogValue::I64(self.created_at)),
        ]
    }

    pub fn from_fields(table_id: u32, fields: Vec<Option<CatalogValue>>) -> Result<Self> {
        let mut fields = fields.into_iter();
        let schema_id = expect_u32(&mut fields, "system.tables.schema_id")?;
        let name = expect_text(&mut fields, "system.tables.name")?;
        let pk_ordinals = decode_u16_list(&expect_blob(&mut fields, "system.tables.pk_ordinals")?)?;
        let schema_version = expect_u32(&mut fields, "system.tables.schema_version")?;
        let state = TableState::from_u8(expect_u8(&mut fields, "system.tables.state")?)?;
        let created_at = expect_i64(&mut fields, "system.tables.created_at")?;
        Ok(TableRow {
            table_id,
            schema_id,
            name,
            pk_ordinals,
            schema_version,
            state,
            created_at,
        })
    }
}

// ---------------------------------------------------------------------
// system.columns
// ---------------------------------------------------------------------

pub const COLUMNS_SCHEMA: [CatalogValueType; 5] = [
    CatalogValueType::Text,
    CatalogValueType::U8,
    CatalogValueType::Bool,
    CatalogValueType::Blob, // default_value; NULL bit means "no default"
    CatalogValueType::U32,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRow {
    pub table_id: u32,
    pub ordinal: u16,
    pub name: String,
    /// D4's full type-tag set (CA.5): this increment stores it verbatim
    /// so a future increment's user-table rows are typed correctly the
    /// moment they exist, but never itself interprets/encodes a value of
    /// most of these types (no user-table row storage exists yet).
    pub data_type: u8,
    pub nullable: bool,
    pub default_value: Option<Vec<u8>>,
    pub added_in_schema_version: u32,
}

impl ColumnRow {
    pub fn to_fields(&self) -> Vec<Option<CatalogValue>> {
        vec![
            Some(CatalogValue::Text(self.name.clone())),
            Some(CatalogValue::U8(self.data_type)),
            Some(CatalogValue::Bool(self.nullable)),
            self.default_value.clone().map(CatalogValue::Blob),
            Some(CatalogValue::U32(self.added_in_schema_version)),
        ]
    }

    pub fn from_fields(
        table_id: u32,
        ordinal: u16,
        fields: Vec<Option<CatalogValue>>,
    ) -> Result<Self> {
        let mut fields = fields.into_iter();
        let name = expect_text(&mut fields, "system.columns.name")?;
        let data_type = expect_u8(&mut fields, "system.columns.data_type")?;
        let nullable = expect_bool(&mut fields, "system.columns.nullable")?;
        let default_value = optional_blob(&mut fields)?;
        let added_in_schema_version =
            expect_u32(&mut fields, "system.columns.added_in_schema_version")?;
        Ok(ColumnRow {
            table_id,
            ordinal,
            name,
            data_type,
            nullable,
            default_value,
            added_in_schema_version,
        })
    }
}

// ---------------------------------------------------------------------
// system.indexes
// ---------------------------------------------------------------------

pub const INDEXES_SCHEMA: [CatalogValueType; 6] = [
    CatalogValueType::U32,
    CatalogValueType::Text,
    CatalogValueType::U8,
    CatalogValueType::Blob,
    CatalogValueType::U8,
    CatalogValueType::I64,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Primary = 0,
    Unique = 1,
    NonUnique = 2,
}

impl IndexKind {
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(IndexKind::Primary),
            1 => Ok(IndexKind::Unique),
            2 => Ok(IndexKind::NonUnique),
            other => Err(CatalogError::InvalidInput {
                detail: format!("unknown IndexKind {other}"),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    Active = 0,
    Building = 1,
}

impl IndexState {
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(IndexState::Active),
            1 => Ok(IndexState::Building),
            other => Err(CatalogError::InvalidInput {
                detail: format!("unknown IndexState {other}"),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    pub index_id: u32,
    pub table_id: u32,
    pub name: String,
    pub kind: IndexKind,
    pub column_ordinals: Vec<u16>,
    pub state: IndexState,
    pub created_at: i64,
}

impl IndexRow {
    pub fn to_fields(&self) -> Vec<Option<CatalogValue>> {
        vec![
            Some(CatalogValue::U32(self.table_id)),
            Some(CatalogValue::Text(self.name.clone())),
            Some(CatalogValue::U8(self.kind as u8)),
            Some(CatalogValue::Blob(encode_u16_list(&self.column_ordinals))),
            Some(CatalogValue::U8(self.state as u8)),
            Some(CatalogValue::I64(self.created_at)),
        ]
    }

    pub fn from_fields(index_id: u32, fields: Vec<Option<CatalogValue>>) -> Result<Self> {
        let mut fields = fields.into_iter();
        let table_id = expect_u32(&mut fields, "system.indexes.table_id")?;
        let name = expect_text(&mut fields, "system.indexes.name")?;
        let kind = IndexKind::from_u8(expect_u8(&mut fields, "system.indexes.kind")?)?;
        let column_ordinals =
            decode_u16_list(&expect_blob(&mut fields, "system.indexes.column_ordinals")?)?;
        let state = IndexState::from_u8(expect_u8(&mut fields, "system.indexes.state")?)?;
        let created_at = expect_i64(&mut fields, "system.indexes.created_at")?;
        Ok(IndexRow {
            index_id,
            table_id,
            name,
            kind,
            column_ordinals,
            state,
            created_at,
        })
    }
}

// ---------------------------------------------------------------------
// system.constraints
// ---------------------------------------------------------------------

pub const CONSTRAINTS_SCHEMA: [CatalogValueType; 6] = [
    CatalogValueType::U32,
    CatalogValueType::Text,
    CatalogValueType::U8,
    CatalogValueType::Blob,
    CatalogValueType::Text,
    CatalogValueType::U32,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    PrimaryKey = 0,
    Unique = 1,
    NotNull = 2,
    Check = 3,
}

impl ConstraintKind {
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(ConstraintKind::PrimaryKey),
            1 => Ok(ConstraintKind::Unique),
            2 => Ok(ConstraintKind::NotNull),
            3 => Ok(ConstraintKind::Check),
            other => Err(CatalogError::InvalidInput {
                detail: format!("unknown ConstraintKind {other}"),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintRow {
    pub constraint_id: u32,
    pub table_id: u32,
    pub name: String,
    pub kind: ConstraintKind,
    pub column_ordinals: Vec<u16>,
    /// Opaque source text for `Check` constraints, `None` otherwise — D8's
    /// expression engine does not exist yet; never parsed or evaluated by
    /// this increment.
    pub check_expression: Option<String>,
    pub added_in_schema_version: u32,
}

impl ConstraintRow {
    pub fn to_fields(&self) -> Vec<Option<CatalogValue>> {
        vec![
            Some(CatalogValue::U32(self.table_id)),
            Some(CatalogValue::Text(self.name.clone())),
            Some(CatalogValue::U8(self.kind as u8)),
            Some(CatalogValue::Blob(encode_u16_list(&self.column_ordinals))),
            self.check_expression.clone().map(CatalogValue::Text),
            Some(CatalogValue::U32(self.added_in_schema_version)),
        ]
    }

    pub fn from_fields(constraint_id: u32, fields: Vec<Option<CatalogValue>>) -> Result<Self> {
        let mut fields = fields.into_iter();
        let table_id = expect_u32(&mut fields, "system.constraints.table_id")?;
        let name = expect_text(&mut fields, "system.constraints.name")?;
        let kind = ConstraintKind::from_u8(expect_u8(&mut fields, "system.constraints.kind")?)?;
        let column_ordinals = decode_u16_list(&expect_blob(
            &mut fields,
            "system.constraints.column_ordinals",
        )?)?;
        let check_expression = optional_text(&mut fields)?;
        let added_in_schema_version =
            expect_u32(&mut fields, "system.constraints.added_in_schema_version")?;
        Ok(ConstraintRow {
            constraint_id,
            table_id,
            name,
            kind,
            column_ordinals,
            check_expression,
            added_in_schema_version,
        })
    }
}

// ---------------------------------------------------------------------
// system.grants
// ---------------------------------------------------------------------

pub const GRANTS_SCHEMA: [CatalogValueType; 5] = [
    CatalogValueType::Text,
    CatalogValueType::U8,
    CatalogValueType::U32,
    CatalogValueType::U8,
    CatalogValueType::I64,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Database = 0,
    Schema = 1,
    Table = 2,
}

impl ObjectKind {
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(ObjectKind::Database),
            1 => Ok(ObjectKind::Schema),
            2 => Ok(ObjectKind::Table),
            other => Err(CatalogError::InvalidInput {
                detail: format!("unknown ObjectKind {other}"),
            }),
        }
    }
}

/// D25's own enumerated privilege list, in the order stated there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    Select = 0,
    Insert = 1,
    Update = 2,
    Delete = 3,
    Ddl = 4,
    CreateIndex = 5,
}

impl Privilege {
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Privilege::Select),
            1 => Ok(Privilege::Insert),
            2 => Ok(Privilege::Update),
            3 => Ok(Privilege::Delete),
            4 => Ok(Privilege::Ddl),
            5 => Ok(Privilege::CreateIndex),
            other => Err(CatalogError::InvalidInput {
                detail: format!("unknown Privilege {other}"),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRow {
    pub grant_id: u32,
    pub principal: String,
    pub object_kind: ObjectKind,
    pub object_id: u32,
    pub privilege: Privilege,
    pub granted_at: i64,
}

impl GrantRow {
    pub fn to_fields(&self) -> Vec<Option<CatalogValue>> {
        vec![
            Some(CatalogValue::Text(self.principal.clone())),
            Some(CatalogValue::U8(self.object_kind as u8)),
            Some(CatalogValue::U32(self.object_id)),
            Some(CatalogValue::U8(self.privilege as u8)),
            Some(CatalogValue::I64(self.granted_at)),
        ]
    }

    pub fn from_fields(grant_id: u32, fields: Vec<Option<CatalogValue>>) -> Result<Self> {
        let mut fields = fields.into_iter();
        let principal = expect_text(&mut fields, "system.grants.principal")?;
        let object_kind =
            ObjectKind::from_u8(expect_u8(&mut fields, "system.grants.object_kind")?)?;
        let object_id = expect_u32(&mut fields, "system.grants.object_id")?;
        let privilege = Privilege::from_u8(expect_u8(&mut fields, "system.grants.privilege")?)?;
        let granted_at = expect_i64(&mut fields, "system.grants.granted_at")?;
        Ok(GrantRow {
            grant_id,
            principal,
            object_kind,
            object_id,
            privilege,
            granted_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::encoding::{decode_row, encode_row};

    #[test]
    fn database_row_round_trips() {
        let row = DatabaseRow {
            database_id: 1,
            name: "default".to_string(),
            created_at: 1000,
        };
        let encoded = encode_row(1, &row.to_fields());
        let (_, fields) = decode_row(&encoded, &DATABASES_SCHEMA).unwrap();
        assert_eq!(DatabaseRow::from_fields(1, fields).unwrap(), row);
    }

    #[test]
    fn schema_row_round_trips() {
        let row = SchemaRow {
            schema_id: 1,
            database_id: 1,
            name: "public".to_string(),
            created_at: 1000,
        };
        let encoded = encode_row(1, &row.to_fields());
        let (_, fields) = decode_row(&encoded, &SCHEMAS_SCHEMA).unwrap();
        assert_eq!(SchemaRow::from_fields(1, fields).unwrap(), row);
    }

    #[test]
    fn table_row_round_trips_with_composite_pk() {
        let row = TableRow {
            table_id: 3,
            schema_id: 1,
            name: "orders".to_string(),
            pk_ordinals: vec![0, 2],
            schema_version: 1,
            state: TableState::Active,
            created_at: 1000,
        };
        let encoded = encode_row(1, &row.to_fields());
        let (_, fields) = decode_row(&encoded, &TABLES_SCHEMA).unwrap();
        assert_eq!(TableRow::from_fields(3, fields).unwrap(), row);
    }

    #[test]
    fn column_row_round_trips_with_and_without_default() {
        let with_default = ColumnRow {
            table_id: 3,
            ordinal: 0,
            name: "id".to_string(),
            data_type: 2,
            nullable: false,
            default_value: Some(vec![0, 0, 0, 0]),
            added_in_schema_version: 1,
        };
        let encoded = encode_row(1, &with_default.to_fields());
        let (_, fields) = decode_row(&encoded, &COLUMNS_SCHEMA).unwrap();
        assert_eq!(ColumnRow::from_fields(3, 0, fields).unwrap(), with_default);

        let without_default = ColumnRow {
            default_value: None,
            ..with_default
        };
        let encoded = encode_row(1, &without_default.to_fields());
        let (_, fields) = decode_row(&encoded, &COLUMNS_SCHEMA).unwrap();
        assert_eq!(
            ColumnRow::from_fields(3, 0, fields).unwrap(),
            without_default
        );
    }

    #[test]
    fn index_row_round_trips_every_kind() {
        for kind in [IndexKind::Primary, IndexKind::Unique, IndexKind::NonUnique] {
            let row = IndexRow {
                index_id: 1,
                table_id: 3,
                name: "idx".to_string(),
                kind,
                column_ordinals: vec![0],
                state: IndexState::Active,
                created_at: 1000,
            };
            let encoded = encode_row(1, &row.to_fields());
            let (_, fields) = decode_row(&encoded, &INDEXES_SCHEMA).unwrap();
            assert_eq!(IndexRow::from_fields(1, fields).unwrap(), row);
        }
    }

    #[test]
    fn constraint_row_round_trips_check_and_non_check() {
        let check = ConstraintRow {
            constraint_id: 1,
            table_id: 3,
            name: "positive_amount".to_string(),
            kind: ConstraintKind::Check,
            column_ordinals: vec![],
            check_expression: Some("amount > 0".to_string()),
            added_in_schema_version: 1,
        };
        let encoded = encode_row(1, &check.to_fields());
        let (_, fields) = decode_row(&encoded, &CONSTRAINTS_SCHEMA).unwrap();
        assert_eq!(ConstraintRow::from_fields(1, fields).unwrap(), check);

        let pk = ConstraintRow {
            kind: ConstraintKind::PrimaryKey,
            column_ordinals: vec![0],
            check_expression: None,
            ..check
        };
        let encoded = encode_row(1, &pk.to_fields());
        let (_, fields) = decode_row(&encoded, &CONSTRAINTS_SCHEMA).unwrap();
        assert_eq!(ConstraintRow::from_fields(1, fields).unwrap(), pk);
    }

    #[test]
    fn grant_row_round_trips() {
        let row = GrantRow {
            grant_id: 1,
            principal: "admin-key".to_string(),
            object_kind: ObjectKind::Table,
            object_id: 3,
            privilege: Privilege::Select,
            granted_at: 1000,
        };
        let encoded = encode_row(1, &row.to_fields());
        let (_, fields) = decode_row(&encoded, &GRANTS_SCHEMA).unwrap();
        assert_eq!(GrantRow::from_fields(1, fields).unwrap(), row);
    }
}
