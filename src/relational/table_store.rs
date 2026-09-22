//! `TableStore` — row-level put/get/delete/scan primitives, resolving a
//! table's shape from the certified `CatalogService` (never a second,
//! duplicated metadata structure — the catalog remains the source of
//! truth) and writing through the certified `LsmEngine`/`write_batch`
//! exclusively. `RELATIONAL ADR AMENDMENT 003` RA.3/RA.5.

use std::collections::HashSet;
use std::ops::Bound;
use std::sync::Arc;

use crate::catalog::schema::{ColumnRow, TableRow};
use crate::catalog::CatalogService;
use crate::lsm::{LsmEngine, WriteOp};
use crate::relational::error::{RelationalError, Result};
use crate::relational::key::{
    decode_composite_key, encode_composite_key, table_row_key, table_row_range,
};
use crate::relational::value::{
    decode_row, encode_row, RelationalType, RelationalValue, TYPE_TAG_BIGINT, TYPE_TAG_BLOB,
    TYPE_TAG_BOOLEAN, TYPE_TAG_DATE, TYPE_TAG_DECIMAL, TYPE_TAG_DOUBLE, TYPE_TAG_INTEGER,
    TYPE_TAG_REAL, TYPE_TAG_TEXT, TYPE_TAG_TIME, TYPE_TAG_TIMESTAMP,
};

/// `RELATIONAL ADR AMENDMENT 003` RA.6 / Architecture doc §4: reused,
/// not reinvented — matches the flat-KV API's own `max_value_bytes`
/// default exactly.
pub const MAX_ROW_VALUE_BYTES: usize = 1024 * 1024;

/// One full logical row: one `Option<RelationalValue>` per column, in
/// column-ordinal order (`None` = `NULL`).
pub type Row = Vec<Option<RelationalValue>>;

pub struct TableStore {
    engine: Arc<LsmEngine>,
    catalog: Arc<CatalogService>,
}

impl TableStore {
    pub fn new(engine: Arc<LsmEngine>, catalog: Arc<CatalogService>) -> Self {
        TableStore { engine, catalog }
    }

    fn resolve_table(&self, table_id: u32) -> Result<(TableRow, Vec<ColumnRow>)> {
        let table = self
            .catalog
            .get_table(table_id)?
            .ok_or_else(|| RelationalError::NotFound {
                object: format!("table {table_id}"),
            })?;
        let columns = self.catalog.get_columns(table_id)?;
        Ok((table, columns))
    }

    fn pk_ordinal_set(table: &TableRow) -> HashSet<u16> {
        table.pk_ordinals.iter().copied().collect()
    }

    /// Row-level `INSERT`/`UPSERT`-shaped write — `RELATIONAL ADR
    /// AMENDMENT 001`'s certified `write_batch` primitive, called
    /// exactly once, even at N=1 (RA.5's own reasoning). Returns the
    /// durable sequence the row committed at.
    pub fn put_row(&self, table_id: u32, values: &[Option<RelationalValue>]) -> Result<u64> {
        let (table, columns) = self.resolve_table(table_id)?;
        validate_row_shape(&table, &columns, values)?;
        let op = build_put_op(&table, &columns, values)?;
        Ok(self.engine.write_batch(&[op])?)
    }

    /// A genuinely multi-row atomic write through one `write_batch`
    /// call — RA.5's own explicit reason for existing: a direct,
    /// SQL-free way to exercise `write_batch`'s multi-row visibility
    /// guarantee through row storage specifically.
    pub fn put_rows(&self, table_id: u32, rows: &[Vec<Option<RelationalValue>>]) -> Result<u64> {
        if rows.is_empty() {
            return Err(RelationalError::InvalidInput {
                detail: "put_rows requires at least one row".to_string(),
            });
        }
        let (table, columns) = self.resolve_table(table_id)?;
        let mut ops = Vec::with_capacity(rows.len());
        for values in rows {
            validate_row_shape(&table, &columns, values)?;
            ops.push(build_put_op(&table, &columns, values)?);
        }
        Ok(self.engine.write_batch(&ops)?)
    }

    pub fn get_row(&self, table_id: u32, pk_values: &[RelationalValue]) -> Result<Option<Row>> {
        let (table, columns) = self.resolve_table(table_id)?;
        if pk_values.len() != table.pk_ordinals.len() {
            return Err(RelationalError::InvalidInput {
                detail: format!(
                    "table {table_id} has a {}-column primary key, got {} value(s)",
                    table.pk_ordinals.len(),
                    pk_values.len()
                ),
            });
        }
        let encoded_pk = encode_composite_key(pk_values)?;
        let key = table_row_key(table_id, &encoded_pk);
        match self.engine.get(&key)? {
            None => Ok(None),
            Some(value_bytes) => Ok(Some(decode_full_row(
                &table,
                &columns,
                pk_values,
                &value_bytes,
            )?)),
        }
    }

    /// `write_batch` even for one physical key removed — RA.5's own
    /// reasoning (an operation that already has index maintenance added
    /// to it later never needs its call shape to change).
    pub fn delete_row(&self, table_id: u32, pk_values: &[RelationalValue]) -> Result<u64> {
        let (table, _columns) = self.resolve_table(table_id)?;
        if pk_values.len() != table.pk_ordinals.len() {
            return Err(RelationalError::InvalidInput {
                detail: format!(
                    "table {table_id} has a {}-column primary key, got {} value(s)",
                    table.pk_ordinals.len(),
                    pk_values.len()
                ),
            });
        }
        let encoded_pk = encode_composite_key(pk_values)?;
        let key = table_row_key(table_id, &encoded_pk);
        Ok(self.engine.write_batch(&[WriteOp::Delete { key }])?)
    }

    /// A table-scoped range scan (D2's physical namespace/`table_id`
    /// prefix) — never returns another table's rows (verified as actual
    /// physical range boundaries by this module's tests, not only
    /// post-filtering). Returns `(primary_key_values, full_row)` pairs
    /// in primary-key order.
    pub fn scan_table(&self, table_id: u32) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        let (table, columns) = self.resolve_table(table_id)?;
        let pk_types = pk_column_types(&table, &columns)?;
        let (start, end) = table_row_range(table_id);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            let pk_bytes = key.get(9..).ok_or_else(|| RelationalError::InvalidInput {
                detail: "table row key shorter than the fixed 9-byte header".to_string(),
            })?;
            let pk_values = decode_composite_key(&pk_types, pk_bytes)?;
            let full_row = decode_full_row(&table, &columns, &pk_values, &value)?;
            out.push((pk_values, full_row));
        }
        Ok(out)
    }
}

fn pk_column_types(table: &TableRow, columns: &[ColumnRow]) -> Result<Vec<RelationalType>> {
    table
        .pk_ordinals
        .iter()
        .map(|&ord| {
            let column = columns.iter().find(|c| c.ordinal == ord).ok_or_else(|| {
                RelationalError::InvalidInput {
                    detail: format!("primary-key ordinal {ord} has no matching column"),
                }
            })?;
            relational_type_from_column(column)
        })
        .collect()
}

fn relational_type_from_column(column: &ColumnRow) -> Result<RelationalType> {
    Ok(match column.data_type {
        TYPE_TAG_BOOLEAN => RelationalType::Boolean,
        TYPE_TAG_INTEGER => RelationalType::Integer,
        TYPE_TAG_BIGINT => RelationalType::Bigint,
        TYPE_TAG_REAL => RelationalType::Real,
        TYPE_TAG_DOUBLE => RelationalType::Double,
        TYPE_TAG_DECIMAL => {
            let params =
                column
                    .type_params
                    .as_ref()
                    .ok_or_else(|| RelationalError::InvalidInput {
                        detail: format!(
                            "column {:?} is DECIMAL but has no type_params",
                            column.name
                        ),
                    })?;
            let &[_precision, scale] = params.as_slice() else {
                return Err(RelationalError::InvalidInput {
                    detail: format!(
                        "column {:?}'s DECIMAL type_params must be exactly 2 bytes",
                        column.name
                    ),
                });
            };
            RelationalType::Decimal { scale }
        }
        TYPE_TAG_TEXT => RelationalType::Text,
        TYPE_TAG_BLOB => RelationalType::Blob,
        TYPE_TAG_DATE => RelationalType::Date,
        TYPE_TAG_TIME => RelationalType::Time,
        TYPE_TAG_TIMESTAMP => RelationalType::Timestamp,
        other => {
            return Err(RelationalError::InvalidInput {
                detail: format!("column {:?} has unknown data_type tag {other}", column.name),
            })
        }
    })
}

fn check_value_matches_type(
    value: &RelationalValue,
    expected: RelationalType,
    column_name: &str,
) -> Result<()> {
    let matches = matches!(
        (value, expected),
        (RelationalValue::Boolean(_), RelationalType::Boolean)
            | (RelationalValue::Integer(_), RelationalType::Integer)
            | (RelationalValue::Bigint(_), RelationalType::Bigint)
            | (RelationalValue::Real(_), RelationalType::Real)
            | (RelationalValue::Double(_), RelationalType::Double)
            | (RelationalValue::Text(_), RelationalType::Text)
            | (RelationalValue::Blob(_), RelationalType::Blob)
            | (RelationalValue::Date(_), RelationalType::Date)
            | (RelationalValue::Time(_), RelationalType::Time)
            | (RelationalValue::Timestamp(_), RelationalType::Timestamp)
    ) || matches!(
        (value, expected),
        (RelationalValue::Decimal(_, scale), RelationalType::Decimal { scale: expected_scale })
            if *scale == expected_scale
    );
    if matches {
        Ok(())
    } else {
        Err(RelationalError::InvalidInput {
            detail: format!(
                "column {column_name:?}: value does not match its declared column type"
            ),
        })
    }
}

fn validate_row_shape(
    table: &TableRow,
    columns: &[ColumnRow],
    values: &[Option<RelationalValue>],
) -> Result<()> {
    if values.len() != columns.len() {
        return Err(RelationalError::InvalidInput {
            detail: format!(
                "expected {} value(s) (one per column), got {}",
                columns.len(),
                values.len()
            ),
        });
    }
    let pk_set = TableStore::pk_ordinal_set(table);
    for column in columns {
        let value =
            values
                .get(column.ordinal as usize)
                .ok_or_else(|| RelationalError::InvalidInput {
                    detail: format!(
                        "no value supplied for column {:?} (ordinal {})",
                        column.name, column.ordinal
                    ),
                })?;
        let is_pk = pk_set.contains(&column.ordinal);
        match value {
            None => {
                if is_pk {
                    return Err(RelationalError::InvalidInput {
                        detail: format!("primary-key column {:?} must not be NULL", column.name),
                    });
                }
                if !column.nullable {
                    return Err(RelationalError::InvalidInput {
                        detail: format!("column {:?} is NOT NULL", column.name),
                    });
                }
            }
            Some(v) => {
                let expected = relational_type_from_column(column)?;
                check_value_matches_type(v, expected, &column.name)?;
                if let (RelationalValue::Decimal(dv, _), Some(params)) = (v, &column.type_params) {
                    if let &[precision, scale] = params.as_slice() {
                        crate::relational::value::validate_decimal(*dv, precision, scale)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn build_put_op(
    table: &TableRow,
    columns: &[ColumnRow],
    values: &[Option<RelationalValue>],
) -> Result<WriteOp> {
    let pk_set = TableStore::pk_ordinal_set(table);
    let pk_values: Vec<RelationalValue> = table
        .pk_ordinals
        .iter()
        .map(|&ord| {
            values[ord as usize]
                .clone()
                .expect("validated: primary-key columns are never NULL")
        })
        .collect();
    let encoded_pk = encode_composite_key(&pk_values)?;
    let key = table_row_key(table.table_id, &encoded_pk);

    let non_pk_fields: Vec<Option<RelationalValue>> = columns
        .iter()
        .filter(|c| !pk_set.contains(&c.ordinal))
        .map(|c| values[c.ordinal as usize].clone())
        .collect();
    let row_bytes = encode_row(table.schema_version, &non_pk_fields);
    if row_bytes.len() > MAX_ROW_VALUE_BYTES {
        return Err(RelationalError::InvalidInput {
            detail: format!(
                "row for table {} exceeds max row size ({} > {MAX_ROW_VALUE_BYTES} bytes)",
                table.table_id,
                row_bytes.len()
            ),
        });
    }
    Ok(WriteOp::Put {
        key,
        value: row_bytes,
    })
}

/// D3: primary-key columns are never redundantly stored inside
/// `RowValue` — reconstructs the complete, column-ordinal-ordered row by
/// merging the already-known `pk_values` back in among the decoded
/// non-primary-key fields.
fn decode_full_row(
    table: &TableRow,
    columns: &[ColumnRow],
    pk_values: &[RelationalValue],
    value_bytes: &[u8],
) -> Result<Row> {
    let pk_set = TableStore::pk_ordinal_set(table);
    let non_pk_types: Vec<RelationalType> = columns
        .iter()
        .filter(|c| !pk_set.contains(&c.ordinal))
        .map(relational_type_from_column)
        .collect::<Result<_>>()?;
    let (_, non_pk_fields) = decode_row(value_bytes, &non_pk_types)?;

    let mut non_pk_iter = non_pk_fields.into_iter();
    let mut result = Vec::with_capacity(columns.len());
    for column in columns {
        if pk_set.contains(&column.ordinal) {
            let idx = table
                .pk_ordinals
                .iter()
                .position(|&o| o == column.ordinal)
                .ok_or_else(|| RelationalError::InvalidInput {
                    detail: format!(
                        "primary-key ordinal {} not found in pk_ordinals",
                        column.ordinal
                    ),
                })?;
            result.push(Some(pk_values[idx].clone()));
        } else {
            result.push(
                non_pk_iter
                    .next()
                    .ok_or_else(|| RelationalError::InvalidInput {
                        detail: "fewer decoded non-primary-key fields than expected".to_string(),
                    })?,
            );
        }
    }
    Ok(result)
}

fn as_bound_ref(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}
