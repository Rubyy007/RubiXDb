//! Catalog row/key encoding — `RELATIONAL ADR AMENDMENT 002` CA.2/CA.5,
//! implementing D2's key layout and D3's `RowValue` envelope exactly, for
//! the closed, catalog-scoped value set CA.5 defines. **Not** the full D4
//! type system (no `DECIMAL`/`REAL`/`DOUBLE`/`DATE`/`TIME`, no
//! order-preserving sign-flip/bit-transform key encodings for signed/
//! float types) — nothing in this increment's schema needs one; see
//! CA.5's own Reason for why building it now would be premature.

use crate::catalog::error::{CatalogError, Result};
use std::ops::Bound;

/// D2: `0x00` = system/catalog namespace, `0x01` = relational table/index
/// data. Reserved here, at the one place every catalog key is built, so
/// no catalog code path can ever construct a key outside this namespace
/// by mistake.
pub const CATALOG_NAMESPACE: u8 = 0x00;

/// `RELATIONAL ADR AMENDMENT 002` CA.2's `system_table_id` assignments.
pub const SYSTEM_TABLE_COUNTERS: u32 = 0;
pub const SYSTEM_TABLE_DATABASES: u32 = 1;
pub const SYSTEM_TABLE_SCHEMAS: u32 = 2;
pub const SYSTEM_TABLE_TABLES: u32 = 3;
pub const SYSTEM_TABLE_COLUMNS: u32 = 4;
pub const SYSTEM_TABLE_INDEXES: u32 = 5;
pub const SYSTEM_TABLE_CONSTRAINTS: u32 = 6;
pub const SYSTEM_TABLE_GRANTS: u32 = 7;

/// D3's `RowValue` format version this module reads and writes.
pub const ROW_FORMAT_VERSION: u8 = 1;

/// Builds a catalog row's physical key: `0x00 || system_table_id:u32 BE
/// || pk_bytes` (D2/CA.2). `pk_bytes` must already be the object's own
/// order-preserving primary-key encoding (fixed-width big-endian for
/// every PK this increment defines — CA.2 chose only unsigned fixed-
/// width numeric PKs specifically so no sign-flip/variable-length
/// transform is ever needed here).
pub fn catalog_key(system_table_id: u32, pk_bytes: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 4 + pk_bytes.len());
    key.push(CATALOG_NAMESPACE);
    key.extend_from_slice(&system_table_id.to_be_bytes());
    key.extend_from_slice(pk_bytes);
    key
}

/// The `[start, end)` range covering every row of one `system_table_id`
/// (used for `list_*`-style full-table scans) — `catalog_key(id, &[])`
/// as the inclusive start, `catalog_key(id + 1, &[])` as the exclusive
/// end. `system_table_id = u32::MAX` (never assigned by CA.2, reserved
/// headroom) would overflow on increment; guarded explicitly rather than
/// silently wrapping.
pub fn system_table_range(system_table_id: u32) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let start = catalog_key(system_table_id, &[]);
    match system_table_id.checked_add(1) {
        Some(next) => (
            Bound::Included(start),
            Bound::Excluded(catalog_key(next, &[])),
        ),
        None => (Bound::Included(start), Bound::Unbounded),
    }
}

/// The `[start, end)` range covering every catalog row whose primary key
/// begins with `pk_prefix` within one `system_table_id` — used for
/// composite-PK sub-scans (e.g. `system.columns` filtered to one
/// `table_id`, CA.2). Increments `pk_prefix` as a big-endian number,
/// propagating carry; if `pk_prefix` is all `0xFF` bytes (or empty —
/// callers should use `system_table_range` instead), falls back to
/// `system_table_range`'s own next-`system_table_id` bound so the range
/// still correctly excludes every other prefix within this table.
pub fn system_table_prefix_range(
    system_table_id: u32,
    pk_prefix: &[u8],
) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let start = catalog_key(system_table_id, pk_prefix);
    let mut incremented = pk_prefix.to_vec();
    let mut carried = false;
    for byte in incremented.iter_mut().rev() {
        if *byte == 0xFF {
            *byte = 0x00;
        } else {
            *byte += 1;
            carried = true;
            break;
        }
    }
    if carried {
        (
            Bound::Included(start),
            Bound::Excluded(catalog_key(system_table_id, &incremented)),
        )
    } else {
        // Every byte of `pk_prefix` was 0xFF (or it was empty) — the
        // "next" prefix within this system_table_id doesn't exist as a
        // finite byte string, so the correct exclusive upper bound is
        // wherever this system_table_id's own row range ends.
        let (_, upper) = system_table_range(system_table_id);
        (Bound::Included(start), upper)
    }
}

/// The catalog-scoped closed value set — `RELATIONAL ADR AMENDMENT 002`
/// CA.5. Every field CA.2's system-table schemas use is one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogValue {
    U8(u8),
    U16(u16),
    U32(u32),
    I64(i64),
    Bool(bool),
    Text(String),
    Blob(Vec<u8>),
}

/// The type of one `RowValue` field, independent of its (possibly
/// `NULL`) value — the fixed, per-system-table schema every row struct
/// in `catalog::schema` decodes against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogValueType {
    U8,
    U16,
    U32,
    I64,
    Bool,
    Text,
    Blob,
}

fn null_bitmap_len(column_count: usize) -> usize {
    column_count.div_ceil(8)
}

/// Encodes D3's `RowValue`: `format_version:u8 || schema_version:u32 LE
/// || null_bitmap || values` — `values` holds only the non-`NULL`
/// entries of `fields`, in order (D3: "per non-null column... a `NULL`
/// column consumes zero value bytes, its presence recorded only in the
/// bitmap"). `fields.len()` must equal the row's declared column count
/// (the null bitmap is sized from it) — callers always pass every
/// column, `None` for `NULL`.
pub fn encode_row(schema_version: u32, fields: &[Option<CatalogValue>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(ROW_FORMAT_VERSION);
    out.extend_from_slice(&schema_version.to_le_bytes());

    let mut bitmap = vec![0u8; null_bitmap_len(fields.len())];
    for (i, field) in fields.iter().enumerate() {
        if field.is_none() {
            bitmap[i / 8] |= 1 << (i % 8);
        }
    }
    out.extend_from_slice(&bitmap);

    for field in fields.iter().flatten() {
        encode_value(field, &mut out);
    }
    out
}

fn encode_value(value: &CatalogValue, out: &mut Vec<u8>) {
    match value {
        CatalogValue::U8(v) => out.push(*v),
        CatalogValue::U16(v) => out.extend_from_slice(&v.to_le_bytes()),
        CatalogValue::U32(v) => out.extend_from_slice(&v.to_le_bytes()),
        CatalogValue::I64(v) => out.extend_from_slice(&v.to_le_bytes()),
        CatalogValue::Bool(v) => out.push(if *v { 1 } else { 0 }),
        CatalogValue::Text(v) => {
            let bytes = v.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        CatalogValue::Blob(v) => {
            out.extend_from_slice(&(v.len() as u32).to_le_bytes());
            out.extend_from_slice(v);
        }
    }
}

/// Decodes a `RowValue` against `schema` (the column types, in the exact
/// order `encode_row` was called with). Returns the row's
/// `schema_version` and one `Option<CatalogValue>` per schema entry.
/// Never panics or indexes out of bounds on truncated/malformed input —
/// every read is bounds-checked, returning `CatalogError::InvalidInput`
/// (this module never sees genuinely untrusted bytes — catalog rows are
/// only ever written by this same module — but the engine's own
/// checksum/corruption detection already guards the on-disk bytes
/// themselves; this decoder's contract is "never panic on a short or
/// malformed buffer," matching the WAL/SSTable decoders' own established
/// discipline, not "must reject nation-state-adversarial input").
pub fn decode_row(
    bytes: &[u8],
    schema: &[CatalogValueType],
) -> Result<(u32, Vec<Option<CatalogValue>>)> {
    let mut pos = 0usize;
    let format_version = read_u8(bytes, &mut pos)?;
    if format_version != ROW_FORMAT_VERSION {
        return Err(CatalogError::InvalidInput {
            detail: format!("unsupported RowValue format_version {format_version}"),
        });
    }
    let schema_version = read_u32(bytes, &mut pos)?;

    let bitmap_len = null_bitmap_len(schema.len());
    let bitmap = read_bytes(bytes, &mut pos, bitmap_len)?.to_vec();

    let mut fields = Vec::with_capacity(schema.len());
    for (i, field_type) in schema.iter().enumerate() {
        let is_null = (bitmap[i / 8] >> (i % 8)) & 1 == 1;
        if is_null {
            fields.push(None);
        } else {
            fields.push(Some(decode_value(*field_type, bytes, &mut pos)?));
        }
    }
    Ok((schema_version, fields))
}

fn decode_value(
    field_type: CatalogValueType,
    bytes: &[u8],
    pos: &mut usize,
) -> Result<CatalogValue> {
    Ok(match field_type {
        CatalogValueType::U8 => CatalogValue::U8(read_u8(bytes, pos)?),
        CatalogValueType::U16 => CatalogValue::U16(read_u16(bytes, pos)?),
        CatalogValueType::U32 => CatalogValue::U32(read_u32(bytes, pos)?),
        CatalogValueType::I64 => CatalogValue::I64(read_i64(bytes, pos)?),
        CatalogValueType::Bool => CatalogValue::Bool(read_u8(bytes, pos)? != 0),
        CatalogValueType::Text => {
            let len = read_u32(bytes, pos)? as usize;
            let raw = read_bytes(bytes, pos, len)?;
            CatalogValue::Text(String::from_utf8(raw.to_vec()).map_err(|_| {
                CatalogError::InvalidInput {
                    detail: "TEXT field is not valid UTF-8".to_string(),
                }
            })?)
        }
        CatalogValueType::Blob => {
            let len = read_u32(bytes, pos)? as usize;
            CatalogValue::Blob(read_bytes(bytes, pos, len)?.to_vec())
        }
    })
}

fn read_bytes<'a>(bytes: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| CatalogError::InvalidInput {
            detail: "field length overflow while decoding RowValue".to_string(),
        })?;
    let slice = bytes
        .get(*pos..end)
        .ok_or_else(|| CatalogError::InvalidInput {
            detail: format!("RowValue truncated: need {len} byte(s) at offset {pos}, fewer remain"),
        })?;
    *pos = end;
    Ok(slice)
}

fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8> {
    Ok(read_bytes(bytes, pos, 1)?[0])
}

fn read_u16(bytes: &[u8], pos: &mut usize) -> Result<u16> {
    let raw = read_bytes(bytes, pos, 2)?;
    Ok(u16::from_le_bytes(raw.try_into().expect("exactly 2 bytes")))
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let raw = read_bytes(bytes, pos, 4)?;
    Ok(u32::from_le_bytes(raw.try_into().expect("exactly 4 bytes")))
}

fn read_i64(bytes: &[u8], pos: &mut usize) -> Result<i64> {
    let raw = read_bytes(bytes, pos, 8)?;
    Ok(i64::from_le_bytes(raw.try_into().expect("exactly 8 bytes")))
}

/// Encodes a list of `u16` ordinals as a `CatalogValue::Blob`:
/// `count:u32 LE || (element:u16 LE)*` — CA.2's representation for
/// `system.tables.pk_ordinals` and `system.indexes.column_ordinals`.
pub fn encode_u16_list(items: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + items.len() * 2);
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for item in items {
        out.extend_from_slice(&item.to_le_bytes());
    }
    out
}

/// Decodes the format `encode_u16_list` produces.
pub fn decode_u16_list(bytes: &[u8]) -> Result<Vec<u16>> {
    let mut pos = 0usize;
    let count = read_u32(bytes, &mut pos)? as usize;
    let mut items = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        items.push(read_u16(bytes, &mut pos)?);
    }
    if pos != bytes.len() {
        return Err(CatalogError::InvalidInput {
            detail: format!("{} trailing byte(s) after u16 list", bytes.len() - pos),
        });
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_key_layout_is_namespace_then_table_id_then_pk() {
        let key = catalog_key(SYSTEM_TABLE_TABLES, &7u32.to_be_bytes());
        assert_eq!(key[0], CATALOG_NAMESPACE);
        assert_eq!(&key[1..5], &SYSTEM_TABLE_TABLES.to_be_bytes());
        assert_eq!(&key[5..9], &7u32.to_be_bytes());
    }

    #[test]
    fn system_table_range_excludes_the_next_table() {
        let (start, end) = system_table_range(SYSTEM_TABLE_TABLES);
        let inside = catalog_key(SYSTEM_TABLE_TABLES, &u32::MAX.to_be_bytes());
        let outside = catalog_key(SYSTEM_TABLE_TABLES + 1, &[]);
        match start {
            Bound::Included(s) => assert!(s <= inside),
            other => panic!("expected Included, got {other:?}"),
        }
        match end {
            Bound::Excluded(e) => {
                assert_eq!(e, outside);
                assert!(inside < e);
            }
            other => panic!("expected Excluded, got {other:?}"),
        }
    }

    #[test]
    fn system_table_range_at_u32_max_falls_back_to_unbounded() {
        let (_, end) = system_table_range(u32::MAX);
        assert!(matches!(end, Bound::Unbounded));
    }

    #[test]
    fn prefix_range_isolates_one_table_ids_columns() {
        let table_a = 5u32.to_be_bytes();
        let table_b = 6u32.to_be_bytes();
        let (start, end) = system_table_prefix_range(SYSTEM_TABLE_COLUMNS, &table_a);
        let a_col_0 = catalog_key(
            SYSTEM_TABLE_COLUMNS,
            &[&table_a[..], &0u16.to_be_bytes()].concat(),
        );
        let a_col_max = catalog_key(
            SYSTEM_TABLE_COLUMNS,
            &[&table_a[..], &u16::MAX.to_be_bytes()].concat(),
        );
        let b_col_0 = catalog_key(
            SYSTEM_TABLE_COLUMNS,
            &[&table_b[..], &0u16.to_be_bytes()].concat(),
        );
        let start = match start {
            Bound::Included(s) => s,
            other => panic!("expected Included, got {other:?}"),
        };
        let end = match end {
            Bound::Excluded(e) => e,
            other => panic!("expected Excluded, got {other:?}"),
        };
        assert!(start <= a_col_0 && a_col_0 < end);
        assert!(start <= a_col_max && a_col_max < end);
        assert!(
            b_col_0 >= end,
            "table B's columns must not be inside table A's range"
        );
    }

    #[test]
    fn prefix_range_all_ff_falls_back_to_system_table_upper_bound() {
        let prefix = vec![0xFFu8; 4];
        let (_, end) = system_table_prefix_range(SYSTEM_TABLE_COLUMNS, &prefix);
        let (_, table_end) = system_table_range(SYSTEM_TABLE_COLUMNS);
        assert_eq!(end, table_end);
    }

    #[test]
    fn row_round_trip_all_null() {
        let schema = [
            CatalogValueType::U32,
            CatalogValueType::Text,
            CatalogValueType::Bool,
        ];
        let encoded = encode_row(1, &[None, None, None]);
        let (version, fields) = decode_row(&encoded, &schema).unwrap();
        assert_eq!(version, 1);
        assert_eq!(fields, vec![None, None, None]);
    }

    #[test]
    fn row_round_trip_all_present_every_type() {
        let schema = [
            CatalogValueType::U8,
            CatalogValueType::U16,
            CatalogValueType::U32,
            CatalogValueType::I64,
            CatalogValueType::Bool,
            CatalogValueType::Text,
            CatalogValueType::Blob,
        ];
        let fields = vec![
            Some(CatalogValue::U8(7)),
            Some(CatalogValue::U16(1000)),
            Some(CatalogValue::U32(70_000)),
            Some(CatalogValue::I64(-123_456_789)),
            Some(CatalogValue::Bool(true)),
            Some(CatalogValue::Text("hello, catalog".to_string())),
            Some(CatalogValue::Blob(vec![1, 2, 3, 4, 5])),
        ];
        let encoded = encode_row(42, &fields);
        let (version, decoded) = decode_row(&encoded, &schema).unwrap();
        assert_eq!(version, 42);
        assert_eq!(decoded, fields);
    }

    #[test]
    fn row_round_trip_mixed_null_and_present() {
        let schema = [
            CatalogValueType::U32,
            CatalogValueType::Text,
            CatalogValueType::Bool,
        ];
        let fields = vec![
            Some(CatalogValue::U32(9)),
            None,
            Some(CatalogValue::Bool(false)),
        ];
        let encoded = encode_row(1, &fields);
        let (_, decoded) = decode_row(&encoded, &schema).unwrap();
        assert_eq!(decoded, fields);
    }

    #[test]
    fn null_bitmap_boundary_at_exactly_eight_columns() {
        let schema = vec![CatalogValueType::Bool; 8];
        let fields: Vec<Option<CatalogValue>> = (0..8)
            .map(|i| Some(CatalogValue::Bool(i % 2 == 0)))
            .collect();
        let encoded = encode_row(1, &fields);
        let (_, decoded) = decode_row(&encoded, &schema).unwrap();
        assert_eq!(decoded, fields);
    }

    #[test]
    fn null_bitmap_boundary_at_nine_columns_needs_two_bytes() {
        let schema = vec![CatalogValueType::Bool; 9];
        let mut fields: Vec<Option<CatalogValue>> = vec![Some(CatalogValue::Bool(true)); 9];
        fields[8] = None; // the one bit living in the bitmap's second byte
        let encoded = encode_row(1, &fields);
        let (_, decoded) = decode_row(&encoded, &schema).unwrap();
        assert_eq!(decoded, fields);
    }

    #[test]
    fn decode_rejects_truncated_buffer_without_panicking() {
        let err = decode_row(&[1, 0, 0], &[CatalogValueType::U32]).unwrap_err();
        assert!(matches!(err, CatalogError::InvalidInput { .. }));
    }

    #[test]
    fn decode_rejects_unsupported_format_version() {
        let mut encoded = encode_row(1, &[Some(CatalogValue::Bool(true))]);
        encoded[0] = 99;
        let err = decode_row(&encoded, &[CatalogValueType::Bool]).unwrap_err();
        assert!(matches!(err, CatalogError::InvalidInput { .. }));
    }

    #[test]
    fn u16_list_round_trips_at_zero_one_and_many_elements() {
        for items in [vec![], vec![7u16], (0..500u16).collect::<Vec<_>>()] {
            let encoded = encode_u16_list(&items);
            let decoded = decode_u16_list(&encoded).unwrap();
            assert_eq!(decoded, items);
        }
    }
}
