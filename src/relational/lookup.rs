//! Finding the rows of a table that hold given values in given columns, by the
//! cheapest access path the table offers -- the one primitive foreign-key
//! checking (`txn::fk`) and `ALTER TABLE` validation (`alter`) are built on
//! (`docs/adr/ADR-DDL-FK-01.md`). It reads committed state at a caller-chosen
//! sequence number; a transaction layers its own uncommitted writes on top.
//!
//! The access path is, in order of preference:
//!
//! 1. **Primary-key point read** -- the columns are exactly the primary key.
//! 2. **Primary-key prefix scan** -- the columns are exactly the leading columns
//!    of the primary key (a child table keyed `(parent_id, seq)` finds its rows of
//!    one parent in one narrow range).
//! 3. **Ready index scan** -- a `Ready` secondary index covers exactly these
//!    columns.
//! 4. **Table scan** -- nothing else is available. Correct, linear in the table.
//!
//! A column set is matched as a *set*; values are reordered to the key's order.

use std::collections::HashSet;
use std::ops::Bound;

use crate::catalog::schema::{ColumnRow, IndexRow, IndexState, TableRow};
use crate::relational::error::{RelationalError, Result};
use crate::relational::index_key::{encode_indexed_columns, index_entry_prefix_range};
use crate::relational::key::{decode_composite_key, encode_composite_key};
use crate::relational::table_store::{relational_type_from_column, Row, TableStore};
use crate::relational::value::{RelationalType, RelationalValue};

/// How rows are found by a column set. See the module documentation.
#[derive(Debug, Clone)]
pub enum Access {
    PkPoint,
    PkPrefix { len: usize },
    Index(IndexRow),
    Scan,
}

impl Access {
    /// Does this path avoid reading the whole table?
    pub fn is_narrow(&self) -> bool {
        !matches!(self, Access::Scan)
    }
}

fn set_of(ordinals: &[u16]) -> HashSet<u16> {
    ordinals.iter().copied().collect()
}

impl TableStore {
    /// The access path for rows matched on `ordinals`.
    pub(crate) fn plan_access(&self, table: &TableRow, ordinals: &[u16]) -> Result<Access> {
        let wanted = set_of(ordinals);
        let pk = &table.pk_ordinals;
        if wanted.len() == pk.len() && wanted == set_of(pk) {
            return Ok(Access::PkPoint);
        }
        if !ordinals.is_empty()
            && ordinals.len() < pk.len()
            && wanted == set_of(&pk[..ordinals.len()])
        {
            return Ok(Access::PkPrefix {
                len: ordinals.len(),
            });
        }
        for index in self.catalog().list_indexes(table.table_id)? {
            if index.state == IndexState::Ready
                && index.column_ordinals.len() == ordinals.len()
                && set_of(&index.column_ordinals) == wanted
            {
                return Ok(Access::Index(index));
            }
        }
        Ok(Access::Scan)
    }

    /// Up to `limit` rows of `table_id` (committed state as of `as_of`) whose
    /// `ordinals` columns equal `values` (pairwise). `values` must not contain
    /// `NULL` (a `NULL` equals nothing).
    pub(crate) fn find_rows_with_access(
        &self,
        table_id: u32,
        ordinals: &[u16],
        values: &[RelationalValue],
        access: &Access,
        as_of: u64,
        limit: usize,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        if ordinals.len() != values.len() {
            return Err(RelationalError::InvalidInput {
                detail: "column and value counts differ".to_string(),
            });
        }
        let (table, columns) = self.resolve_table(table_id)?;
        // The value held for `ordinal`, wherever it sits in `ordinals`.
        let value_for = |ordinal: u16| -> Result<&RelationalValue> {
            ordinals
                .iter()
                .position(|&o| o == ordinal)
                .map(|i| &values[i])
                .ok_or_else(|| RelationalError::InvalidInput {
                    detail: "access path does not match the column set".to_string(),
                })
        };
        let mut out: Vec<(Vec<RelationalValue>, Row)> = Vec::new();
        match access {
            Access::PkPoint => {
                let pk_values = table
                    .pk_ordinals
                    .iter()
                    .map(|&o| value_for(o).cloned())
                    .collect::<Result<Vec<_>>>()?;
                let encoded = encode_composite_key(&pk_values)?;
                if let Some(row) =
                    self.fetch_row_by_encoded_pk(&table, &columns, &pk_values, &encoded, as_of)?
                {
                    out.push((pk_values, row));
                }
            }
            Access::PkPrefix { len } => {
                let prefix = table.pk_ordinals[..*len]
                    .iter()
                    .map(|&o| value_for(o).cloned())
                    .collect::<Result<Vec<_>>>()?;
                for entry in self.scan_table_pk_range_rows_as_of(
                    table_id,
                    Bound::Included(prefix.clone()),
                    Bound::Included(prefix),
                    as_of,
                )? {
                    out.push(entry?);
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            Access::Index(index) => {
                let ordered = index
                    .column_ordinals
                    .iter()
                    .map(|&o| value_for(o).cloned().map(Some))
                    .collect::<Result<Vec<_>>>()?;
                let prefix = encode_indexed_columns(&ordered)?;
                let (start, end) = index_entry_prefix_range(table_id, index.index_id, &prefix);
                let pk_types = pk_types(&table, &columns)?;
                let header_and_prefix = 9 + prefix.len();
                for entry in self
                    .engine()
                    .range_scan(bound_ref(&start), bound_ref(&end), as_of)
                {
                    let (key, _) = entry?;
                    let pk_bytes = key.get(header_and_prefix..).unwrap_or(&[]);
                    let pk_values = decode_composite_key(&pk_types, pk_bytes)?;
                    if let Some(row) =
                        self.fetch_row_by_encoded_pk(&table, &columns, &pk_values, pk_bytes, as_of)?
                    {
                        out.push((pk_values, row));
                        if out.len() >= limit {
                            break;
                        }
                    }
                }
            }
            Access::Scan => {
                for entry in self.scan_table_rows_as_of(table_id, as_of)? {
                    let (pk, row) = entry?;
                    if row_matches(&row, ordinals, values) {
                        out.push((pk, row));
                        if out.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// Calls `visit(pk, row)` for each row of `table_id` (as of `as_of`) until it
    /// returns `Ok(false)` (stop) or an `Err`. Lazy: one row in memory at a time.
    pub(crate) fn for_each_row_as_of(
        &self,
        table_id: u32,
        as_of: u64,
        mut visit: impl FnMut(&[RelationalValue], &Row) -> Result<bool>,
    ) -> Result<()> {
        for entry in self.scan_table_rows_as_of(table_id, as_of)? {
            let (pk, row) = entry?;
            if !visit(&pk, &row)? {
                break;
            }
        }
        Ok(())
    }

    /// Is there at least one row in the table (committed state, `u64::MAX`)?
    pub(crate) fn table_has_rows(&self, table_id: u32) -> Result<bool> {
        let (start, end) = crate::relational::key::table_row_range(table_id);
        Ok(self
            .engine()
            .range_scan(bound_ref(&start), bound_ref(&end), u64::MAX)
            .next()
            .transpose()?
            .is_some())
    }
}

/// Does `row` hold `values` in `ordinals`?
pub(crate) fn row_matches(row: &Row, ordinals: &[u16], values: &[RelationalValue]) -> bool {
    ordinals
        .iter()
        .zip(values)
        .all(|(&o, v)| row.get(o as usize).and_then(|x| x.as_ref()) == Some(v))
}

fn pk_types(table: &TableRow, columns: &[ColumnRow]) -> Result<Vec<RelationalType>> {
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

fn bound_ref(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}
