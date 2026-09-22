//! `RelationalValue` — D4's full type system, and D3's `RowValue`
//! envelope applied to it — `RELATIONAL ADR AMENDMENT 003` RA.1. The
//! envelope itself (`format_version || schema_version || null_bitmap ||
//! values`) is `catalog::encoding::{encode_row_envelope, decode_row_
//! envelope}`, reused verbatim, not reimplemented (RA.1's own Reason).
//! Row-value (non-key) encoding only — order-preserving *key* encoding
//! is `relational::key` (RA.2), since D4 itself treats the two as
//! distinct: "every type has both a row-value encoding... and, for key-
//! bearing columns, an order-preserving key encoding."

use crate::catalog::encoding::{
    decode_row_envelope, encode_row_envelope, read_bytes, read_i128, read_i32, read_i64, read_u32,
    read_u64, read_u8,
};
use crate::relational::error::{RelationalError, Result};

/// D4's closed type set, one variant per SQL type — `RELATIONAL ADR
/// AMENDMENT 003` RA.1. `Decimal` carries `(scaled_value, scale)`
/// together (the scale is per-*value* here; the column's own declared
/// `(precision, scale)` — `catalog::schema::ColumnRow::type_params`,
/// AMENDMENT 003 RA.4 — is what a caller validates a value against
/// before constructing one, not re-derived from the value itself).
#[derive(Debug, Clone, PartialEq)]
pub enum RelationalValue {
    Boolean(bool),
    Integer(i32),
    Bigint(i64),
    Real(f32),
    Double(f64),
    Decimal(i128, u8),
    Text(String),
    Blob(Vec<u8>),
    Date(i32),
    Time(i64),
    Timestamp(i64),
}

/// The type of one `RowValue`/key field, independent of its (possibly
/// `NULL`) value — mirrors `catalog::encoding::CatalogValueType`'s own
/// role, for the full D4 set. `Decimal`'s `scale` travels with the type
/// descriptor (not just the value) because key *decoding* (`relational::
/// key`) must know a column's declared scale to reconstruct a `Decimal`
/// value from key bytes alone, which carry no scale of their own (D4:
/// "sign-flipped big-endian" of the scaled integer only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationalType {
    Boolean,
    Integer,
    Bigint,
    Real,
    Double,
    Decimal { scale: u8 },
    Text,
    Blob,
    Date,
    Time,
    Timestamp,
}

/// D4's own raw type tags (`system.columns.data_type`, CA.2/CA.5) — the
/// coarse type identity a catalog column row stores; `Decimal`'s scale
/// lives separately in `type_params` (RA.4), not in this tag.
pub const TYPE_TAG_BOOLEAN: u8 = 1;
pub const TYPE_TAG_INTEGER: u8 = 2;
pub const TYPE_TAG_BIGINT: u8 = 3;
pub const TYPE_TAG_REAL: u8 = 4;
pub const TYPE_TAG_DOUBLE: u8 = 5;
pub const TYPE_TAG_DECIMAL: u8 = 6;
pub const TYPE_TAG_TEXT: u8 = 7;
pub const TYPE_TAG_BLOB: u8 = 8;
pub const TYPE_TAG_DATE: u8 = 9;
pub const TYPE_TAG_TIME: u8 = 10;
pub const TYPE_TAG_TIMESTAMP: u8 = 11;

/// `RELATIONAL ADR AMENDMENT 003` RA.4: bounded `DECIMAL` precision — an
/// `i128`'s own natural limit (~38 decimal digits), not an arbitrary
/// choice.
pub const MAX_DECIMAL_PRECISION: u8 = 38;

/// D3's `RowValue`, applied to `RelationalValue` — see this module's own
/// doc comment for why the envelope itself is reused, not reimplemented.
pub fn encode_row(schema_version: u32, fields: &[Option<RelationalValue>]) -> Vec<u8> {
    encode_row_envelope(schema_version, fields, encode_value)
}

fn encode_value(value: &RelationalValue, out: &mut Vec<u8>) {
    match value {
        RelationalValue::Boolean(v) => out.push(if *v { 1 } else { 0 }),
        RelationalValue::Integer(v) => out.extend_from_slice(&v.to_le_bytes()),
        RelationalValue::Bigint(v) => out.extend_from_slice(&v.to_le_bytes()),
        RelationalValue::Real(v) => out.extend_from_slice(&v.to_bits().to_le_bytes()),
        RelationalValue::Double(v) => out.extend_from_slice(&v.to_bits().to_le_bytes()),
        RelationalValue::Decimal(v, scale) => {
            out.extend_from_slice(&v.to_le_bytes());
            out.push(*scale);
        }
        RelationalValue::Text(v) => {
            let bytes = v.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        RelationalValue::Blob(v) => {
            out.extend_from_slice(&(v.len() as u32).to_le_bytes());
            out.extend_from_slice(v);
        }
        RelationalValue::Date(v) => out.extend_from_slice(&v.to_le_bytes()),
        RelationalValue::Time(v) => out.extend_from_slice(&v.to_le_bytes()),
        RelationalValue::Timestamp(v) => out.extend_from_slice(&v.to_le_bytes()),
    }
}

/// Decodes a `RowValue` against `schema` (in the exact order `encode_
/// row` was called with). Never panics on truncated/malformed input —
/// every read is bounds-checked (the same `catalog::encoding` primitive
/// readers, reused, not reimplemented).
pub fn decode_row(
    bytes: &[u8],
    schema: &[RelationalType],
) -> Result<(u32, Vec<Option<RelationalValue>>)> {
    Ok(decode_row_envelope(
        bytes,
        schema.len(),
        |i, bytes, pos| decode_value(schema[i], bytes, pos),
    )?)
}

fn decode_value(
    field_type: RelationalType,
    bytes: &[u8],
    pos: &mut usize,
) -> crate::catalog::error::Result<RelationalValue> {
    Ok(match field_type {
        RelationalType::Boolean => RelationalValue::Boolean(read_u8(bytes, pos)? != 0),
        RelationalType::Integer => RelationalValue::Integer(read_i32(bytes, pos)?),
        RelationalType::Bigint => RelationalValue::Bigint(read_i64(bytes, pos)?),
        RelationalType::Real => RelationalValue::Real(f32::from_bits(read_u32(bytes, pos)?)),
        RelationalType::Double => RelationalValue::Double(f64::from_bits(read_u64(bytes, pos)?)),
        RelationalType::Decimal { .. } => {
            let v = read_i128(bytes, pos)?;
            let scale = read_u8(bytes, pos)?;
            RelationalValue::Decimal(v, scale)
        }
        RelationalType::Text => {
            let len = read_u32(bytes, pos)? as usize;
            let raw = read_bytes(bytes, pos, len)?;
            RelationalValue::Text(String::from_utf8(raw.to_vec()).map_err(|_| {
                crate::catalog::error::CatalogError::InvalidInput {
                    detail: "TEXT field is not valid UTF-8".to_string(),
                }
            })?)
        }
        RelationalType::Blob => {
            let len = read_u32(bytes, pos)? as usize;
            RelationalValue::Blob(read_bytes(bytes, pos, len)?.to_vec())
        }
        RelationalType::Date => RelationalValue::Date(read_i32(bytes, pos)?),
        RelationalType::Time => RelationalValue::Time(read_i64(bytes, pos)?),
        RelationalType::Timestamp => RelationalValue::Timestamp(read_i64(bytes, pos)?),
    })
}

/// The `RelationalType` of a `RelationalValue` — used by callers that
/// have a value but need its type descriptor (e.g. to validate it
/// against a column's declared type before storing it).
impl RelationalValue {
    pub fn value_type(&self) -> RelationalType {
        match self {
            RelationalValue::Boolean(_) => RelationalType::Boolean,
            RelationalValue::Integer(_) => RelationalType::Integer,
            RelationalValue::Bigint(_) => RelationalType::Bigint,
            RelationalValue::Real(_) => RelationalType::Real,
            RelationalValue::Double(_) => RelationalType::Double,
            RelationalValue::Decimal(_, scale) => RelationalType::Decimal { scale: *scale },
            RelationalValue::Text(_) => RelationalType::Text,
            RelationalValue::Blob(_) => RelationalType::Blob,
            RelationalValue::Date(_) => RelationalType::Date,
            RelationalValue::Time(_) => RelationalType::Time,
            RelationalValue::Timestamp(_) => RelationalType::Timestamp,
        }
    }
}

/// `RELATIONAL ADR AMENDMENT 003` RA.4: validates a `Decimal`'s scaled
/// value fits within its declared `(precision, scale)` — `10^precision`
/// is the exclusive bound on `|scaled_value|` (a `DECIMAL(p,s)` holds at
/// most `p` total decimal digits). Bounded `precision <= 38` (`i128`'s
/// own natural limit, RA.2).
pub fn validate_decimal(value: i128, precision: u8, scale: u8) -> Result<()> {
    if precision == 0 || precision > MAX_DECIMAL_PRECISION {
        return Err(RelationalError::InvalidInput {
            detail: format!(
                "DECIMAL precision must be in 1..={MAX_DECIMAL_PRECISION}, got {precision}"
            ),
        });
    }
    if scale > precision {
        return Err(RelationalError::InvalidInput {
            detail: format!("DECIMAL scale ({scale}) must not exceed precision ({precision})"),
        });
    }
    let bound =
        10i128
            .checked_pow(precision as u32)
            .ok_or_else(|| RelationalError::InvalidInput {
                detail: format!("DECIMAL precision {precision} overflows i128"),
            })?;
    if value.unsigned_abs() >= bound.unsigned_abs() {
        return Err(RelationalError::InvalidInput {
            detail: format!("DECIMAL value {value} exceeds precision {precision}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_round_trip_every_type_present() {
        let schema = [
            RelationalType::Boolean,
            RelationalType::Integer,
            RelationalType::Bigint,
            RelationalType::Real,
            RelationalType::Double,
            RelationalType::Decimal { scale: 2 },
            RelationalType::Text,
            RelationalType::Blob,
            RelationalType::Date,
            RelationalType::Time,
            RelationalType::Timestamp,
        ];
        let fields = vec![
            Some(RelationalValue::Boolean(true)),
            Some(RelationalValue::Integer(-42)),
            Some(RelationalValue::Bigint(-9_000_000_000)),
            Some(RelationalValue::Real(3.5)),
            Some(RelationalValue::Double(-2.25)),
            Some(RelationalValue::Decimal(12345, 2)),
            Some(RelationalValue::Text("hello row".to_string())),
            Some(RelationalValue::Blob(vec![9, 8, 7])),
            Some(RelationalValue::Date(19000)),
            Some(RelationalValue::Time(3_600_000_000)),
            Some(RelationalValue::Timestamp(-1_000_000)),
        ];
        let encoded = encode_row(1, &fields);
        let (version, decoded) = decode_row(&encoded, &schema).unwrap();
        assert_eq!(version, 1);
        assert_eq!(decoded, fields);
    }

    #[test]
    fn row_round_trip_all_null() {
        let schema = [RelationalType::Integer, RelationalType::Text];
        let encoded = encode_row(1, &[None, None]);
        let (_, decoded) = decode_row(&encoded, &schema).unwrap();
        assert_eq!(decoded, vec![None, None]);
    }

    #[test]
    fn decode_rejects_truncated_buffer_without_panicking() {
        let err = decode_row(&[1, 0, 0], &[RelationalType::Bigint]).unwrap_err();
        assert!(matches!(err, RelationalError::Catalog(_)));
    }

    #[test]
    fn validate_decimal_accepts_in_range_rejects_out_of_range() {
        assert!(validate_decimal(999, 3, 0).is_ok());
        assert!(
            validate_decimal(1000, 3, 0).is_err(),
            "1000 has 4 digits, exceeds precision 3"
        );
        assert!(validate_decimal(-999, 3, 0).is_ok());
        assert!(validate_decimal(-1000, 3, 0).is_err());
        assert!(
            validate_decimal(0, 3, 5).is_err(),
            "scale must not exceed precision"
        );
        assert!(validate_decimal(0, 0, 0).is_err(), "precision must be >= 1");
        assert!(
            validate_decimal(0, 39, 0).is_err(),
            "precision must be <= 38"
        );
    }
}
