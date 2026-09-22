//! Order-preserving key encoding — `RELATIONAL ADR AMENDMENT 003` RA.2.
//! The governing invariant, verified by property tests below, not
//! inspection alone: `byte_lexicographic_compare(encode(a), encode(b))`
//! must equal `logical_compare(a, b)` for every valid key value.
//!
//! Used only for primary-key columns in this increment (D6: the primary
//! key *is* the physical row key). D5 forbids `NULL` primary-key
//! columns, so no path here ever encodes/decodes a `NULL`.

use std::ops::Bound;

use crate::relational::error::{RelationalError, Result};
use crate::relational::value::{RelationalType, RelationalValue};

/// D2: `0x01` = relational table/index data namespace (`0x00` is D1's
/// catalog namespace, `catalog::encoding::CATALOG_NAMESPACE`).
pub const RELATIONAL_NAMESPACE: u8 = 0x01;

/// RA.3 / Architecture document §5, implemented exactly: `0x01 ||
/// table_id:u32 BE || 0x00000000:u32 BE || encoded_pk_columns` — the
/// `0x00000000` is the reserved `index_id = 0` slot the Architecture
/// document assigns to "the table itself," keeping this layout
/// structurally uniform with a future increment's index-entry-key
/// layout (same position, a real `index_id > 0`).
pub fn table_row_key(table_id: u32, encoded_pk: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 4 + 4 + encoded_pk.len());
    key.push(RELATIONAL_NAMESPACE);
    key.extend_from_slice(&table_id.to_be_bytes());
    key.extend_from_slice(&0u32.to_be_bytes());
    key.extend_from_slice(encoded_pk);
    key
}

/// The `[start, end)` range covering every row of one table — bounded to
/// exactly the `index_id = 0` slot (`table_row_key`'s own reserved
/// meaning), **not** the whole `table_id` prefix. `PHASE_RELATIONAL_
/// INDEX_BACKFILL_ADR.md` §1 corrects this function: before secondary
/// indexes existed, `table_id`'s prefix contained nothing but `index_id =
/// 0` rows, so bounding by "the next `table_id`" was harmlessly
/// equivalent to bounding by "the next `index_id`." Once a secondary
/// index (`index_id > 0`) physically lives under the *same* `table_id`
/// prefix (`relational::index_key::index_entry_key`), that equivalence
/// breaks: the old bound would silently include every index's entries in
/// a plain table scan. Bounding to `[table_id||0, table_id||1)` instead
/// makes this structurally impossible, matching `catalog::encoding::
/// system_table_range`'s own "never trust a wider bound than the data's
/// own reserved slot" discipline, one level deeper.
pub fn table_row_range(table_id: u32) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let start = table_row_key(table_id, &[]);
    let mut end = Vec::with_capacity(1 + 4 + 4);
    end.push(RELATIONAL_NAMESPACE);
    end.extend_from_slice(&table_id.to_be_bytes());
    end.extend_from_slice(&1u32.to_be_bytes());
    (Bound::Included(start), Bound::Excluded(end))
}

/// RA.2: encodes one key-bearing value's order-preserving representation
/// into `out`. Rejects `NaN` (D4: "NaN excluded from key-bearing
/// columns") and a negative `TIME` (D4: "non-negative") before writing
/// anything.
pub fn encode_key_value(value: &RelationalValue, out: &mut Vec<u8>) -> Result<()> {
    match value {
        RelationalValue::Boolean(v) => out.push(if *v { 1 } else { 0 }),
        RelationalValue::Integer(v) => out.extend_from_slice(&sign_flip_32(*v)),
        RelationalValue::Bigint(v) => out.extend_from_slice(&sign_flip_64(*v)),
        RelationalValue::Date(v) => out.extend_from_slice(&sign_flip_32(*v)),
        RelationalValue::Timestamp(v) => out.extend_from_slice(&sign_flip_64(*v)),
        RelationalValue::Time(v) => {
            if *v < 0 {
                return Err(RelationalError::InvalidInput {
                    detail: format!("TIME must be non-negative, got {v}"),
                });
            }
            out.extend_from_slice(&v.to_be_bytes());
        }
        RelationalValue::Decimal(v, _scale) => out.extend_from_slice(&sign_flip_128(*v)),
        RelationalValue::Real(v) => out.extend_from_slice(&monotonic_f32(*v)?),
        RelationalValue::Double(v) => out.extend_from_slice(&monotonic_f64(*v)?),
        RelationalValue::Text(v) => encode_escaped_terminated(v.as_bytes(), out),
        RelationalValue::Blob(v) => encode_escaped_terminated(v, out),
    }
    Ok(())
}

/// RA.2's composite-key rule: each column's order-preserving encoding,
/// concatenated in declared column order — unambiguous because every
/// fixed-width component occupies a known byte span and every `TEXT`/
/// `BLOB` component's own terminator delimits it from whatever follows.
pub fn encode_composite_key(values: &[RelationalValue]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for v in values {
        encode_key_value(v, &mut out)?;
    }
    Ok(out)
}

/// The inverse of `encode_composite_key` — reconstructs the original
/// `RelationalValue`s from a composite key's bytes, given the declared
/// column types in the same order. Needed because D3 recovers primary-
/// key column values from the physical key alone (never redundantly
/// stored in `RowValue`) — a full row read must decode them back.
pub fn decode_composite_key(
    types: &[RelationalType],
    bytes: &[u8],
) -> Result<Vec<RelationalValue>> {
    let mut pos = 0usize;
    let mut out = Vec::with_capacity(types.len());
    for t in types {
        out.push(decode_key_value(*t, bytes, &mut pos)?);
    }
    if pos != bytes.len() {
        return Err(RelationalError::InvalidInput {
            detail: format!("{} trailing byte(s) after composite key", bytes.len() - pos),
        });
    }
    Ok(out)
}

/// `pub(crate)`: reused by `relational::index_key` to decode one indexed
/// column's value after consuming its NULL/present presence tag (the
/// primary-key composite-key decode path above never needs a presence
/// tag, since PK columns are never `NULL` — D5).
pub(crate) fn decode_key_value(
    field_type: RelationalType,
    bytes: &[u8],
    pos: &mut usize,
) -> Result<RelationalValue> {
    Ok(match field_type {
        RelationalType::Boolean => RelationalValue::Boolean(take(bytes, pos, 1)?[0] != 0),
        RelationalType::Integer => RelationalValue::Integer(unsign_flip_32(take(bytes, pos, 4)?)),
        RelationalType::Bigint => RelationalValue::Bigint(unsign_flip_64(take(bytes, pos, 8)?)),
        RelationalType::Date => RelationalValue::Date(unsign_flip_32(take(bytes, pos, 4)?)),
        RelationalType::Timestamp => {
            RelationalValue::Timestamp(unsign_flip_64(take(bytes, pos, 8)?))
        }
        RelationalType::Time => {
            let raw = take(bytes, pos, 8)?;
            RelationalValue::Time(i64::from_be_bytes(
                raw.try_into().expect("checked length 8"),
            ))
        }
        RelationalType::Decimal { scale } => {
            RelationalValue::Decimal(unsign_flip_128(take(bytes, pos, 16)?), scale)
        }
        RelationalType::Real => RelationalValue::Real(unmonotonic_f32(take(bytes, pos, 4)?)),
        RelationalType::Double => RelationalValue::Double(unmonotonic_f64(take(bytes, pos, 8)?)),
        RelationalType::Text => {
            let raw = decode_escaped_terminated(bytes, pos)?;
            RelationalValue::Text(String::from_utf8(raw).map_err(|_| {
                RelationalError::InvalidInput {
                    detail: "TEXT key field is not valid UTF-8".to_string(),
                }
            })?)
        }
        RelationalType::Blob => RelationalValue::Blob(decode_escaped_terminated(bytes, pos)?),
    })
}

fn take<'a>(bytes: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| RelationalError::InvalidInput {
            detail: "key field length overflow".to_string(),
        })?;
    let slice = bytes
        .get(*pos..end)
        .ok_or_else(|| RelationalError::InvalidInput {
            detail: format!("key truncated: need {len} byte(s) at offset {pos}, fewer remain"),
        })?;
    *pos = end;
    Ok(slice)
}

// --- Fixed-width sign-flip transforms (RA.2) ---

fn sign_flip_32(v: i32) -> [u8; 4] {
    ((v as u32) ^ 0x8000_0000).to_be_bytes()
}

fn unsign_flip_32(raw: &[u8]) -> i32 {
    let transformed = u32::from_be_bytes(raw.try_into().expect("checked length 4"));
    (transformed ^ 0x8000_0000) as i32
}

fn sign_flip_64(v: i64) -> [u8; 8] {
    ((v as u64) ^ 0x8000_0000_0000_0000).to_be_bytes()
}

fn unsign_flip_64(raw: &[u8]) -> i64 {
    let transformed = u64::from_be_bytes(raw.try_into().expect("checked length 8"));
    (transformed ^ 0x8000_0000_0000_0000) as i64
}

fn sign_flip_128(v: i128) -> [u8; 16] {
    ((v as u128) ^ (1u128 << 127)).to_be_bytes()
}

fn unsign_flip_128(raw: &[u8]) -> i128 {
    let transformed = u128::from_be_bytes(raw.try_into().expect("checked length 16"));
    (transformed ^ (1u128 << 127)) as i128
}

// --- IEEE-754 monotonic bit-transforms (RA.2) ---

fn monotonic_f32(v: f32) -> Result<[u8; 4]> {
    if v.is_nan() {
        return Err(RelationalError::InvalidInput {
            detail: "NaN is not allowed in a key-bearing REAL column".to_string(),
        });
    }
    // RA.2: canonicalize -0.0 to +0.0 before the transform — `==`
    // already treats them equal, so this reassignment forces the
    // positive-zero bit pattern regardless of which was passed in.
    let v = if v == 0.0 { 0.0f32 } else { v };
    let bits = v.to_bits();
    let transformed = if bits & 0x8000_0000 != 0 {
        !bits
    } else {
        bits | 0x8000_0000
    };
    Ok(transformed.to_be_bytes())
}

fn unmonotonic_f32(raw: &[u8]) -> f32 {
    let transformed = u32::from_be_bytes(raw.try_into().expect("checked length 4"));
    let bits = if transformed & 0x8000_0000 != 0 {
        transformed & 0x7FFF_FFFF
    } else {
        !transformed
    };
    f32::from_bits(bits)
}

fn monotonic_f64(v: f64) -> Result<[u8; 8]> {
    if v.is_nan() {
        return Err(RelationalError::InvalidInput {
            detail: "NaN is not allowed in a key-bearing DOUBLE column".to_string(),
        });
    }
    let v = if v == 0.0 { 0.0f64 } else { v };
    let bits = v.to_bits();
    let transformed = if bits & 0x8000_0000_0000_0000 != 0 {
        !bits
    } else {
        bits | 0x8000_0000_0000_0000
    };
    Ok(transformed.to_be_bytes())
}

fn unmonotonic_f64(raw: &[u8]) -> f64 {
    let transformed = u64::from_be_bytes(raw.try_into().expect("checked length 8"));
    let bits = if transformed & 0x8000_0000_0000_0000 != 0 {
        transformed & 0x7FFF_FFFF_FFFF_FFFF
    } else {
        !transformed
    };
    f64::from_bits(bits)
}

// --- TEXT/BLOB escape-then-terminate (RA.2) ---

/// `0x00` → `0x00 0xFF`; every other byte unchanged; ends with `0x00
/// 0x00`. See RA.2's own worked correctness proof for why this preserves
/// order for variable-length content regardless of composite-key
/// position.
fn encode_escaped_terminated(bytes: &[u8], out: &mut Vec<u8>) {
    for &b in bytes {
        if b == 0x00 {
            out.push(0x00);
            out.push(0xFF);
        } else {
            out.push(b);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

fn decode_escaped_terminated(bytes: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
    let mut content = Vec::new();
    loop {
        let b = *take(bytes, pos, 1)?.first().expect("checked length 1");
        if b == 0x00 {
            let next = *take(bytes, pos, 1)?.first().expect("checked length 1");
            match next {
                0x00 => return Ok(content), // terminator
                0xFF => content.push(0x00), // escaped embedded 0x00
                other => {
                    return Err(RelationalError::InvalidInput {
                        detail: format!("malformed escape sequence: 0x00 0x{other:02X}"),
                    })
                }
            }
        } else {
            content.push(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::cmp::Ordering;

    fn assert_order_preserving<T: PartialOrd + std::fmt::Debug + Clone>(
        a: T,
        b: T,
        to_value: impl Fn(T) -> RelationalValue,
    ) {
        let mut encoded_a = Vec::new();
        encode_key_value(&to_value(a.clone()), &mut encoded_a).unwrap();
        let mut encoded_b = Vec::new();
        encode_key_value(&to_value(b.clone()), &mut encoded_b).unwrap();
        let logical = a.partial_cmp(&b).unwrap();
        let physical = encoded_a.cmp(&encoded_b);
        assert_eq!(
            physical, logical,
            "encode({a:?}) vs encode({b:?}): byte order {physical:?}, logical order {logical:?}"
        );
    }

    // --- INTEGER: negative / zero / positive / boundaries ---

    #[test]
    fn integer_ordering_at_boundaries_and_zero() {
        let cases = [i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX];
        for w in cases.windows(2) {
            assert_order_preserving(w[0], w[1], RelationalValue::Integer);
        }
    }

    proptest! {
        #[test]
        fn integer_ordering_property(a: i32, b: i32) {
            if a != b {
                assert_order_preserving(a, b, RelationalValue::Integer);
            }
        }

        #[test]
        fn bigint_ordering_property(a: i64, b: i64) {
            if a != b {
                assert_order_preserving(a, b, RelationalValue::Bigint);
            }
        }

        #[test]
        fn date_ordering_property(a: i32, b: i32) {
            if a != b {
                assert_order_preserving(a, b, RelationalValue::Date);
            }
        }

        #[test]
        fn timestamp_ordering_property(a: i64, b: i64) {
            if a != b {
                assert_order_preserving(a, b, RelationalValue::Timestamp);
            }
        }

        #[test]
        fn time_ordering_property(a in 0i64..=86_399_999_999i64, b in 0i64..=86_399_999_999i64) {
            if a != b {
                assert_order_preserving(a, b, RelationalValue::Time);
            }
        }

        #[test]
        fn real_ordering_property(a: f32, b: f32) {
            if !a.is_nan() && !b.is_nan() && a != b {
                assert_order_preserving(a, b, RelationalValue::Real);
            }
        }

        #[test]
        fn double_ordering_property(a: f64, b: f64) {
            if !a.is_nan() && !b.is_nan() && a != b {
                assert_order_preserving(a, b, RelationalValue::Double);
            }
        }

        #[test]
        fn text_ordering_property(a in ".*", b in ".*") {
            if a != b {
                assert_order_preserving(a, b, RelationalValue::Text);
            }
        }

        #[test]
        fn blob_ordering_property(a: Vec<u8>, b: Vec<u8>) {
            if a != b {
                let av = a.clone();
                let bv = b.clone();
                let mut ea = Vec::new();
                encode_key_value(&RelationalValue::Blob(av), &mut ea).unwrap();
                let mut eb = Vec::new();
                encode_key_value(&RelationalValue::Blob(bv), &mut eb).unwrap();
                let logical = a.cmp(&b);
                assert_eq!(ea.cmp(&eb), logical);
            }
        }
    }

    #[test]
    fn real_and_double_boundaries_including_zero_and_negative() {
        assert_order_preserving(f32::MIN, -1.0f32, RelationalValue::Real);
        assert_order_preserving(-1.0f32, -0.0f32, RelationalValue::Real);
        assert_order_preserving(0.0f32, 1.0f32, RelationalValue::Real);
        assert_order_preserving(1.0f32, f32::MAX, RelationalValue::Real);
        assert_order_preserving(f64::MIN, -1.0f64, RelationalValue::Double);
        assert_order_preserving(1.0f64, f64::MAX, RelationalValue::Double);
    }

    /// The specific edge case RA.2 calls out by name: `-0.0` and `+0.0`
    /// are the same value under IEEE-754 equality, so their encodings
    /// must be byte-identical, not merely adjacent.
    #[test]
    fn negative_zero_and_positive_zero_encode_identically() {
        let mut neg = Vec::new();
        encode_key_value(&RelationalValue::Real(-0.0), &mut neg).unwrap();
        let mut pos = Vec::new();
        encode_key_value(&RelationalValue::Real(0.0), &mut pos).unwrap();
        assert_eq!(neg, pos, "REAL -0.0 and +0.0 must encode identically");

        let mut neg64 = Vec::new();
        encode_key_value(&RelationalValue::Double(-0.0), &mut neg64).unwrap();
        let mut pos64 = Vec::new();
        encode_key_value(&RelationalValue::Double(0.0), &mut pos64).unwrap();
        assert_eq!(neg64, pos64, "DOUBLE -0.0 and +0.0 must encode identically");
    }

    #[test]
    fn nan_is_rejected_in_key_position() {
        let mut out = Vec::new();
        assert!(encode_key_value(&RelationalValue::Real(f32::NAN), &mut out).is_err());
        assert!(encode_key_value(&RelationalValue::Double(f64::NAN), &mut out).is_err());
    }

    #[test]
    fn negative_time_is_rejected() {
        let mut out = Vec::new();
        let err = encode_key_value(&RelationalValue::Time(-1), &mut out).unwrap_err();
        assert!(matches!(err, RelationalError::InvalidInput { .. }));
    }

    // --- DECIMAL sign handling ---

    #[test]
    fn decimal_ordering_across_sign_boundary() {
        assert_order_preserving(-100i128, -1i128, |v| RelationalValue::Decimal(v, 2));
        assert_order_preserving(-1i128, 0i128, |v| RelationalValue::Decimal(v, 2));
        assert_order_preserving(0i128, 1i128, |v| RelationalValue::Decimal(v, 2));
        assert_order_preserving(1i128, 100i128, |v| RelationalValue::Decimal(v, 2));
        assert_order_preserving(i128::MIN, i128::MIN + 1, |v| RelationalValue::Decimal(v, 2));
        assert_order_preserving(i128::MAX - 1, i128::MAX, |v| RelationalValue::Decimal(v, 2));
    }

    proptest! {
        #[test]
        fn decimal_ordering_property(a: i128, b: i128) {
            if a != b {
                assert_order_preserving(a, b, |v| RelationalValue::Decimal(v, 2));
            }
        }
    }

    // --- TEXT/BLOB length-boundary cases ---

    #[test]
    fn text_prefix_sorts_before_its_extension() {
        assert_order_preserving("ab".to_string(), "abc".to_string(), RelationalValue::Text);
        assert_order_preserving("".to_string(), "a".to_string(), RelationalValue::Text);
    }

    #[test]
    fn text_with_embedded_null_byte_round_trips_and_orders_correctly() {
        // The exact case RA.2's proof turns on: a string that is a
        // prefix of another, where the longer one's next byte is an
        // escaped embedded 0x00.
        let a = "ab".to_string();
        let b = "ab\u{0}c".to_string();
        assert_order_preserving(a, b, RelationalValue::Text);
    }

    #[test]
    fn blob_with_high_bytes_and_embedded_zero_round_trips() {
        let value = RelationalValue::Blob(vec![0xFF, 0x00, 0x01, 0x00, 0xFF]);
        let mut encoded = Vec::new();
        encode_key_value(&value, &mut encoded).unwrap();
        let decoded = decode_composite_key(&[RelationalType::Blob], &encoded).unwrap();
        assert_eq!(decoded, vec![value]);
    }

    // --- Composite key round trip and ordering ---

    #[test]
    fn composite_key_round_trips_mixed_types() {
        let values = vec![
            RelationalValue::Integer(-5),
            RelationalValue::Text("mid".to_string()),
            RelationalValue::Bigint(999),
        ];
        let types = [
            RelationalType::Integer,
            RelationalType::Text,
            RelationalType::Bigint,
        ];
        let encoded = encode_composite_key(&values).unwrap();
        let decoded = decode_composite_key(&types, &encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn composite_key_orders_by_first_column_then_second() {
        let types = [RelationalType::Integer, RelationalType::Text];
        let a = vec![
            RelationalValue::Integer(1),
            RelationalValue::Text("z".to_string()),
        ];
        let b = vec![
            RelationalValue::Integer(2),
            RelationalValue::Text("a".to_string()),
        ];
        let encoded_a = encode_composite_key(&a).unwrap();
        let encoded_b = encode_composite_key(&b).unwrap();
        assert_eq!(
            encoded_a.cmp(&encoded_b),
            Ordering::Less,
            "first column (1 < 2) must dominate ordering regardless of the second column"
        );
        let _ = types; // decoded separately above; kept here for documentation context
    }

    #[test]
    fn composite_key_with_text_not_in_last_position_is_unambiguous() {
        // Without a terminator, (TEXT="a", TEXT="bc") and (TEXT="ab",
        // TEXT="c") could collide. With escape-terminate, they must not.
        let types = [RelationalType::Text, RelationalType::Text];
        let first = vec![
            RelationalValue::Text("a".to_string()),
            RelationalValue::Text("bc".to_string()),
        ];
        let second = vec![
            RelationalValue::Text("ab".to_string()),
            RelationalValue::Text("c".to_string()),
        ];
        let encoded_first = encode_composite_key(&first).unwrap();
        let encoded_second = encode_composite_key(&second).unwrap();
        assert_ne!(encoded_first, encoded_second);
        assert_eq!(decode_composite_key(&types, &encoded_first).unwrap(), first);
        assert_eq!(
            decode_composite_key(&types, &encoded_second).unwrap(),
            second
        );
    }

    #[test]
    fn decode_rejects_malformed_escape_sequence() {
        // 0x00 followed by neither 0x00 nor 0xFF.
        let bytes = vec![b'a', 0x00, 0x42];
        let err = decode_composite_key(&[RelationalType::Text], &bytes).unwrap_err();
        assert!(matches!(err, RelationalError::InvalidInput { .. }));
    }

    #[test]
    fn decode_rejects_missing_terminator_without_panicking() {
        let bytes = vec![b'a', b'b'];
        let err = decode_composite_key(&[RelationalType::Text], &bytes).unwrap_err();
        assert!(matches!(err, RelationalError::InvalidInput { .. }));
    }
}
