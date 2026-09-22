//! Secondary-index physical key encoding — `PHASE_RELATIONAL_INDEX_
//! BACKFILL_ADR.md` §2/§3. Verified (not merely asserted) against
//! `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §5's already-committed
//! layout: `0x01 || table_id:u32 BE || index_id:u32 BE (>0) ||
//! encoded_indexed_columns || encoded_pk_columns`, with `index_id = 0`
//! reserved for the table's own row key (`relational::key::table_row_
//! key`) — this module never constructs `index_id = 0`.
//!
//! Indexed columns (unlike primary-key columns, `relational::key`) may be
//! `NULL` — Architecture doc §5: "encoded as a reserved lowest-sorting
//! marker byte, consistently, so `NULL`s in a secondary index sort
//! together at one end." Implemented here as a 1-byte presence tag
//! prefixing every indexed column's own encoding: `0x00` = NULL (sorts
//! before every real value), `0x01 || <relational::key::encode_key_
//! value>` = present. This costs one byte per indexed column and keeps
//! every indexed-key field self-delimiting in the same way `relational::
//! key`'s TEXT/BLOB escape-terminate scheme already is, so a composite
//! index's multi-column key remains unambiguous and order-preserving
//! column-by-column (NULL < any real value in the first indexed column
//! dominates ordering regardless of later columns, matching ordinary SQL
//! `ORDER BY` `NULLS FIRST` semantics for ascending order at the storage
//! level — a query layer wanting `NULLS LAST` reorders at read time, not
//! by inventing a second key encoding).
//!
//! An index entry's PRIMARY KEY suffix reuses `relational::key::encode_
//! composite_key` verbatim (D6: PK columns are never `NULL`, so no
//! presence tag is needed there — this module adds the tag only for the
//! indexed-column prefix).

use std::ops::Bound;

use crate::relational::error::{RelationalError, Result};
use crate::relational::key::{encode_key_value, RELATIONAL_NAMESPACE};
use crate::relational::value::{RelationalType, RelationalValue};

/// Presence tags for one indexed column's encoding (see module doc).
const INDEXED_NULL_TAG: u8 = 0x00;
const INDEXED_PRESENT_TAG: u8 = 0x01;

/// `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §14: bounds an index entry's
/// physical key size before it is ever constructed from caller/backfill-
/// derived data — defense in depth alongside `TableStore::MAX_ROW_VALUE_
/// BYTES` (item 22/36: "maximum key size" is an explicit required
/// resource limit).
pub const MAX_INDEX_KEY_BYTES: usize = 8 * 1024;

/// One index entry's physical key: `0x01 || table_id:u32 BE ||
/// index_id:u32 BE || encoded_indexed_columns || encoded_pk`.
/// `index_id` must be `> 0` (`0` is the table's own row key, D2) —
/// callers derive it from a `Catalog`-resolved `IndexRow`, never from
/// caller-supplied input (D26/§17: physical IDs are never client-
/// chosen).
pub fn index_entry_key(
    table_id: u32,
    index_id: u32,
    encoded_indexed_columns: &[u8],
    encoded_pk: &[u8],
) -> Result<Vec<u8>> {
    debug_assert!(index_id > 0, "index_id 0 is reserved for table rows");
    let mut key = Vec::with_capacity(1 + 4 + 4 + encoded_indexed_columns.len() + encoded_pk.len());
    key.push(RELATIONAL_NAMESPACE);
    key.extend_from_slice(&table_id.to_be_bytes());
    key.extend_from_slice(&index_id.to_be_bytes());
    key.extend_from_slice(encoded_indexed_columns);
    key.extend_from_slice(encoded_pk);
    if key.len() > MAX_INDEX_KEY_BYTES {
        return Err(RelationalError::InvalidInput {
            detail: format!(
                "index entry key exceeds max size ({} > {MAX_INDEX_KEY_BYTES} bytes)",
                key.len()
            ),
        });
    }
    Ok(key)
}

/// The `[start, end)` range covering every entry of one index (D2's
/// namespace/`table_id`/`index_id` prefix) — the whole-index range scan
/// primitive (backfill enumeration, `DROP INDEX`'s physical sweep, an
/// unbounded `IndexScan`).
pub fn index_entry_range(table_id: u32, index_id: u32) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let start = index_prefix(table_id, index_id);
    let end = match index_id.checked_add(1) {
        Some(next) => index_prefix(table_id, next),
        None => {
            // index_id == u32::MAX: fall back to the next table_id's start
            // (mirrors `catalog::encoding::system_table_prefix_range`'s own
            // documented "all-0xFF" fallback) unless table_id is itself
            // u32::MAX, in which case there genuinely is no finite upper
            // bound and Unbounded is correct.
            return match table_id.checked_add(1) {
                Some(next_table) => (
                    Bound::Included(start),
                    Bound::Excluded(index_prefix(next_table, 1)),
                ),
                None => (Bound::Included(start), Bound::Unbounded),
            };
        }
    };
    (Bound::Included(start), Bound::Excluded(end))
}

fn index_prefix(table_id: u32, index_id: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(9);
    key.push(RELATIONAL_NAMESPACE);
    key.extend_from_slice(&table_id.to_be_bytes());
    key.extend_from_slice(&index_id.to_be_bytes());
    key
}

/// The `[start, end)` range of one index's entries whose indexed-column
/// prefix bytes equal `encoded_prefix` exactly (an equality/composite-
/// prefix lookup, item 16/20) — every entry sharing this prefix, followed
/// by any primary-key suffix. `encoded_prefix` must itself be a complete,
/// unambiguous encoding of one or more leading indexed columns (produced
/// by `encode_indexed_columns`, called with just the leading column
/// values a lookup has), never a truncated partial column.
pub fn index_entry_prefix_range(
    table_id: u32,
    index_id: u32,
    encoded_prefix: &[u8],
) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let mut start = index_prefix(table_id, index_id);
    start.extend_from_slice(encoded_prefix);
    let mut incremented = encoded_prefix.to_vec();
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
        let mut end = index_prefix(table_id, index_id);
        end.extend_from_slice(&incremented);
        (Bound::Included(start), Bound::Excluded(end))
    } else {
        // Every byte was 0xFF (or the prefix was empty) — fall back to
        // this index's own whole-range upper bound, mirroring
        // `catalog::encoding::system_table_prefix_range`'s established
        // precedent for the identical edge case.
        let (_, upper) = index_entry_range(table_id, index_id);
        (Bound::Included(start), upper)
    }
}

/// Builds the physical `[start, end)` bounds for an `IndexScan` over one
/// index's entries, given caller-supplied bounds expressed purely in
/// terms of *encoded indexed-column bytes* (`encode_indexed_columns`,
/// possibly a prefix). `Bound::Unbounded` on either side defaults to that
/// index's own whole-range boundary (never spills into a neighboring
/// index or table, D2) — item 17's "inclusive / exclusive / prefix /
/// composite" semantics fall out directly from which `Bound` variant the
/// caller passes, no separate case analysis needed here.
pub fn index_scan_range(
    table_id: u32,
    index_id: u32,
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let (default_start, default_end) = index_entry_range(table_id, index_id);
    let physical_start = match start {
        Bound::Unbounded => default_start,
        Bound::Included(bytes) => {
            let mut k = index_prefix(table_id, index_id);
            k.extend_from_slice(&bytes);
            Bound::Included(k)
        }
        Bound::Excluded(bytes) => {
            // Exclusive lower bound on the indexed-column prefix: every
            // entry sharing that exact prefix (any PK suffix) must be
            // excluded, so the physical bound is the prefix's own
            // successor (mirrors `index_entry_prefix_range`'s carry
            // logic), not the prefix bytes themselves (which would
            // wrongly re-include same-prefix entries via their PK
            // suffix).
            let (_, excl_end) = index_entry_prefix_range(table_id, index_id, &bytes);
            match excl_end {
                Bound::Excluded(k) => Bound::Included(k),
                other => other,
            }
        }
    };
    let physical_end = match end {
        Bound::Unbounded => default_end,
        Bound::Excluded(bytes) => {
            let mut k = index_prefix(table_id, index_id);
            k.extend_from_slice(&bytes);
            Bound::Excluded(k)
        }
        Bound::Included(bytes) => {
            let (_, excl_end) = index_entry_prefix_range(table_id, index_id, &bytes);
            excl_end
        }
    };
    (physical_start, physical_end)
}

/// Encodes one indexed column value (`None` = SQL `NULL`) with its
/// presence tag — see module doc for the ordering proof.
pub fn encode_indexed_column(value: Option<&RelationalValue>, out: &mut Vec<u8>) -> Result<()> {
    match value {
        None => out.push(INDEXED_NULL_TAG),
        Some(v) => {
            out.push(INDEXED_PRESENT_TAG);
            encode_key_value(v, out)?;
        }
    }
    Ok(())
}

/// The complete, order-preserving encoding of a tuple of indexed-column
/// values, in the index's declared column order — used both to build a
/// concrete entry's key (backfill, maintenance, passing every indexed
/// column) and to build an equality/prefix-lookup bound (passing only the
/// leading columns a caller has concrete values for; `index_entry_prefix_
/// range` treats the result as a prefix).
pub fn encode_indexed_columns(values: &[Option<RelationalValue>]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for v in values {
        encode_indexed_column(v.as_ref(), &mut out)?;
    }
    Ok(out)
}

/// Decodes an index entry's indexed-column prefix, given the declared
/// types in index-column order (mirrors `relational::key::decode_
/// composite_key`, NULL-aware). Returns the number of bytes consumed so
/// the caller can decode the trailing primary-key suffix from the same
/// buffer.
pub fn decode_indexed_columns(
    types: &[RelationalType],
    bytes: &[u8],
) -> Result<(Vec<Option<RelationalValue>>, usize)> {
    let mut pos = 0usize;
    let mut out = Vec::with_capacity(types.len());
    for &t in types {
        let tag = *bytes
            .get(pos)
            .ok_or_else(|| RelationalError::InvalidInput {
                detail: "index entry key truncated before indexed-column presence tag".to_string(),
            })?;
        pos += 1;
        match tag {
            INDEXED_NULL_TAG => out.push(None),
            INDEXED_PRESENT_TAG => {
                let value = crate::relational::key::decode_key_value(t, bytes, &mut pos)?;
                out.push(Some(value));
            }
            other => {
                return Err(RelationalError::InvalidInput {
                    detail: format!("invalid indexed-column presence tag 0x{other:02X}"),
                })
            }
        }
    }
    Ok((out, pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relational::key::encode_composite_key;
    use proptest::prelude::*;

    #[test]
    fn index_entry_key_layout() {
        let indexed = encode_indexed_columns(&[Some(RelationalValue::Integer(5))]).unwrap();
        let pk = encode_composite_key(&[RelationalValue::Integer(1)]).unwrap();
        let key = index_entry_key(3, 7, &indexed, &pk).unwrap();
        assert_eq!(key[0], RELATIONAL_NAMESPACE);
        assert_eq!(&key[1..5], &3u32.to_be_bytes());
        assert_eq!(&key[5..9], &7u32.to_be_bytes());
    }

    #[test]
    fn null_sorts_before_any_real_value() {
        let null_key = encode_indexed_columns(&[None]).unwrap();
        let present_min =
            encode_indexed_columns(&[Some(RelationalValue::Integer(i32::MIN))]).unwrap();
        assert!(null_key < present_min, "NULL must sort before i32::MIN");
    }

    #[test]
    fn round_trips_null_and_present_composite() {
        let types = [RelationalType::Integer, RelationalType::Text];
        let values = vec![None, Some(RelationalValue::Text("hi".to_string()))];
        let encoded = encode_indexed_columns(&values).unwrap();
        let (decoded, consumed) = decode_indexed_columns(&types, &encoded).unwrap();
        assert_eq!(decoded, values);
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn decode_rejects_invalid_presence_tag() {
        let err =
            decode_indexed_columns(&[RelationalType::Integer], &[0x02, 0, 0, 0, 0]).unwrap_err();
        assert!(matches!(err, RelationalError::InvalidInput { .. }));
    }

    #[test]
    fn decode_rejects_truncated_buffer_without_panicking() {
        let err = decode_indexed_columns(&[RelationalType::Integer], &[]).unwrap_err();
        assert!(matches!(err, RelationalError::InvalidInput { .. }));
        let err2 = decode_indexed_columns(&[RelationalType::Integer], &[INDEXED_PRESENT_TAG, 0, 0])
            .unwrap_err();
        assert!(matches!(err2, RelationalError::InvalidInput { .. }));
    }

    #[test]
    fn oversized_key_is_rejected() {
        let huge_pk = vec![0u8; MAX_INDEX_KEY_BYTES + 1];
        let err = index_entry_key(1, 1, &[], &huge_pk).unwrap_err();
        assert!(matches!(err, RelationalError::InvalidInput { .. }));
    }

    #[test]
    fn prefix_range_isolates_matching_entries() {
        let table_id = 5;
        let index_id = 2;
        let prefix_a = encode_indexed_columns(&[Some(RelationalValue::Integer(10))]).unwrap();
        let prefix_b = encode_indexed_columns(&[Some(RelationalValue::Integer(11))]).unwrap();
        let (start, end) = index_entry_prefix_range(table_id, index_id, &prefix_a);
        let pk1 = encode_composite_key(&[RelationalValue::Integer(1)]).unwrap();
        let pk2 = encode_composite_key(&[RelationalValue::Integer(999)]).unwrap();
        let inside_low = index_entry_key(table_id, index_id, &prefix_a, &pk1).unwrap();
        let inside_high = index_entry_key(table_id, index_id, &prefix_a, &pk2).unwrap();
        let outside = index_entry_key(table_id, index_id, &prefix_b, &pk1).unwrap();
        let start = match start {
            Bound::Included(s) => s,
            other => panic!("expected Included, got {other:?}"),
        };
        let end = match end {
            Bound::Excluded(e) => e,
            other => panic!("expected Excluded, got {other:?}"),
        };
        assert!(start <= inside_low && inside_low < end);
        assert!(start <= inside_high && inside_high < end);
        assert!(
            outside >= end,
            "a different indexed value must fall outside the prefix range"
        );
    }

    proptest! {
        #[test]
        fn indexed_column_ordering_property(a: i32, b: i32) {
            if a != b {
                let ea = encode_indexed_columns(&[Some(RelationalValue::Integer(a))]).unwrap();
                let eb = encode_indexed_columns(&[Some(RelationalValue::Integer(b))]).unwrap();
                assert_eq!(ea.cmp(&eb), a.cmp(&b));
            }
        }

        #[test]
        fn composite_indexed_columns_round_trip(a: i32, b in ".*") {
            let types = [RelationalType::Integer, RelationalType::Text];
            let values = vec![Some(RelationalValue::Integer(a)), Some(RelationalValue::Text(b))];
            let encoded = encode_indexed_columns(&values).unwrap();
            let (decoded, consumed) = decode_indexed_columns(&types, &encoded).unwrap();
            assert_eq!(decoded, values);
            assert_eq!(consumed, encoded.len());
        }
    }
}
