//! Integrity verification (`rubixdb check`).
//!
//! Two independent passes, both read-only:
//!
//! * [`check_engine`] — **logical**: scans the raw keyspace at ONE snapshot
//!   sequence (so it is consistent even while writers run) and validates the
//!   catalog, every table row, every secondary-index entry and the
//!   row↔index correspondence using its own raw decoding. It never goes
//!   through the SQL layer and never trusts a query result: a table whose
//!   rows decode fine through `SELECT` can still be reported here (stale or
//!   missing index entries, rows that violate the schema, dangling data).
//! * [`check_physical`] — **filesystem / storage layout** of a *stopped*
//!   data directory: manifest replay, every live SSTable's footer and every
//!   data block checksum, WAL segment framing — before any recovery runs.
//!
//! Nothing here repairs anything. See
//! `PHASE_RUBIXDB_INTEGRITY_ARCHITECTURE.md`.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::catalog::encoding::{CATALOG_NAMESPACE, SYSTEM_TABLE_COLUMNS};
use crate::catalog::schema::{ColumnRow, IndexKind, IndexRow, IndexState, TableRow, TableState};
use crate::lsm::LsmEngine;
use crate::ops::catalog_mirror::CatalogMirror;
use crate::relational::index_key::{
    decode_indexed_columns, index_entry_range, MAX_INDEX_KEY_BYTES,
};
use crate::relational::key::{
    decode_composite_key, encode_composite_key, table_row_key, table_row_range,
    RELATIONAL_NAMESPACE,
};
use crate::relational::table_store::{
    decode_full_row, indexed_entry_key, relational_type_from_column,
};
use crate::relational::value::{RelationalType, RelationalValue};

/// Closed set of finding codes (safe as labels).
pub mod finding_codes {
    pub const CATALOG_KEY_MALFORMED: &str = "CATALOG_KEY_MALFORMED";
    pub const CATALOG_ROW_UNDECODABLE: &str = "CATALOG_ROW_UNDECODABLE";
    pub const CATALOG_DANGLING_REF: &str = "CATALOG_DANGLING_REF";
    pub const CATALOG_COLUMNS_INVALID: &str = "CATALOG_COLUMNS_INVALID";
    pub const CATALOG_COUNTER_BEHIND: &str = "CATALOG_COUNTER_BEHIND";
    pub const ROW_KEY_UNDECODABLE: &str = "ROW_KEY_UNDECODABLE";
    pub const ROW_KEY_NONCANONICAL: &str = "ROW_KEY_NONCANONICAL";
    pub const ROW_VALUE_UNDECODABLE: &str = "ROW_VALUE_UNDECODABLE";
    pub const ROW_NULL_VIOLATION: &str = "ROW_NULL_VIOLATION";
    pub const ROW_SCHEMA_VERSION_AHEAD: &str = "ROW_SCHEMA_VERSION_AHEAD";
    pub const INDEX_ENTRY_UNDECODABLE: &str = "INDEX_ENTRY_UNDECODABLE";
    pub const INDEX_ENTRY_DANGLING: &str = "INDEX_ENTRY_DANGLING";
    pub const INDEX_ENTRY_STALE: &str = "INDEX_ENTRY_STALE";
    pub const INDEX_ENTRY_MISSING: &str = "INDEX_ENTRY_MISSING";
    pub const INDEX_UNIQUE_VIOLATION: &str = "INDEX_UNIQUE_VIOLATION";
    pub const INDEX_NOT_READY: &str = "INDEX_NOT_READY";
    pub const TABLE_DROPPING: &str = "TABLE_DROPPING";
    pub const ORPHAN_TABLE_DATA: &str = "ORPHAN_TABLE_DATA";
    pub const ORPHAN_INDEX_DATA: &str = "ORPHAN_INDEX_DATA";
    pub const NON_RELATIONAL_KEYS: &str = "NON_RELATIONAL_KEYS";
    pub const LAYOUT_MISSING: &str = "LAYOUT_MISSING";
    pub const UNEXPECTED_FILE: &str = "UNEXPECTED_FILE";
    pub const MANIFEST_CORRUPT: &str = "MANIFEST_CORRUPT";
    pub const MANIFEST_TORN_TAIL: &str = "MANIFEST_TORN_TAIL";
    pub const SSTABLE_MISSING: &str = "SSTABLE_MISSING";
    pub const SSTABLE_CORRUPT: &str = "SSTABLE_CORRUPT";
    pub const SSTABLE_SIZE_MISMATCH: &str = "SSTABLE_SIZE_MISMATCH";
    pub const SSTABLE_ORPHAN: &str = "SSTABLE_ORPHAN";
    pub const WAL_CORRUPT: &str = "WAL_CORRUPT";
    pub const WAL_TORN_TAIL: &str = "WAL_TORN_TAIL";
    pub const WAL_UNREADABLE: &str = "WAL_UNREADABLE";
    pub const CHECK_INCOMPLETE: &str = "CHECK_INCOMPLETE";
}
use finding_codes as fc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Informational; nothing is wrong.
    Info,
    /// Safe-to-run condition that an operator should know about (data left
    /// behind, an interrupted background operation, a recoverable torn tail).
    Warning,
    /// The stored state is inconsistent or unreadable.
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub code: &'static str,
    /// What the finding is about, e.g. `table[3]`, `index[7]`. Never row data.
    pub object: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct CheckStats {
    pub snapshot_seq: u64,
    pub catalog_rows: u64,
    pub tables_checked: u64,
    pub rows_checked: u64,
    pub indexes_checked: u64,
    pub index_entries_checked: u64,
    pub orphan_entries: u64,
    pub non_relational_entries: u64,
    pub duration: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    /// At most `CheckOptions::max_findings` retained; `counts` is complete.
    pub findings: Vec<Finding>,
    pub counts: BTreeMap<&'static str, u64>,
    pub errors: u64,
    pub warnings: u64,
    pub stats: CheckStats,
    pub complete: bool,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.errors == 0 && self.complete
    }
}

#[derive(Clone)]
pub struct CheckOptions<'a> {
    pub max_findings: usize,
    pub cancel: Option<&'a AtomicBool>,
}

impl Default for CheckOptions<'_> {
    fn default() -> Self {
        CheckOptions {
            max_findings: 200,
            cancel: None,
        }
    }
}

struct Sink {
    report: CheckReport,
    max: usize,
}

impl Sink {
    fn add(
        &mut self,
        severity: Severity,
        code: &'static str,
        object: impl Into<String>,
        detail: impl Into<String>,
    ) {
        match severity {
            Severity::Error => self.report.errors += 1,
            Severity::Warning => self.report.warnings += 1,
            Severity::Info => {}
        }
        *self.report.counts.entry(code).or_insert(0) += 1;
        if self.report.findings.len() < self.max {
            self.report.findings.push(Finding {
                severity,
                code,
                object: object.into(),
                detail: detail.into(),
            });
        }
    }
}

fn range_end_exclusive(prefix_byte: u8) -> Vec<u8> {
    vec![prefix_byte + 1]
}

struct TableMeta {
    row: TableRow,
    columns: Vec<ColumnRow>,
    pk_types: Vec<RelationalType>,
    col_types: Vec<RelationalType>,
}

fn build_meta(mirror: &CatalogMirror, table: &TableRow) -> Result<TableMeta, String> {
    let columns: Vec<ColumnRow> = mirror
        .columns
        .range((table.table_id, 0)..=(table.table_id, u16::MAX))
        .map(|(_, c)| c.clone())
        .collect();
    let col_types: Vec<RelationalType> = columns
        .iter()
        .map(relational_type_from_column)
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let pk_types = table
        .pk_ordinals
        .iter()
        .map(|&o| {
            col_types
                .get(usize::from(o))
                .copied()
                .ok_or_else(|| format!("primary key ordinal {o} out of range"))
        })
        .collect::<Result<_, _>>()?;
    Ok(TableMeta {
        row: table.clone(),
        columns,
        pk_types,
        col_types,
    })
}

fn bound_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// Logical integrity check at one snapshot (see module docs). Never fails
/// as a function: engine read errors become `Error` findings and mark the
/// report incomplete.
pub fn check_engine(engine: &LsmEngine, opts: &CheckOptions<'_>) -> CheckReport {
    let started = Instant::now();
    let snapshot = engine.snapshot();
    let seq = snapshot.seq();
    let mut sink = Sink {
        report: CheckReport::default(),
        max: opts.max_findings,
    };
    sink.report.stats.snapshot_seq = seq;
    let cancelled = || opts.cancel.is_some_and(|c| c.load(Ordering::Relaxed));

    // ---------------- catalog ----------------
    let mut mirror = CatalogMirror::default();
    let cat_start = vec![CATALOG_NAMESPACE];
    let cat_end = range_end_exclusive(CATALOG_NAMESPACE);
    let mut read_failed = false;
    for item in engine.range_scan(Bound::Included(&cat_start), Bound::Excluded(&cat_end), seq) {
        match item {
            Ok((k, v)) => {
                sink.report.stats.catalog_rows += 1;
                if let Err(p) = mirror.ingest(&k, &v) {
                    sink.add(Severity::Error, p.code, p.object, p.detail);
                }
            }
            Err(e) => {
                sink.add(
                    Severity::Error,
                    fc::CHECK_INCOMPLETE,
                    "catalog",
                    format!("scan failed: {e}"),
                );
                read_failed = true;
                break;
            }
        }
    }
    for p in mirror.validate() {
        sink.add(Severity::Error, p.code, p.object, p.detail);
    }
    let _ = SYSTEM_TABLE_COLUMNS;

    // ---------------- relational data ----------------
    let mut complete = !read_failed;
    let rel_end = range_end_exclusive(RELATIONAL_NAMESPACE);
    let mut next_start: Vec<u8> = vec![RELATIONAL_NAMESPACE];
    'tables: while complete {
        if cancelled() {
            complete = false;
            sink.add(
                Severity::Error,
                fc::CHECK_INCOMPLETE,
                "check",
                "cancelled before completion",
            );
            break;
        }
        // Find the next table id present in the data.
        let first = engine
            .range_scan(Bound::Included(&next_start), Bound::Excluded(&rel_end), seq)
            .next();
        let Some(first) = first else { break };
        let (key, _) = match first {
            Ok(kv) => kv,
            Err(e) => {
                sink.add(
                    Severity::Error,
                    fc::CHECK_INCOMPLETE,
                    "relational",
                    format!("scan failed: {e}"),
                );
                complete = false;
                break;
            }
        };
        if key.len() < 9 {
            sink.add(
                Severity::Error,
                fc::CATALOG_KEY_MALFORMED,
                "relational",
                "key shorter than table_id+index_id",
            );
            // Skip past this single key.
            next_start = key.clone();
            next_start.push(0);
            continue;
        }
        let table_id = u32::from_be_bytes(key[1..5].try_into().unwrap());
        // Where the next table starts.
        let after_table = match table_id.checked_add(1) {
            Some(n) => {
                let mut v = vec![RELATIONAL_NAMESPACE];
                v.extend_from_slice(&n.to_be_bytes());
                Some(v)
            }
            None => None,
        };

        match mirror.tables.get(&table_id) {
            None => {
                // Data of a table that is not in the catalog.
                let n = count_range(
                    engine,
                    &key_prefix(table_id),
                    after_table.as_deref(),
                    &rel_end,
                    seq,
                );
                sink.report.stats.orphan_entries += n;
                sink.add(
                    Severity::Warning,
                    fc::ORPHAN_TABLE_DATA,
                    format!("table[{table_id}]"),
                    format!("{n} stored entries belong to a table id that is not in the catalog (left behind by DROP TABLE; reclaimable)"),
                );
            }
            Some(table) => {
                if table.state == TableState::Dropping {
                    sink.add(
                        Severity::Warning,
                        fc::TABLE_DROPPING,
                        format!("table[{table_id}]"),
                        "table is marked Dropping",
                    );
                }
                match build_meta(&mirror, table) {
                    Ok(meta) => {
                        check_table(engine, seq, &mirror, &meta, &mut sink, &cancelled);
                    }
                    Err(e) => {
                        sink.add(
                            Severity::Error,
                            fc::CATALOG_COLUMNS_INVALID,
                            format!("table[{table_id}]"),
                            e,
                        );
                    }
                }
                // Index ids present under this table but absent from the catalog.
                check_orphan_indexes(engine, seq, &mirror, table_id, &rel_end, &mut sink);
            }
        }
        match after_table {
            Some(n) => next_start = n,
            None => break 'tables,
        }
    }
    if cancelled() {
        complete = false;
    }

    // ---------------- other namespaces ----------------
    let other_start = vec![RELATIONAL_NAMESPACE + 1];
    let mut other = 0u64;
    for item in engine.range_scan(Bound::Included(&other_start), Bound::Unbounded, seq) {
        match item {
            Ok(_) => other += 1,
            Err(e) => {
                sink.add(
                    Severity::Error,
                    fc::CHECK_INCOMPLETE,
                    "keyspace",
                    format!("scan failed: {e}"),
                );
                complete = false;
                break;
            }
        }
    }
    sink.report.stats.non_relational_entries = other;
    if other > 0 {
        sink.add(
            Severity::Info,
            fc::NON_RELATIONAL_KEYS,
            "keyspace",
            format!("{other} entries outside the catalog/relational namespaces (raw key-value API data)"),
        );
    }

    sink.report.complete = complete;
    sink.report.stats.duration = started.elapsed();
    drop(snapshot);
    sink.report
}

fn key_prefix(table_id: u32) -> Vec<u8> {
    let mut v = vec![RELATIONAL_NAMESPACE];
    v.extend_from_slice(&table_id.to_be_bytes());
    v
}

fn count_range(
    engine: &LsmEngine,
    start: &[u8],
    end: Option<&[u8]>,
    rel_end: &[u8],
    seq: u64,
) -> u64 {
    let end_b = match end {
        Some(e) => Bound::Excluded(e),
        None => Bound::Excluded(rel_end),
    };
    engine
        .range_scan(Bound::Included(start), end_b, seq)
        .filter(|r| r.is_ok())
        .count() as u64
}

fn check_orphan_indexes(
    engine: &LsmEngine,
    seq: u64,
    mirror: &CatalogMirror,
    table_id: u32,
    rel_end: &[u8],
    sink: &mut Sink,
) {
    let mut start = key_prefix(table_id);
    start.extend_from_slice(&1u32.to_be_bytes()); // first index id
    let table_end = match table_id.checked_add(1) {
        Some(n) => {
            let mut v = vec![RELATIONAL_NAMESPACE];
            v.extend_from_slice(&n.to_be_bytes());
            v
        }
        None => rel_end.to_vec(),
    };
    loop {
        let first = engine
            .range_scan(Bound::Included(&start), Bound::Excluded(&table_end), seq)
            .next();
        let Some(Ok((k, _))) = first else { return };
        if k.len() < 9 {
            return;
        }
        let index_id = u32::from_be_bytes(k[5..9].try_into().unwrap());
        let known = mirror
            .indexes
            .get(&index_id)
            .is_some_and(|i| i.table_id == table_id);
        if !known {
            let mut istart = key_prefix(table_id);
            istart.extend_from_slice(&index_id.to_be_bytes());
            let iend = match index_id.checked_add(1) {
                Some(n) => {
                    let mut v = key_prefix(table_id);
                    v.extend_from_slice(&n.to_be_bytes());
                    v
                }
                None => table_end.clone(),
            };
            let n = count_range(engine, &istart, Some(&iend), rel_end, seq);
            sink.report.stats.orphan_entries += n;
            sink.add(
                Severity::Warning,
                fc::ORPHAN_INDEX_DATA,
                format!("index[{index_id}]"),
                format!("{n} index entries under table[{table_id}] belong to an index id that is not in the catalog"),
            );
        }
        match index_id.checked_add(1) {
            Some(n) => {
                start = key_prefix(table_id);
                start.extend_from_slice(&n.to_be_bytes());
            }
            None => return,
        }
    }
}

fn check_table(
    engine: &LsmEngine,
    seq: u64,
    mirror: &CatalogMirror,
    meta: &TableMeta,
    sink: &mut Sink,
    cancelled: &dyn Fn() -> bool,
) {
    let tid = meta.row.table_id;
    let obj = format!("table[{tid}]");
    sink.report.stats.tables_checked += 1;
    let indexes: Vec<&IndexRow> = mirror
        .indexes
        .values()
        .filter(|i| i.table_id == tid && i.kind != IndexKind::Primary)
        .collect();
    let ready: Vec<&IndexRow> = indexes
        .iter()
        .copied()
        .filter(|i| i.state == IndexState::Ready)
        .collect();
    for i in &indexes {
        if i.state != IndexState::Ready {
            sink.add(
                Severity::Warning,
                fc::INDEX_NOT_READY,
                format!("index[{}]", i.index_id),
                format!(
                    "index on table[{tid}] is in state {:?}; completeness is not asserted for it",
                    i.state
                ),
            );
        }
    }

    // ---- rows ----
    let (start, end) = table_row_range(tid);
    let mut rows = 0u64;
    for item in engine.range_scan(bound_ref(&start), bound_ref(&end), seq) {
        if rows.is_multiple_of(4096) && cancelled() {
            return;
        }
        let (key, value) = match item {
            Ok(kv) => kv,
            Err(e) => {
                sink.add(
                    Severity::Error,
                    fc::CHECK_INCOMPLETE,
                    &obj,
                    format!("row scan failed: {e}"),
                );
                return;
            }
        };
        rows += 1;
        let pk_bytes = &key[9..];
        let pk_values = match decode_composite_key(&meta.pk_types, pk_bytes) {
            Ok(v) => v,
            Err(e) => {
                sink.add(
                    Severity::Error,
                    fc::ROW_KEY_UNDECODABLE,
                    &obj,
                    format!("a row key does not decode: {e}"),
                );
                continue;
            }
        };
        match encode_composite_key(&pk_values) {
            Ok(re) if re == pk_bytes => {}
            _ => {
                sink.add(
                    Severity::Error,
                    fc::ROW_KEY_NONCANONICAL,
                    &obj,
                    "a row key is not the canonical encoding of its own values",
                );
                continue;
            }
        }
        let row = match decode_full_row(&meta.row, &meta.columns, &pk_values, &value) {
            Ok(r) => r,
            Err(e) => {
                sink.add(
                    Severity::Error,
                    fc::ROW_VALUE_UNDECODABLE,
                    &obj,
                    format!("a row value does not decode: {e}"),
                );
                continue;
            }
        };
        // Envelope schema_version sits at bytes 1..5 (format_version:u8 first).
        if value.len() >= 5 {
            let sv = u32::from_le_bytes(value[1..5].try_into().unwrap());
            if sv > meta.row.schema_version {
                sink.add(
                    Severity::Error,
                    fc::ROW_SCHEMA_VERSION_AHEAD,
                    &obj,
                    format!(
                        "row schema_version {sv} is newer than the table's {}",
                        meta.row.schema_version
                    ),
                );
            }
        }
        for (c, v) in meta.columns.iter().zip(row.iter()) {
            if v.is_none() && !c.nullable {
                sink.add(
                    Severity::Error,
                    fc::ROW_NULL_VIOLATION,
                    &obj,
                    format!("NULL in NOT NULL column ordinal {}", c.ordinal),
                );
            }
        }
        // row -> every Ready index must hold exactly the expected entry.
        for idx in &ready {
            match indexed_entry_key(tid, idx, &row, pk_bytes) {
                Ok(expected) => match engine.contains(&expected, seq) {
                    Ok(true) => {}
                    Ok(false) => sink.add(
                        Severity::Error,
                        fc::INDEX_ENTRY_MISSING,
                        format!("index[{}]", idx.index_id),
                        format!("a row of table[{tid}] has no entry in this Ready index"),
                    ),
                    Err(e) => sink.add(
                        Severity::Error,
                        fc::CHECK_INCOMPLETE,
                        &obj,
                        format!("read failed: {e}"),
                    ),
                },
                Err(e) => sink.add(
                    Severity::Error,
                    fc::INDEX_ENTRY_UNDECODABLE,
                    format!("index[{}]", idx.index_id),
                    format!("expected entry for a row cannot be built: {e}"),
                ),
            }
        }
    }
    sink.report.stats.rows_checked += rows;

    // ---- index entries -> rows ----
    for idx in &ready {
        sink.report.stats.indexes_checked += 1;
        check_index_entries(engine, seq, meta, idx, sink, cancelled);
    }
}

fn check_index_entries(
    engine: &LsmEngine,
    seq: u64,
    meta: &TableMeta,
    idx: &IndexRow,
    sink: &mut Sink,
    cancelled: &dyn Fn() -> bool,
) {
    let tid = meta.row.table_id;
    let obj = format!("index[{}]", idx.index_id);
    let idx_types: Vec<RelationalType> = match idx
        .column_ordinals
        .iter()
        .map(|&o| meta.col_types.get(usize::from(o)).copied())
        .collect::<Option<Vec<_>>>()
    {
        Some(t) => t,
        None => return, // already reported as CATALOG_COLUMNS_INVALID
    };
    let (start, end) = index_entry_range(tid, idx.index_id);
    let mut prev_prefix: Option<Vec<u8>> = None;
    for (n, item) in engine
        .range_scan(bound_ref(&start), bound_ref(&end), seq)
        .enumerate()
    {
        if (n as u64).is_multiple_of(4096) && cancelled() {
            return;
        }
        let (key, value) = match item {
            Ok(kv) => kv,
            Err(e) => {
                sink.add(
                    Severity::Error,
                    fc::CHECK_INCOMPLETE,
                    &obj,
                    format!("index scan failed: {e}"),
                );
                return;
            }
        };
        sink.report.stats.index_entries_checked += 1;
        if key.len() > MAX_INDEX_KEY_BYTES || !value.is_empty() {
            sink.add(
                Severity::Error,
                fc::INDEX_ENTRY_UNDECODABLE,
                &obj,
                "entry key too large or value not the empty marker",
            );
            continue;
        }
        let rest = &key[9..];
        let (indexed, consumed) = match decode_indexed_columns(&idx_types, rest) {
            Ok(v) => v,
            Err(e) => {
                sink.add(
                    Severity::Error,
                    fc::INDEX_ENTRY_UNDECODABLE,
                    &obj,
                    format!("indexed columns do not decode: {e}"),
                );
                continue;
            }
        };
        let pk_bytes = &rest[consumed..];
        if decode_composite_key(&meta.pk_types, pk_bytes).is_err() {
            sink.add(
                Severity::Error,
                fc::INDEX_ENTRY_UNDECODABLE,
                &obj,
                "primary-key suffix does not decode",
            );
            continue;
        }
        let row_key = table_row_key(tid, pk_bytes);
        match engine.get_as_of(&row_key, seq) {
            Ok(Some(row_value)) => {
                let pk_values = decode_composite_key(&meta.pk_types, pk_bytes).unwrap_or_default();
                match decode_full_row(&meta.row, &meta.columns, &pk_values, &row_value) {
                    Ok(row) => {
                        let actual: Vec<Option<RelationalValue>> = idx
                            .column_ordinals
                            .iter()
                            .map(|&o| row.get(usize::from(o)).cloned().flatten())
                            .collect();
                        if actual != indexed {
                            sink.add(
                                Severity::Error,
                                fc::INDEX_ENTRY_STALE,
                                &obj,
                                format!(
                                    "an entry's indexed values differ from its row in table[{tid}]"
                                ),
                            );
                        }
                    }
                    Err(_) => { /* reported by the row pass */ }
                }
            }
            Ok(None) => sink.add(
                Severity::Error,
                fc::INDEX_ENTRY_DANGLING,
                &obj,
                format!("an entry points at a row that does not exist in table[{tid}]"),
            ),
            Err(e) => sink.add(
                Severity::Error,
                fc::CHECK_INCOMPLETE,
                &obj,
                format!("read failed: {e}"),
            ),
        }
        if idx.kind == IndexKind::Unique {
            let prefix = &rest[..consumed];
            let has_null = indexed.iter().any(|v| v.is_none());
            if !has_null {
                if prev_prefix.as_deref() == Some(prefix) {
                    sink.add(
                        Severity::Error,
                        fc::INDEX_UNIQUE_VIOLATION,
                        &obj,
                        "two entries of a UNIQUE index share the same indexed values",
                    );
                }
                prev_prefix = Some(prefix.to_vec());
            } else {
                prev_prefix = None;
            }
        }
    }
}

// ---------------------------------------------------------------------
// Physical (stopped data directory) check
// ---------------------------------------------------------------------

/// Read-only structural verification of a data directory that no engine has
/// open. Never creates, truncates or deletes anything. Must be run *before*
/// any recovery so it sees the state a crash left behind.
pub fn check_physical(data_dir: &Path) -> CheckReport {
    let started = Instant::now();
    let mut sink = Sink {
        report: CheckReport::default(),
        max: 500,
    };
    if !data_dir.is_dir() {
        sink.add(
            Severity::Error,
            fc::LAYOUT_MISSING,
            "data directory",
            "directory does not exist",
        );
        sink.report.complete = true;
        return sink.report;
    }
    // ---- layout ----
    if let Ok(rd) = std::fs::read_dir(data_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let known = name == "MANIFEST"
                || name == "wal"
                || name == "sstables"
                || name == crate::wal::LOCK_FILE_NAME
                || name == "MANIFEST.tmp"
                || name == crate::ops::format::DATA_FORMAT_FILE
                || name == crate::ops::restore::MARKER_FILE;
            if !known {
                sink.add(
                    Severity::Info,
                    fc::UNEXPECTED_FILE,
                    "data directory",
                    format!("unrecognised entry {name:?}"),
                );
            }
        }
    }

    // ---- manifest ----
    let mut live: BTreeMap<u64, crate::manifest::SstableManifestEntry> = BTreeMap::new();
    match crate::manifest::replay_readonly(data_dir) {
        Ok(r) => {
            if r.truncated {
                sink.add(
                    Severity::Warning,
                    fc::MANIFEST_TORN_TAIL,
                    "MANIFEST",
                    "a torn tail was found; recovery truncates it (expected after a crash)",
                );
            }
            live = r.state.live_sstables.clone();
        }
        Err(e) => sink.add(
            Severity::Error,
            fc::MANIFEST_CORRUPT,
            "MANIFEST",
            e.to_string(),
        ),
    }

    // ---- sstables ----
    let sst_dir = data_dir.join("sstables");
    let mut on_disk: BTreeMap<u64, std::path::PathBuf> = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(&sst_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(id) = crate::sstable::parse_sstable_id(&name) {
                on_disk.insert(id, e.path());
            } else if name.ends_with(".sst.tmp") {
                sink.add(
                    Severity::Info,
                    fc::UNEXPECTED_FILE,
                    format!("sstable {name}"),
                    "interrupted build output; removed at startup",
                );
            }
        }
    }
    for (id, entry) in &live {
        let obj = format!("sstable[{id}]");
        let Some(path) = on_disk.get(id) else {
            sink.add(
                Severity::Error,
                fc::SSTABLE_MISSING,
                &obj,
                "listed live by the manifest but the file is absent",
            );
            continue;
        };
        match std::fs::metadata(path) {
            Ok(m) if m.len() != entry.file_size => sink.add(
                Severity::Error,
                fc::SSTABLE_SIZE_MISMATCH,
                &obj,
                format!(
                    "file is {} bytes, manifest says {}",
                    m.len(),
                    entry.file_size
                ),
            ),
            Ok(_) => {}
            Err(e) => sink.add(Severity::Error, fc::SSTABLE_CORRUPT, &obj, e.to_string()),
        }
        match crate::sstable::SsTable::open(path, *id) {
            Ok(t) => {
                let mut records = 0u64;
                let mut failed = false;
                for item in t.range_scan_raw(Bound::Unbounded, Bound::Unbounded) {
                    match item {
                        Ok(_) => records += 1,
                        Err(e) => {
                            sink.add(
                                Severity::Error,
                                fc::SSTABLE_CORRUPT,
                                &obj,
                                format!("data block failed verification: {e}"),
                            );
                            failed = true;
                            break;
                        }
                    }
                }
                if !failed && records != t.record_count() {
                    sink.add(
                        Severity::Error,
                        fc::SSTABLE_CORRUPT,
                        &obj,
                        format!(
                            "{records} records readable, footer says {}",
                            t.record_count()
                        ),
                    );
                }
            }
            Err(e) => sink.add(Severity::Error, fc::SSTABLE_CORRUPT, &obj, e.to_string()),
        }
    }
    for id in on_disk.keys() {
        if !live.contains_key(id) {
            sink.add(Severity::Warning, fc::SSTABLE_ORPHAN, format!("sstable[{id}]"), "file is not listed live by the manifest (leftover of an interrupted flush/compaction)");
        }
    }

    // ---- WAL ----
    match crate::wal::inspect(data_dir, &crate::wal::WalConfig::default()) {
        Ok(r) => {
            if !r.corrupted_segments.is_empty() {
                sink.add(
                    Severity::Error,
                    fc::WAL_CORRUPT,
                    "wal",
                    format!("corrupted segment id(s): {:?}", r.corrupted_segments),
                );
            }
            if r.truncated {
                sink.add(Severity::Warning, fc::WAL_TORN_TAIL, "wal", "a torn tail was found in the newest segment; recovery truncates it (expected after a crash)");
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if matches!(e, crate::EngineError::Corruption { .. }) {
                sink.add(Severity::Error, fc::WAL_CORRUPT, "wal", msg);
            } else {
                sink.add(Severity::Error, fc::WAL_UNREADABLE, "wal", msg);
            }
        }
    }
    sink.report.complete = true;
    sink.report.stats.duration = started.elapsed();
    sink.report
}

/// Used by tests and the CLI to turn a report into process-exit semantics.
pub fn exit_class(report: &CheckReport) -> i32 {
    if !report.complete {
        3
    } else if report.errors > 0 {
        2
    } else if report.warnings > 0 {
        1
    } else {
        0
    }
}
