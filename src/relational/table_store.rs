//! `TableStore` — row-level put/get/delete/scan primitives, resolving a
//! table's shape from the certified `CatalogService` (never a second,
//! duplicated metadata structure — the catalog remains the source of
//! truth) and writing through the certified `LsmEngine`/`write_batch`
//! exclusively. `RELATIONAL ADR AMENDMENT 003` RA.3/RA.5.

use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::sync::{Arc, Mutex, RwLock};

use crate::catalog::schema::{ColumnRow, IndexKind, IndexRow, IndexState, TableRow};
use crate::catalog::CatalogService;
use crate::lsm::{LsmEngine, WriteOp};
use crate::relational::error::{RelationalError, Result};
use crate::relational::index_key::{encode_indexed_columns, index_entry_key};
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
    /// `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §6/§7's per-table "index
    /// epoch lock" — see `epoch_lock`'s own doc comment for the full
    /// correctness argument it exists to support. Lazily populated (one
    /// entry per table ever written to or indexed, for this process's
    /// lifetime — bounded by the table count, never by write volume).
    epoch_locks: Mutex<HashMap<u32, Arc<RwLock<()>>>>,
}

impl TableStore {
    pub fn new(engine: Arc<LsmEngine>, catalog: Arc<CatalogService>) -> Self {
        TableStore {
            engine,
            catalog,
            epoch_locks: Mutex::new(HashMap::new()),
        }
    }

    /// The affected table's index epoch lock, creating it on first use.
    /// **Readers** (`put_row`/`put_rows`/`delete_row`, one per ordinary
    /// row write) hold the *read* side for exactly the critical section
    /// that resolves the currently-maintained index set and issues the
    /// row's `write_batch` — many such writers run fully concurrently
    /// (`RwLock` read/read is non-exclusive), so ordinary write
    /// throughput is unaffected by an index build in progress.
    /// **`IndexBuilder`** holds the *write* side only for two narrow
    /// purposes: (a) the instant a `CREATE INDEX`/`DROP INDEX` catalog
    /// transition that changes the maintained-index set commits, so no
    /// writer's critical section can straddle that boundary and use a
    /// stale index list (`PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §6's
    /// proof); and (b) each backfill chunk's final re-validate-then-
    /// commit step, so a concurrent delete/insert of a row already in
    /// that chunk can never race a stale backfilled entry (§7's proof).
    /// Never held for an entire backfill or sweep's duration — only per
    /// operation or per (bounded-size) chunk, so "the table remains
    /// writable during backfill" (item 6/51 of the governing directive)
    /// holds by construction, not merely by intention.
    pub(crate) fn epoch_lock(&self, table_id: u32) -> Arc<RwLock<()>> {
        let mut locks = self.epoch_locks.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(
            locks
                .entry(table_id)
                .or_insert_with(|| Arc::new(RwLock::new(()))),
        )
    }

    /// The indexes `put_row`/`put_rows`/`delete_row`/backfill must keep in
    /// lockstep with the table: `Building` (already receiving live writes,
    /// ADR §4 — a build in progress must never miss a concurrent write) and
    /// `Ready` (ordinary steady-state DML). Never `Failed`/`Dropping` (§9:
    /// a dropping index must stop receiving new entries the instant its
    /// catalog transition commits) and never `Primary` (D6: the table's own
    /// row key already **is** the primary-key structure; there is no
    /// separate physical entry to maintain for it).
    pub(crate) fn maintained_indexes(&self, table_id: u32) -> Result<Vec<IndexRow>> {
        Ok(self
            .catalog
            .list_indexes(table_id)?
            .into_iter()
            .filter(|i| {
                i.kind != IndexKind::Primary
                    && matches!(i.state, IndexState::Building | IndexState::Ready)
            })
            .collect())
    }

    pub(crate) fn resolve_table(&self, table_id: u32) -> Result<(TableRow, Vec<ColumnRow>)> {
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
    /// exactly once, even at N=1 (RA.5's own reasoning), now including
    /// every affected `Building`/`Ready` secondary index's entry writes
    /// in that same call (D11, `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md`
    /// §5/§6). Returns the durable sequence the row (and its index
    /// entries) committed at.
    pub fn put_row(&self, table_id: u32, values: &[Option<RelationalValue>]) -> Result<u64> {
        let (table, columns) = self.resolve_table(table_id)?;
        validate_row_shape(&table, &columns, values)?;
        let epoch = self.epoch_lock(table_id);
        let _epoch_guard = epoch.read().unwrap_or_else(|p| p.into_inner());

        let indexes = self.maintained_indexes(table_id)?;
        let pk_values = extract_pk_values(&table, values);
        let encoded_pk = encode_composite_key(&pk_values)?;
        let new_row: Row = values.to_vec();

        let mut ops = Vec::with_capacity(1 + indexes.len() * 2);
        if !indexes.is_empty() {
            let old_row =
                self.fetch_row_by_encoded_pk(&table, &columns, &pk_values, &encoded_pk)?;
            ops.extend(index_maintenance_ops(
                table_id,
                &indexes,
                old_row.as_ref(),
                Some(&new_row),
                &encoded_pk,
            )?);
        }
        ops.push(build_put_op(&table, &encoded_pk, &columns, values)?);
        Ok(self.engine.write_batch(&ops)?)
    }

    /// A genuinely multi-row atomic write through one `write_batch`
    /// call — RA.5's own explicit reason for existing: a direct,
    /// SQL-free way to exercise `write_batch`'s multi-row visibility
    /// guarantee through row storage specifically. Every row's index
    /// maintenance (same rule as `put_row`) is folded into the identical
    /// single `write_batch` call.
    pub fn put_rows(&self, table_id: u32, rows: &[Vec<Option<RelationalValue>>]) -> Result<u64> {
        if rows.is_empty() {
            return Err(RelationalError::InvalidInput {
                detail: "put_rows requires at least one row".to_string(),
            });
        }
        let (table, columns) = self.resolve_table(table_id)?;
        for values in rows {
            validate_row_shape(&table, &columns, values)?;
        }
        let epoch = self.epoch_lock(table_id);
        let _epoch_guard = epoch.read().unwrap_or_else(|p| p.into_inner());
        let indexes = self.maintained_indexes(table_id)?;

        let mut ops = Vec::with_capacity(rows.len() * (1 + indexes.len() * 2));
        for values in rows {
            let pk_values = extract_pk_values(&table, values);
            let encoded_pk = encode_composite_key(&pk_values)?;
            if !indexes.is_empty() {
                let old_row =
                    self.fetch_row_by_encoded_pk(&table, &columns, &pk_values, &encoded_pk)?;
                let new_row: Row = values.clone();
                ops.extend(index_maintenance_ops(
                    table_id,
                    &indexes,
                    old_row.as_ref(),
                    Some(&new_row),
                    &encoded_pk,
                )?);
            }
            ops.push(build_put_op(&table, &encoded_pk, &columns, values)?);
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
        self.fetch_row_by_encoded_pk(&table, &columns, pk_values, &encoded_pk)
    }

    fn fetch_row_by_encoded_pk(
        &self,
        table: &TableRow,
        columns: &[ColumnRow],
        pk_values: &[RelationalValue],
        encoded_pk: &[u8],
    ) -> Result<Option<Row>> {
        let key = table_row_key(table.table_id, encoded_pk);
        match self.engine.get(&key)? {
            None => Ok(None),
            Some(value_bytes) => Ok(Some(decode_full_row(
                table,
                columns,
                pk_values,
                &value_bytes,
            )?)),
        }
    }

    /// `write_batch` even for one physical key removed — RA.5's own
    /// reasoning (an operation that already has index maintenance added
    /// to it later never needs its call shape to change). Every affected
    /// `Building`/`Ready` index's entry removal is included in the same
    /// call (D11/§5/§6) — requires reading the row's current values
    /// first (to know which indexed values to remove), which only
    /// happens when the table actually has a maintained index (no read
    /// cost added to a plain, unindexed `DELETE`).
    pub fn delete_row(&self, table_id: u32, pk_values: &[RelationalValue]) -> Result<u64> {
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
        let epoch = self.epoch_lock(table_id);
        let _epoch_guard = epoch.read().unwrap_or_else(|p| p.into_inner());

        let indexes = self.maintained_indexes(table_id)?;
        let encoded_pk = encode_composite_key(pk_values)?;
        let key = table_row_key(table_id, &encoded_pk);

        let mut ops = Vec::with_capacity(1 + indexes.len());
        if !indexes.is_empty() {
            let old_row = self.fetch_row_by_encoded_pk(&table, &columns, pk_values, &encoded_pk)?;
            ops.extend(index_maintenance_ops(
                table_id,
                &indexes,
                old_row.as_ref(),
                None,
                &encoded_pk,
            )?);
        }
        ops.push(WriteOp::Delete { key });
        Ok(self.engine.write_batch(&ops)?)
    }

    /// A table-scoped range scan (D2's physical namespace/`table_id`
    /// prefix) — never returns another table's rows (verified as actual
    /// physical range boundaries by this module's tests, not only
    /// post-filtering). Returns `(primary_key_values, full_row)` pairs
    /// in primary-key order.
    pub fn scan_table(&self, table_id: u32) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        let (table, columns) = self.resolve_table(table_id)?;
        let (start, end) = table_row_range(table_id);
        let mut out = Vec::new();
        for row in self
            .engine
            .range_scan(as_bound_ref(&start), as_bound_ref(&end), u64::MAX)
        {
            let (key, value) = row?;
            out.push(decode_table_row_entry(&table, &columns, &key, &value)?);
        }
        Ok(out)
    }
}

/// Decodes one raw `(key, value)` pair from a table's physical row range
/// (as `range_scan(table_row_range(table_id), ...)` yields it) into
/// `(primary_key_values, full_row)` — shared by `scan_table` and
/// `IndexBuilder`'s backfill enumeration (`src/relational/index.rs`) so
/// both walk the identical decode path, never two independently-
/// maintained copies of it.
pub(crate) fn decode_table_row_entry(
    table: &TableRow,
    columns: &[ColumnRow],
    key: &[u8],
    value: &[u8],
) -> Result<(Vec<RelationalValue>, Row)> {
    let pk_types = pk_column_types(table, columns)?;
    let pk_bytes = key.get(9..).ok_or_else(|| RelationalError::InvalidInput {
        detail: "table row key shorter than the fixed 9-byte header".to_string(),
    })?;
    let pk_values = decode_composite_key(&pk_types, pk_bytes)?;
    let full_row = decode_full_row(table, columns, &pk_values, value)?;
    Ok((pk_values, full_row))
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

/// `pub(crate)`: reused by `relational::index` to resolve an index's
/// indexed-column and primary-key types for entry encoding/decoding and
/// backfill (same source of truth `TableStore` itself uses — never a
/// second, independently-maintained type-resolution path).
pub(crate) fn relational_type_from_column(column: &ColumnRow) -> Result<RelationalType> {
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

/// `pub(crate)`: reused by `relational::txn` (`Transaction::put_row`),
/// so a transactional write is validated by the exact same rule an
/// autocommit `put_row` already is — one implementation, never two.
pub(crate) fn validate_row_shape(
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

/// Extracts a row's primary-key values, in `table.pk_ordinals` order,
/// from a full column-ordinal-ordered `values` slice. Every PK ordinal is
/// guaranteed non-`NULL` here because `validate_row_shape` (called by
/// every public entry point before this) already rejected a `NULL`
/// primary-key column.
pub(crate) fn extract_pk_values(
    table: &TableRow,
    values: &[Option<RelationalValue>],
) -> Vec<RelationalValue> {
    table
        .pk_ordinals
        .iter()
        .map(|&ord| {
            values[ord as usize]
                .clone()
                .expect("validated: primary-key columns are never NULL")
        })
        .collect()
}

/// One index entry's physical key for `row`'s currently-visible values
/// under `index`'s declared indexed columns (`PHASE_RELATIONAL_INDEX_
/// BACKFILL_ADR.md` §2/§3). `pk_encoded` is passed in rather than
/// recomputed (the caller already has it, and it is identical for every
/// index touched by one row write). `pub(crate)`: reused by
/// `IndexBuilder`'s backfill (`src/relational/index.rs`), which computes
/// the identical key from a freshly re-validated row.
pub(crate) fn indexed_entry_key(
    table_id: u32,
    index: &IndexRow,
    row: &Row,
    pk_encoded: &[u8],
) -> Result<Vec<u8>> {
    let mut values = Vec::with_capacity(index.column_ordinals.len());
    for &ordinal in &index.column_ordinals {
        let value = row
            .get(ordinal as usize)
            .ok_or_else(|| RelationalError::InvalidInput {
                detail: format!(
                    "index {} references column ordinal {ordinal} outside the row",
                    index.index_id
                ),
            })?;
        values.push(value.clone());
    }
    let indexed_bytes = encode_indexed_columns(&values)?;
    index_entry_key(table_id, index.index_id, &indexed_bytes, pk_encoded)
}

/// The complete set of index-entry `WriteOp`s required to move one
/// logical row from `old` to `new` across every index in `indexes` — D11/
/// `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §5/§6's "table row + every
/// affected index entry, one atomic unit" rule, expressed as the ops a
/// caller folds into its own single `write_batch` call (never issued as
/// an independent write here). `old = None` is an insert (no prior
/// entries to remove); `new = None` is a delete (no new entries to add);
/// both `Some` is a future `UPDATE`'s shape — already correct today even
/// though no SQL `UPDATE` exists yet (item 15 of the governing
/// directive), because this function only ever looks at column values,
/// never at *how* a row changed. Per index: the old entry is deleted
/// unless it is byte-identical to the new entry (the common "value
/// unchanged" case), in which case emitting a redundant delete-then-put
/// pair is simply skipped — `write_batch`'s own last-op-wins overwrite
/// would make it a no-op anyway, so skipping it is a pure write-
/// amplification optimization, not a behavior change.
/// `pub(crate)`: reused by `relational::txn::Transaction::commit`, which
/// computes the identical table-row + index-delta `WriteOp` set for a
/// transaction's buffered writes at commit time — one implementation,
/// never a second, independently-maintained copy of D11's atomicity
/// rule.
pub(crate) fn index_maintenance_ops(
    table_id: u32,
    indexes: &[IndexRow],
    old: Option<&Row>,
    new: Option<&Row>,
    pk_encoded: &[u8],
) -> Result<Vec<WriteOp>> {
    let mut ops = Vec::with_capacity(indexes.len() * 2);
    for index in indexes {
        let old_key = old
            .map(|row| indexed_entry_key(table_id, index, row, pk_encoded))
            .transpose()?;
        let new_key = new
            .map(|row| indexed_entry_key(table_id, index, row, pk_encoded))
            .transpose()?;
        if let Some(old_key) = &old_key {
            if new_key.as_ref() != Some(old_key) {
                ops.push(WriteOp::Delete {
                    key: old_key.clone(),
                });
            }
        }
        if let Some(new_key) = new_key {
            ops.push(WriteOp::Put {
                key: new_key,
                // Every piece of information an index entry carries lives
                // in its key (indexed columns + primary key, D2/D5's
                // Architecture doc §5) — the value is an empty marker, not
                // a covering payload (`PHASE_RELATIONAL_DATABASE_ADR.md`
                // D7: "Do not duplicate full row data in the index without
                // evidence the current ADR requires covering indexes" —
                // no such evidence exists).
                value: Vec::new(),
            });
        }
    }
    Ok(ops)
}

/// `pub(crate)`: reused by `relational::txn::Transaction::commit`.
pub(crate) fn build_put_op(
    table: &TableRow,
    encoded_pk: &[u8],
    columns: &[ColumnRow],
    values: &[Option<RelationalValue>],
) -> Result<WriteOp> {
    let pk_set = TableStore::pk_ordinal_set(table);
    let key = table_row_key(table.table_id, encoded_pk);

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
/// non-primary-key fields. `pub(crate)`: reused by `IndexBuilder`'s
/// backfill re-validation step (`src/relational/index.rs`), which reads
/// a table row's raw current bytes directly (bypassing `get_row`'s own
/// redundant PK-length check, already known to be correct here).
pub(crate) fn decode_full_row(
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
