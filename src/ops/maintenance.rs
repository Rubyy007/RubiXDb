//! Maintenance operations built on the public engine API.
//!
//! Currently: reclaiming the storage `DROP TABLE` / `DROP INDEX` leave
//! behind. `CatalogService::drop_table` removes the catalog rows only; the
//! table's row keys and index entries stay in the keyspace forever
//! (measured and documented in `PHASE_RUBIXDB_MAINTENANCE_ARCHITECTURE.md`).
//!
//! Safety argument for the purge, in the order it is enforced:
//!
//! * IDs are issued from monotonic counters (`system.counters`) that are
//!   bumped in the *same* atomic batch that creates the object, and never
//!   reused. So at one snapshot `S`, an id that is `<= counter(S)` and not in
//!   the catalog at `S` belongs to an object that was created and has since
//!   been removed; nobody can create it again and nobody writes to it.
//!   Anything `> counter(S)` is never touched.
//! * Everything is decided at ONE snapshot, so an in-flight `CREATE TABLE`
//!   cannot be mistaken for an orphan.
//! * The operation is a dry run unless `apply` is set, and `apply` also
//!   requires `expected_entries` to equal the freshly computed plan: the
//!   caller must have looked at the plan, and a changed database refuses.
//! * Deletes are tombstone `write_batch`es of bounded size; space returns
//!   when compaction rewrites the affected SSTables.

use std::collections::BTreeSet;
use std::ops::Bound;

use crate::catalog::encoding::CATALOG_NAMESPACE;
use crate::lsm::{LsmEngine, WriteOp};
use crate::ops::catalog_mirror::CatalogMirror;
use crate::ops::{codes, OpsError};
use crate::relational::key::RELATIONAL_NAMESPACE;

const DELETE_BATCH: usize = 1000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgePlan {
    pub snapshot_seq: u64,
    pub orphan_table_ids: Vec<u32>,
    /// `(table_id, index_id)` of index data whose table still exists.
    pub orphan_index_ids: Vec<(u32, u32)>,
    pub entries: u64,
}

#[derive(Debug, Clone)]
pub struct PurgeReport {
    pub plan: PurgePlan,
    pub applied: bool,
    pub deleted: u64,
    /// Entries still present for the purged ids afterwards (must be 0).
    pub remaining: u64,
}

fn count_prefix(engine: &LsmEngine, start: &[u8], end: &[u8], seq: u64) -> Result<u64, OpsError> {
    let mut n = 0;
    for item in engine.range_scan(Bound::Included(start), Bound::Excluded(end), seq) {
        item?;
        n += 1;
    }
    Ok(n)
}

fn table_range(table_id: u32) -> (Vec<u8>, Vec<u8>) {
    let mut s = vec![RELATIONAL_NAMESPACE];
    s.extend_from_slice(&table_id.to_be_bytes());
    let e = match table_id.checked_add(1) {
        Some(n) => {
            let mut v = vec![RELATIONAL_NAMESPACE];
            v.extend_from_slice(&n.to_be_bytes());
            v
        }
        None => vec![RELATIONAL_NAMESPACE + 1],
    };
    (s, e)
}

fn index_range(table_id: u32, index_id: u32) -> (Vec<u8>, Vec<u8>) {
    let mut s = vec![RELATIONAL_NAMESPACE];
    s.extend_from_slice(&table_id.to_be_bytes());
    s.extend_from_slice(&index_id.to_be_bytes());
    let e = match index_id.checked_add(1) {
        Some(n) => {
            let mut v = vec![RELATIONAL_NAMESPACE];
            v.extend_from_slice(&table_id.to_be_bytes());
            v.extend_from_slice(&n.to_be_bytes());
            v
        }
        None => table_range(table_id).1,
    };
    (s, e)
}

/// Computes what a purge would remove, at one snapshot.
pub fn plan_purge_orphans(engine: &LsmEngine) -> Result<PurgePlan, OpsError> {
    let snapshot = engine.snapshot();
    let seq = snapshot.seq();

    let mut mirror = CatalogMirror::default();
    let (cs, ce) = (vec![CATALOG_NAMESPACE], vec![CATALOG_NAMESPACE + 1]);
    for item in engine.range_scan(Bound::Included(&cs), Bound::Excluded(&ce), seq) {
        let (k, v) = item?;
        mirror.ingest(&k, &v).map_err(|p| {
            OpsError::new(
                codes::PRECONDITION,
                format!(
                    "catalog is not readable ({} {}); run `rubixdb check` first",
                    p.code, p.object
                ),
            )
        })?;
    }
    let table_counter = mirror.counters.get(&3).copied().unwrap_or(0);
    let index_counter = mirror.counters.get(&4).copied().unwrap_or(0);

    let mut table_ids: BTreeSet<u32> = BTreeSet::new();
    let mut index_ids: BTreeSet<(u32, u32)> = BTreeSet::new();
    // Enumerate distinct (table_id, index_id) groups by jumping.
    let rel_end = vec![RELATIONAL_NAMESPACE + 1];
    let mut next = vec![RELATIONAL_NAMESPACE];
    loop {
        let first = engine
            .range_scan(Bound::Included(&next), Bound::Excluded(&rel_end), seq)
            .next();
        let Some(first) = first else { break };
        let (k, _) = first?;
        if k.len() < 9 {
            next = k;
            next.push(0);
            continue;
        }
        let t = u32::from_be_bytes(k[1..5].try_into().unwrap());
        let i = u32::from_be_bytes(k[5..9].try_into().unwrap());
        if i == 0 {
            table_ids.insert(t);
        } else {
            index_ids.insert((t, i));
        }
        let (_, group_end) = if i == 0 {
            // rows of this table occupy index_id 0 only
            index_range(t, 0)
        } else {
            index_range(t, i)
        };
        next = group_end;
    }

    let mut plan = PurgePlan {
        snapshot_seq: seq,
        ..PurgePlan::default()
    };
    let mut dead_tables: BTreeSet<u32> = BTreeSet::new();
    let mut all_tables: BTreeSet<u32> = table_ids.clone();
    all_tables.extend(index_ids.iter().map(|(t, _)| *t));
    for t in all_tables {
        if t <= table_counter && !mirror.tables.contains_key(&t) {
            dead_tables.insert(t);
        }
    }
    for t in &dead_tables {
        let (s, e) = table_range(*t);
        plan.entries += count_prefix(engine, &s, &e, seq)?;
        plan.orphan_table_ids.push(*t);
    }
    for (t, i) in index_ids {
        if dead_tables.contains(&t) {
            continue; // already covered by the whole-table range
        }
        let live_table = mirror.tables.contains_key(&t);
        let known = mirror.indexes.get(&i).is_some_and(|ix| ix.table_id == t);
        if live_table && !known && i <= index_counter {
            let (s, e) = index_range(t, i);
            plan.entries += count_prefix(engine, &s, &e, seq)?;
            plan.orphan_index_ids.push((t, i));
        }
    }
    drop(snapshot);
    Ok(plan)
}

/// Dry run unless `apply`. With `apply`, `expected_entries` must equal the
/// recomputed plan's entry count.
pub fn purge_orphans(
    engine: &LsmEngine,
    apply: bool,
    expected_entries: Option<u64>,
) -> Result<PurgeReport, OpsError> {
    let plan = plan_purge_orphans(engine)?;
    if !apply {
        return Ok(PurgeReport {
            plan,
            applied: false,
            deleted: 0,
            remaining: 0,
        });
    }
    match expected_entries {
        Some(n) if n == plan.entries => {}
        Some(n) => {
            return Err(OpsError::new(
                codes::PRECONDITION,
                format!(
                    "the database changed since the plan was shown: {} entries now, {n} confirmed",
                    plan.entries
                ),
            ))
        }
        None => {
            return Err(OpsError::new(
                codes::NOT_CONFIRMED,
                "apply requires the entry count shown by the dry run",
            ))
        }
    }

    let mut ranges: Vec<(Vec<u8>, Vec<u8>)> = plan
        .orphan_table_ids
        .iter()
        .map(|t| table_range(*t))
        .collect();
    ranges.extend(
        plan.orphan_index_ids
            .iter()
            .map(|(t, i)| index_range(*t, *i)),
    );

    let mut deleted = 0u64;
    for (s, e) in &ranges {
        loop {
            let mut batch: Vec<WriteOp> = Vec::with_capacity(DELETE_BATCH);
            for item in engine.range(Bound::Included(s.as_slice()), Bound::Excluded(e.as_slice())) {
                let (k, _) = item?;
                batch.push(WriteOp::Delete { key: k });
                if batch.len() >= DELETE_BATCH {
                    break;
                }
            }
            if batch.is_empty() {
                break;
            }
            deleted += batch.len() as u64;
            engine.write_batch(&batch)?;
        }
    }

    let mut remaining = 0;
    for (s, e) in &ranges {
        remaining += count_prefix(engine, s, e, u64::MAX)?;
    }
    Ok(PurgeReport {
        plan,
        applied: true,
        deleted,
        remaining,
    })
}
