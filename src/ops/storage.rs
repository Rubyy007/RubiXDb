//! Per-table / per-index storage accounting from one snapshot scan: the
//! operator's "how big is each table and index" view. Pure read, bounded
//! memory (one counter pair per table/index id), cancellable.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::catalog::encoding::CATALOG_NAMESPACE;
use crate::lsm::LsmEngine;
use crate::ops::catalog_mirror::CatalogMirror;
use crate::ops::{codes, OpsError};
use crate::relational::key::RELATIONAL_NAMESPACE;

#[derive(Debug, Clone, Default)]
pub struct IndexStorage {
    pub index_id: u32,
    pub name: String,
    pub state: String,
    pub entries: u64,
    /// Sum of key + value bytes (logical, before block compression/overhead).
    pub bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TableStorage {
    pub table_id: u32,
    pub name: String,
    pub schema: String,
    pub rows: u64,
    pub row_bytes: u64,
    pub indexes: Vec<IndexStorage>,
}

#[derive(Debug, Clone, Default)]
pub struct StorageReport {
    pub snapshot_seq: u64,
    pub tables: Vec<TableStorage>,
    pub catalog_entries: u64,
    pub catalog_bytes: u64,
    pub orphan_entries: u64,
    pub orphan_bytes: u64,
    pub other_entries: u64,
    pub other_bytes: u64,
}

pub fn storage_report(
    engine: &LsmEngine,
    cancel: Option<&AtomicBool>,
) -> Result<StorageReport, OpsError> {
    let snapshot = engine.snapshot();
    let seq = snapshot.seq();
    let mut report = StorageReport {
        snapshot_seq: seq,
        ..Default::default()
    };
    let mut mirror = CatalogMirror::default();
    let mut counts: BTreeMap<(u32, u32), (u64, u64)> = BTreeMap::new();

    for (n, item) in engine
        .range_scan(Bound::Unbounded, Bound::Unbounded, seq)
        .enumerate()
    {
        if (n as u64).is_multiple_of(8192) && cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(OpsError::new(codes::CANCELLED, "storage report cancelled"));
        }
        let (k, v) = item?;
        let bytes = (k.len() + v.len()) as u64;
        match k.first().copied() {
            Some(CATALOG_NAMESPACE) => {
                report.catalog_entries += 1;
                report.catalog_bytes += bytes;
                let _ = mirror.ingest(&k, &v);
            }
            Some(RELATIONAL_NAMESPACE) if k.len() >= 9 => {
                let t = u32::from_be_bytes(k[1..5].try_into().unwrap());
                let i = u32::from_be_bytes(k[5..9].try_into().unwrap());
                let e = counts.entry((t, i)).or_insert((0, 0));
                e.0 += 1;
                e.1 += bytes;
            }
            _ => {
                report.other_entries += 1;
                report.other_bytes += bytes;
            }
        }
    }

    for t in mirror.tables.values() {
        let schema = mirror
            .schemas
            .get(&t.schema_id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let (rows, row_bytes) = counts.remove(&(t.table_id, 0)).unwrap_or((0, 0));
        let mut ts = TableStorage {
            table_id: t.table_id,
            name: t.name.clone(),
            schema,
            rows,
            row_bytes,
            indexes: Vec::new(),
        };
        for i in mirror.indexes.values().filter(|i| i.table_id == t.table_id) {
            let (entries, bytes) = counts.remove(&(t.table_id, i.index_id)).unwrap_or((0, 0));
            ts.indexes.push(IndexStorage {
                index_id: i.index_id,
                name: i.name.clone(),
                state: format!("{:?}", i.state),
                entries,
                bytes,
            });
        }
        report.tables.push(ts);
    }
    for (entries, bytes) in counts.values() {
        report.orphan_entries += entries;
        report.orphan_bytes += bytes;
    }
    drop(snapshot);
    Ok(report)
}
