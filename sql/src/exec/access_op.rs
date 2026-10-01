//! `AccessOp` -- the one operator every table access executes through
//! (`PkLookup`, `PkRangeScan`, `IndexScan`, `SeqScan`), shared by `SELECT`,
//! JOIN inner sides, aggregation inputs and the target-finding pass of
//! `UPDATE`/`DELETE`.
//!
//! Increment 17 added snapshot-validity fallback and the index-vs-scan cost
//! decision. Increment 18 adds, in this one place:
//!
//! 1. **Transaction scan semantics** -- a transaction's own uncommitted
//!    insertions, updates and deletions are overlaid on every scan
//!    (`PHASE_RUBIXDB_INCREMENT18_TRANSACTION_SCAN_SEMANTICS.md`).
//! 2. **Unified candidate selection** -- a PK range and any number of
//!    secondary indexes that can all satisfy the predicate are priced by
//!    their *exact* row counts and the cheapest is executed
//!    (`PHASE_RUBIXDB_INCREMENT18_ACCESS_PATH_ARCHITECTURE.md`).
//! 3. **Lazy index row fetch** -- matching index entries are enumerated up
//!    front (key-only, bounded) but rows are fetched on demand, so `LIMIT`,
//!    cancellation and deadlines stop the work instead of discarding it
//!    (`PHASE_RUBIXDB_INCREMENT18_MATERIALIZATION_ARCHITECTURE.md`).

use std::ops::Bound;
use std::sync::Arc;

use rubixdb::relational::index::{IndexEntries, IndexProbe, IndexRowFetcher, IndexScanSpec};
use rubixdb::relational::key::encode_composite_key;
use rubixdb::relational::{RelationalValue, Row, TableOverlay};

use crate::bound::BoundExpr;
use crate::error::{Result, SqlError};
use crate::exec::cost::{self, AccessPathMode};
use crate::exec::expr_eval::eval_predicate;
use crate::exec::operators::{
    resolve_bound, resolve_pk_bound, resolve_values, value_ordering, Operator,
};
use crate::exec::{ExecCtx, RowContext, Tuple};
use crate::plan::access::{IndexAccessMode, IndexFallback, PhysicalAccess};

type BaseIter<'a> =
    Box<dyn Iterator<Item = rubixdb::relational::Result<(Vec<RelationalValue>, Row)>> + 'a>;

pub(super) enum AccessSource<'a> {
    /// `PkLookup`: at most one row, already fetched (through the
    /// transaction's own overlay-aware `get_row`).
    Single(Option<Row>),
    /// Fully materialized rows (an ordered fallback, an ordered index scan
    /// merged with the transaction's overlay, or an empty result).
    Vec(std::vec::IntoIter<(Vec<RelationalValue>, Row)>),
    /// A lazy, row-at-a-time scan (`SeqScan`, `PkRangeScan`, table-scan
    /// fallback) -- never materializes the table.
    Lazy(BaseIter<'a>),
    /// A lazy index fetch: the entries were enumerated (bounded, key-only);
    /// rows are fetched on demand. `extras` are the transaction's own
    /// uncommitted rows, tested against the complete predicate when reached.
    IndexFetch {
        fetcher: IndexRowFetcher<'a>,
        extras: std::vec::IntoIter<Row>,
    },
}

/// Merge state for a lazy scan over a transaction that has written the
/// scanned table: the base scan (primary-key order) is merged with the
/// transaction's key-ordered overlay in one pass.
struct OverlayMerge {
    overlay: Arc<TableOverlay>,
    pos: usize,
    pending: Option<(Vec<u8>, Row)>,
    base_done: bool,
}

pub(super) struct AccessOp<'a> {
    table_ref: u32,
    /// Applied to rows from the base scan.
    residual: Option<BoundExpr>,
    source: AccessSource<'a>,
    /// The outer context this access was built against -- merged with
    /// every fetched row before evaluating predicates, since a correlated
    /// predicate may still reference the outer side.
    outer: RowContext,
    /// Cost-model observation (statistics only).
    obs: Option<ScanObs>,
    /// Complete predicate applied to rows that came from the transaction's
    /// own overlay (a bound-consumed conjunct is not in `residual`).
    overlay_predicate: Option<BoundExpr>,
    merge: Option<OverlayMerge>,
}

/// Rows behind an index-path cost observation (below it fixed per-scan
/// overheads would distort the per-row figure).
const INDEX_COST_SAMPLE_ROWS: usize = 256;

/// Every `SCAN_SAMPLE_EVERY`-th row of a lazy scan is timed (fetch, decode,
/// and predicate evaluation -- the whole per-row cost), so the model learns
/// the real per-row cost of *this* machine and data at the price of two
/// clock reads per 16 rows.
const SCAN_SAMPLE_EVERY: u64 = 16;

enum ObsKind {
    /// A table scan (`SeqScan`, `PkRangeScan`, fallback).
    Seq,
    /// A lazy index fetch: `probe_ns` is the (already spent) entry
    /// enumeration, `entries` how many rows it will produce.
    Index { probe_ns: u64, entries: u64 },
}

/// Observes one lazy access for the cost model: its sampled per-row cost
/// and, for a scan that visited the whole table to exhaustion, the table's
/// exact row count. Purely statistical; never affects results.
struct ScanObs {
    table_id: u32,
    kind: ObsKind,
    /// `true` for a scan of the entire table.
    whole_table: bool,
    rows: u64,
    attempts: u64,
    sampled_rows: u64,
    sampled_ns: u64,
}

impl ScanObs {
    fn seq(table_id: u32, whole_table: bool) -> Self {
        ScanObs {
            table_id,
            kind: ObsKind::Seq,
            whole_table,
            rows: 0,
            attempts: 0,
            sampled_rows: 0,
            sampled_ns: 0,
        }
    }

    fn index(table_id: u32, probe_ns: u64, entries: u64) -> Self {
        ScanObs {
            table_id,
            kind: ObsKind::Index { probe_ns, entries },
            whole_table: false,
            rows: 0,
            attempts: 0,
            sampled_rows: 0,
            sampled_ns: 0,
        }
    }

    #[inline]
    fn start(&mut self) -> Option<std::time::Instant> {
        self.attempts += 1;
        if self.attempts % SCAN_SAMPLE_EVERY == 1 {
            Some(std::time::Instant::now())
        } else {
            None
        }
    }

    #[inline]
    fn stop(&mut self, started: Option<std::time::Instant>) {
        if let Some(t) = started {
            self.sampled_ns += t.elapsed().as_nanos() as u64;
            self.sampled_rows += 1;
        }
    }

    /// Called when the access is exhausted.
    fn finish(&self, ec: &ExecCtx) {
        let stats = ec.table_store.runtime_stats();
        match self.kind {
            ObsKind::Seq => {
                if self.whole_table {
                    stats.observe_row_count(self.table_id, self.rows);
                }
                stats.observe_seq_cost(self.sampled_rows, self.sampled_ns);
            }
            ObsKind::Index { probe_ns, entries } => {
                if entries as usize >= INDEX_COST_SAMPLE_ROWS && self.sampled_rows > 0 {
                    let per_row = self.sampled_ns / self.sampled_rows;
                    stats.observe_index_cost(entries, probe_ns + per_row * entries);
                }
            }
        }
    }
}

/// Why an access ran as a table scan instead of its planned path.
enum FallbackReason {
    /// F-2: no index candidate was `Ready` as of the transaction's snapshot.
    Snapshot,
    /// The cost model (or a forced mode) chose the table scan.
    Cost,
}

/// One executable candidate, with this execution's bound values resolved.
enum Cand<'p> {
    Index {
        index_id: u32,
        spec: IndexScanSpec,
        residual: &'p Option<BoundExpr>,
        fallback: &'p IndexFallback,
    },
    Pk {
        start: Bound<Vec<RelationalValue>>,
        end: Bound<Vec<RelationalValue>>,
        residual: &'p Option<BoundExpr>,
        fallback: &'p IndexFallback,
    },
}

/// What candidate selection decided.
enum Chosen {
    Index {
        cand: usize,
        entries: IndexEntries,
        probe_ns: u64,
    },
    Pk {
        cand: usize,
    },
    Seq(FallbackReason),
}

/// Ascending, `NULLS FIRST` comparison of two rows over `ordinals`, ties
/// broken by primary key -- the same comparator `SortOp` applies to an
/// `ORDER BY` item with those defaults, so a fallback's (or an overlay
/// merge's) order is exactly what the eliminated `Sort` would have
/// produced and exactly the physical index order.
fn compare_rows_by_ordinals(
    ordinals: &[u16],
    a: &(Vec<RelationalValue>, Row),
    b: &(Vec<RelationalValue>, Row),
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for &ord in ordinals {
        let av = a.1.get(ord as usize).and_then(|v| v.as_ref());
        let bv = b.1.get(ord as usize).and_then(|v| v.as_ref());
        let o = match (av, bv) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => value_ordering(x, y),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    for (x, y) in a.0.iter().zip(b.0.iter()) {
        let o = value_ordering(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

impl<'a> AccessOp<'a> {
    fn new(
        table_ref: u32,
        residual: Option<BoundExpr>,
        source: AccessSource<'a>,
        outer: &RowContext,
    ) -> Self {
        AccessOp {
            table_ref,
            residual,
            source,
            outer: outer.clone(),
            obs: None,
            overlay_predicate: None,
            merge: None,
        }
    }

    fn empty(table_ref: u32, outer: &RowContext) -> Self {
        Self::new(
            table_ref,
            None,
            AccessSource::Vec(Vec::new().into_iter()),
            outer,
        )
    }

    pub(super) fn build(
        access: &PhysicalAccess,
        outer: &RowContext,
        ec: &ExecCtx<'a>,
    ) -> Result<Self> {
        match access {
            PhysicalAccess::PkLookup {
                table_ref,
                key_values,
                residual,
                ..
            } => {
                let resolved = resolve_values(key_values, outer, ec)?;
                let row = match resolved {
                    None => None, // a NULL key component can never match (item 62)
                    Some(values) => {
                        ec.metrics.record_pk_lookup();
                        // `Transaction::get_row` is itself overlay-aware.
                        let row = ec.txn.get_row(access.table_id(), &values)?;
                        if row.is_some() {
                            ec.metrics.record_table_fetch();
                        }
                        row
                    }
                };
                Ok(Self::new(
                    *table_ref,
                    residual.clone(),
                    AccessSource::Single(row),
                    outer,
                ))
            }
            PhysicalAccess::SeqScan {
                table_ref,
                predicate,
                ..
            } => {
                ec.metrics.record_seq_scan();
                Self::lazy_table_scan(access.table_id(), *table_ref, predicate.clone(), outer, ec)
            }
            PhysicalAccess::IndexScan { table_ref, .. }
            | PhysicalAccess::PkRangeScan { table_ref, .. } => {
                Self::build_ranged(access, *table_ref, outer, ec)
            }
        }
    }

    /// A lazy scan of the whole table at the transaction's snapshot with
    /// `predicate` as the filter; merged with the transaction's own writes.
    fn lazy_table_scan(
        table_id: u32,
        table_ref: u32,
        predicate: Option<BoundExpr>,
        outer: &RowContext,
        ec: &ExecCtx<'a>,
    ) -> Result<Self> {
        let as_of = ec.txn.snapshot_seq();
        let iter = ec.table_store.scan_table_rows_as_of(table_id, as_of)?;
        let mut op = Self::new(
            table_ref,
            predicate.clone(),
            AccessSource::Lazy(Box::new(iter)),
            outer,
        );
        op.attach_overlay(table_id, predicate, ec);
        op.obs = Some(ScanObs::seq(table_id, op.merge.is_none()));
        Ok(op)
    }

    /// If the transaction has written this table, merge its writes over the
    /// lazy base scan.
    fn attach_overlay(&mut self, table_id: u32, complete: Option<BoundExpr>, ec: &ExecCtx<'a>) {
        if let Some(overlay) = ec.txn.overlay_for(table_id) {
            self.overlay_predicate = complete;
            self.merge = Some(OverlayMerge {
                overlay,
                pos: 0,
                pending: None,
                base_done: false,
            });
        }
    }

    /// Resolves one planned candidate's bound values against this
    /// execution (a parameter, or the current outer row of a correlated
    /// join). `None` means a NULL bound: the candidate's conjunct can never
    /// be true, so the whole access is empty.
    fn resolve_candidate<'p>(
        access: &'p PhysicalAccess,
        outer: &RowContext,
        ec: &ExecCtx<'a>,
    ) -> Result<Option<Cand<'p>>> {
        Ok(match access {
            PhysicalAccess::IndexScan {
                index_id,
                mode,
                residual,
                fallback,
                ..
            } => {
                let spec = match mode {
                    IndexAccessMode::Equality { prefix } => resolve_values(prefix, outer, ec)?
                        .map(|v| IndexScanSpec::Equality(v.into_iter().map(Some).collect())),
                    IndexAccessMode::Range { start, end } => {
                        match (
                            resolve_bound(start, outer, ec)?,
                            resolve_bound(end, outer, ec)?,
                        ) {
                            (Some(start), Some(end)) => Some(IndexScanSpec::Range { start, end }),
                            _ => None,
                        }
                    }
                };
                spec.map(|spec| Cand::Index {
                    index_id: *index_id,
                    spec,
                    residual,
                    fallback,
                })
            }
            PhysicalAccess::PkRangeScan {
                start,
                end,
                residual,
                fallback,
                ..
            } => match (
                resolve_pk_bound(start, outer, ec)?,
                resolve_pk_bound(end, outer, ec)?,
            ) {
                (Some(start), Some(end)) => Some(Cand::Pk {
                    start,
                    end,
                    residual,
                    fallback,
                }),
                _ => None,
            },
            _ => None,
        })
    }

    /// `IndexScan` / `PkRangeScan`: pick the cheapest candidate among the
    /// planned access and its alternatives, then execute it.
    fn build_ranged(
        access: &PhysicalAccess,
        table_ref: u32,
        outer: &RowContext,
        ec: &ExecCtx<'a>,
    ) -> Result<Self> {
        let table_id = access.table_id();
        let (fallback, alternatives) = match access {
            PhysicalAccess::IndexScan {
                fallback,
                alternatives,
                ..
            }
            | PhysicalAccess::PkRangeScan {
                fallback,
                alternatives,
                ..
            } => (fallback, alternatives),
            _ => unreachable!("build_ranged is only called for IndexScan/PkRangeScan"),
        };
        match access {
            PhysicalAccess::IndexScan { .. } => ec.metrics.record_index_scan(),
            _ => ec.metrics.record_pk_range_scan(),
        }

        // Candidates: the planned access first, then its alternatives --
        // unless an eliminated `Sort` relies on the planned access's order,
        // in which case nothing may replace it.
        let ordered = !fallback.order_ordinals.is_empty();
        let mut cands: Vec<Cand> = Vec::with_capacity(1 + alternatives.len());
        let planned = match Self::resolve_candidate(access, outer, ec)? {
            Some(c) => c,
            // A NULL bound can never match (item 62).
            None => return Ok(Self::empty(table_ref, outer)),
        };
        cands.push(planned);
        if !ordered {
            for alt in alternatives {
                match Self::resolve_candidate(alt, outer, ec)? {
                    Some(c) => cands.push(c),
                    // The conjunction contains a NULL-bounded conjunct: no
                    // row can satisfy it, whichever candidate would run.
                    None => return Ok(Self::empty(table_ref, outer)),
                }
            }
        }
        // Cheap candidates are priced first: index probes are bounded by the
        // remaining budget and make the later PK-range count cheap. Stable,
        // so equal-cost ties keep the planner's structural order.
        let order: Vec<usize> = {
            let mut idx: Vec<usize> = (0..cands.len()).collect();
            idx.sort_by_key(|&i| matches!(cands[i], Cand::Pk { .. }) as u8);
            idx
        };

        let chosen = Self::choose(ec, table_id, &cands, &order, ordered)?;
        let as_of = ec.txn.snapshot_seq();
        match chosen {
            Chosen::Seq(reason) => {
                Self::build_fallback(table_id, table_ref, fallback, outer, ec, reason)
            }
            Chosen::Pk { cand } => {
                if cand != 0 {
                    ec.metrics.record_access_path_switch();
                }
                let Cand::Pk {
                    start,
                    end,
                    residual,
                    fallback,
                } = &cands[cand]
                else {
                    unreachable!("Chosen::Pk always indexes a Pk candidate")
                };
                let iter = ec.table_store.scan_table_pk_range_rows_as_of(
                    table_id,
                    start.clone(),
                    end.clone(),
                    as_of,
                )?;
                ec.metrics.record_table_fetch();
                let mut op = Self::new(
                    table_ref,
                    (*residual).clone(),
                    AccessSource::Lazy(Box::new(iter)),
                    outer,
                );
                op.attach_overlay(table_id, fallback.predicate.clone(), ec);
                op.obs = Some(ScanObs::seq(table_id, false));
                Ok(op)
            }
            Chosen::Index {
                cand,
                entries,
                probe_ns,
            } => {
                if cand != 0 {
                    ec.metrics.record_access_path_switch();
                }
                let Cand::Index {
                    residual, fallback, ..
                } = &cands[cand]
                else {
                    unreachable!("Chosen::Index always indexes an Index candidate")
                };
                Self::build_index_fetch(
                    table_id, table_ref, residual, fallback, entries, probe_ns, outer, ec,
                )
            }
        }
    }

    /// Execute a chosen index candidate: hide base rows the transaction has
    /// rewritten, fetch lazily, and add the transaction's own rows.
    #[allow(clippy::too_many_arguments)]
    fn build_index_fetch(
        table_id: u32,
        table_ref: u32,
        residual: &Option<BoundExpr>,
        fallback: &IndexFallback,
        mut entries: IndexEntries,
        probe_ns: u64,
        outer: &RowContext,
        ec: &ExecCtx<'a>,
    ) -> Result<Self> {
        let as_of = ec.txn.snapshot_seq();
        let overlay = ec.txn.overlay_for(table_id);
        if let Some(ov) = &overlay {
            // The transaction's version of these rows (if any) replaces the
            // base snapshot's: drop the base entries now, before any fetch.
            entries.retain_pk_bytes(|pk| !ov.contains(pk));
        }
        let n_entries = entries.len();
        ec.metrics.record_index_rows_examined(n_entries as u64);
        ec.metrics.record_table_fetch();

        let ordered = !fallback.order_ordinals.is_empty();
        let has_local_puts = overlay
            .as_ref()
            .is_some_and(|ov| ov.entries().iter().any(|e| e.row.is_some()));
        if ordered && has_local_puts {
            // An eliminated Sort relies on index order, and the
            // transaction's own rows must be merged into it: materialize
            // (bounded by max_index_scan_rows) and sort exactly as the
            // index would have ordered them.
            let mut rows: Vec<(Vec<RelationalValue>, Row)> = Vec::new();
            for item in ec.index_builder.lazy_row_fetcher(entries, as_of) {
                ec.check()?;
                rows.push(item?);
            }
            if let Some(ov) = &overlay {
                for e in ov.entries() {
                    if let Some(row) = &e.row {
                        if Self::passes(&fallback.predicate, outer, table_ref, row, ec)? {
                            rows.push((e.pk_values.clone(), row.clone()));
                        }
                    }
                }
            }
            let ords = &fallback.order_ordinals;
            rows.sort_by(|a, b| compare_rows_by_ordinals(ords, a, b));
            return Ok(Self::new(
                table_ref,
                None,
                AccessSource::Vec(rows.into_iter()),
                outer,
            ));
        }

        let extras: Vec<Row> = overlay
            .as_ref()
            .map(|ov| ov.entries().iter().filter_map(|e| e.row.clone()).collect())
            .unwrap_or_default();
        let fetcher = ec.index_builder.lazy_row_fetcher(entries, as_of);
        let mut op = Self::new(
            table_ref,
            residual.clone(),
            AccessSource::IndexFetch {
                fetcher,
                extras: extras.into_iter(),
            },
            outer,
        );
        op.overlay_predicate = fallback.predicate.clone();
        op.obs = Some(ScanObs::index(table_id, probe_ns, n_entries as u64));
        Ok(op)
    }

    /// Evaluates a (possibly absent) predicate against one row in the
    /// context of `outer`.
    fn passes(
        predicate: &Option<BoundExpr>,
        outer: &RowContext,
        table_ref: u32,
        row: &Row,
        ec: &ExecCtx<'a>,
    ) -> Result<bool> {
        match predicate {
            None => Ok(true),
            Some(p) => {
                let ctx = outer.merged(&RowContext::single(table_ref, row.clone()));
                Ok(eval_predicate(p, &ctx, ec)?.is_true())
            }
        }
    }

    /// The cost-based decision among the candidates: exact counts, bounded
    /// probes, conservative under uncertainty (see `exec/cost.rs`).
    fn choose(
        ec: &ExecCtx<'a>,
        table_id: u32,
        cands: &[Cand],
        order: &[usize],
        ordered: bool,
    ) -> Result<Chosen> {
        let as_of = ec.txn.snapshot_seq();
        let mode = ec.limits.access_path;
        let max_rows = ec.limits.max_index_scan_rows;

        match mode {
            AccessPathMode::ForceSeq => return Ok(Chosen::Seq(FallbackReason::Cost)),
            AccessPathMode::ForcePkRange => {
                if let Some(i) = cands.iter().position(|c| matches!(c, Cand::Pk { .. })) {
                    return Ok(Chosen::Pk { cand: i });
                }
            }
            _ => {}
        }
        // Forced or non-abandonable: a single named index candidate is
        // probed without a cost bound (F-2 validity still applies).
        let forced_index = mode == AccessPathMode::ForceIndex || ordered;
        if forced_index || cands.len() == 1 && matches!(cands[0], Cand::Pk { .. }) {
            let i = cands
                .iter()
                .position(|c| matches!(c, Cand::Index { .. }))
                .unwrap_or(0);
            return match &cands[i] {
                Cand::Pk { .. } => Ok(Chosen::Pk { cand: i }),
                Cand::Index { index_id, spec, .. } => {
                    let started = std::time::Instant::now();
                    match ec.index_builder.probe_index_entries_as_of(
                        *index_id,
                        spec,
                        as_of,
                        usize::MAX,
                        max_rows,
                    )? {
                        IndexProbe::Entries(entries) => Ok(Chosen::Index {
                            cand: i,
                            entries,
                            probe_ns: started.elapsed().as_nanos() as u64,
                        }),
                        IndexProbe::Unusable => {
                            // F-2: never use an index that was not Ready at
                            // the snapshot -- a PK-range candidate (always
                            // valid) is preferred over a full scan.
                            Ok(
                                match cands.iter().position(|c| matches!(c, Cand::Pk { .. })) {
                                    Some(p) => Chosen::Pk { cand: p },
                                    None => Chosen::Seq(FallbackReason::Snapshot),
                                },
                            )
                        }
                        IndexProbe::Truncated => unreachable!("entry_limit is usize::MAX"),
                    }
                }
            };
        }

        let stats = ec.table_store.runtime_stats();
        let mut counted = false;
        loop {
            let est = stats.row_estimate(table_id);
            let p = stats.cost_params();
            // The table scan is the baseline every candidate must beat.
            let seq_budget: Option<u128> =
                est.map(|e| (e.rows as u128 + e.drift as u128) * p.seq_ns_per_row as u128);
            let mut best: Option<(usize, u128, Option<(IndexEntries, u64)>)> = None;
            let mut truncated_any = false;
            let mut unusable = 0usize;
            let mut indexes = 0usize;
            for &i in order {
                let budget = match (best.as_ref().map(|b| b.1), seq_budget) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (Some(a), None) => Some(a),
                    (None, b) => b,
                };
                match &cands[i] {
                    Cand::Index { index_id, spec, .. } => {
                        indexes += 1;
                        let limit = match budget {
                            // At least one entry: a single match always
                            // goes to the index (cost::break_even's rule).
                            Some(b) => ((b / p.index_ns_per_row.max(1) as u128)
                                .min(usize::MAX as u128)
                                as usize)
                                .max(1),
                            None => cost::PROBE_FLOOR,
                        };
                        let started = std::time::Instant::now();
                        match ec
                            .index_builder
                            .probe_index_entries_as_of(*index_id, spec, as_of, limit, max_rows)?
                        {
                            IndexProbe::Entries(entries) => {
                                let c = entries.len() as u128 * p.index_ns_per_row as u128;
                                if best.as_ref().is_none_or(|b| c < b.1) {
                                    let ns = started.elapsed().as_nanos() as u64;
                                    best = Some((i, c, Some((entries, ns))));
                                }
                            }
                            IndexProbe::Truncated => truncated_any = true,
                            IndexProbe::Unusable => unusable += 1,
                        }
                    }
                    Cand::Pk { start, end, .. } => {
                        let limit = match budget {
                            Some(b) => ((b / p.seq_ns_per_row.max(1) as u128).min(u64::MAX as u128)
                                as u64)
                                .max(1),
                            None => cost::PROBE_FLOOR as u64,
                        };
                        let (r, truncated) = ec.table_store.count_pk_range_rows_as_of(
                            table_id,
                            start.clone(),
                            end.clone(),
                            as_of,
                            limit,
                        )?;
                        if truncated {
                            truncated_any = true;
                        } else {
                            let c = r as u128 * p.seq_ns_per_row as u128;
                            if best.as_ref().is_none_or(|b| c < b.1) {
                                best = Some((i, c, None));
                            }
                        }
                    }
                }
            }
            if let Some((i, _, payload)) = best {
                return Ok(match payload {
                    Some((entries, probe_ns)) => Chosen::Index {
                        cand: i,
                        entries,
                        probe_ns,
                    },
                    None => Chosen::Pk { cand: i },
                });
            }
            // Nothing priced within budget. If the table size is unknown or
            // stale, count it once (cached) and decide again; otherwise the
            // table scan is cheaper than every candidate.
            if truncated_any && !counted && cost::estimate_is_unreliable(est) {
                ec.table_store.count_rows(table_id)?;
                counted = true;
                continue;
            }
            let all_unusable = indexes > 0
                && unusable == indexes
                && cands.iter().all(|c| matches!(c, Cand::Index { .. }));
            return Ok(Chosen::Seq(if all_unusable {
                FallbackReason::Snapshot
            } else {
                FallbackReason::Cost
            }));
        }
    }

    /// The table scan an access falls back to (see `IndexFallback`): the
    /// table's whole predicate over a snapshot-consistent scan merged with
    /// the transaction's own writes, with the index's ordering
    /// re-established when an eliminated `Sort` relied on it.
    /// Result-equivalent to every index path by construction -- the same
    /// predicate over the same snapshot of the table.
    fn build_fallback(
        table_id: u32,
        table_ref: u32,
        fallback: &IndexFallback,
        outer: &RowContext,
        ec: &ExecCtx<'a>,
        reason: FallbackReason,
    ) -> Result<Self> {
        ec.metrics.record_seq_scan();
        match reason {
            FallbackReason::Snapshot => ec.metrics.record_index_snapshot_fallback(),
            FallbackReason::Cost => ec.metrics.record_index_cost_fallback(),
        }
        if fallback.order_ordinals.is_empty() {
            return Self::lazy_table_scan(
                table_id,
                table_ref,
                fallback.predicate.clone(),
                outer,
                ec,
            );
        }
        // Ordered: collect the matching rows, then sort by the index's own
        // column order. Bounded like every other materializing operator.
        let as_of = ec.txn.snapshot_seq();
        let overlay = ec.txn.overlay_for(table_id);
        let mut rows: Vec<(Vec<RelationalValue>, Row)> = Vec::new();
        let mut scanned: u64 = 0;
        for item in ec.table_store.scan_table_rows_as_of(table_id, as_of)? {
            ec.check()?;
            let (pk, row) = item?;
            scanned += 1;
            ec.metrics.record_rows_scanned(1);
            if let Some(ov) = &overlay {
                // The transaction's version replaces the base row.
                if ov.contains(&encode_composite_key(&pk)?) {
                    continue;
                }
            }
            if !Self::passes(&fallback.predicate, outer, table_ref, &row, ec)? {
                ec.metrics.record_rows_filtered(1);
                continue;
            }
            Self::push_bounded(&mut rows, pk, row, ec)?;
        }
        if let Some(ov) = &overlay {
            for e in ov.entries() {
                if let Some(row) = &e.row {
                    if Self::passes(&fallback.predicate, outer, table_ref, row, ec)? {
                        Self::push_bounded(&mut rows, e.pk_values.clone(), row.clone(), ec)?;
                    }
                }
            }
        } else {
            ec.table_store
                .runtime_stats()
                .observe_row_count(table_id, scanned);
        }
        let ords = &fallback.order_ordinals;
        rows.sort_by(|a, b| compare_rows_by_ordinals(ords, a, b));
        Ok(Self::new(
            table_ref,
            None,
            AccessSource::Vec(rows.into_iter()),
            outer,
        ))
    }

    fn push_bounded(
        rows: &mut Vec<(Vec<RelationalValue>, Row)>,
        pk: Vec<RelationalValue>,
        row: Row,
        ec: &ExecCtx<'a>,
    ) -> Result<()> {
        if rows.len() >= ec.limits.max_materialized_rows {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "ordered index-scan fallback exceeds max_materialized_rows ({})",
                    ec.limits.max_materialized_rows
                ),
            });
        }
        rows.push((pk, row));
        Ok(())
    }

    /// Next row and whether it came from the transaction's own overlay.
    fn fetch_next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<(Row, bool)>> {
        let AccessOp { source, merge, .. } = self;
        match source {
            AccessSource::Single(row) => Ok(row.take().map(|r| (r, false))),
            AccessSource::Vec(iter) => Ok(iter.next().map(|(_, r)| (r, false))),
            AccessSource::IndexFetch { fetcher, extras } => match fetcher.next().transpose()? {
                Some((_, row)) => Ok(Some((row, false))),
                None => Ok(extras.next().map(|r| (r, true))),
            },
            AccessSource::Lazy(iter) => match merge {
                None => {
                    let next = iter.next().transpose()?;
                    if next.is_some() {
                        ec.metrics.record_rows_scanned(1);
                    }
                    Ok(next.map(|(_, row)| (row, false)))
                }
                Some(m) => loop {
                    if m.pending.is_none() && !m.base_done {
                        match iter.next().transpose()? {
                            Some((pk, row)) => {
                                ec.metrics.record_rows_scanned(1);
                                let enc = encode_composite_key(&pk)?;
                                m.pending = Some((enc, row));
                            }
                            None => m.base_done = true,
                        }
                    }
                    let local = m.overlay.entries().get(m.pos);
                    match (&m.pending, local) {
                        (None, None) => return Ok(None),
                        (Some(_), None) => {
                            let (_, row) = m.pending.take().expect("checked Some");
                            return Ok(Some((row, false)));
                        }
                        (None, Some(e)) => {
                            m.pos += 1;
                            if let Some(row) = &e.row {
                                return Ok(Some((row.clone(), true)));
                            }
                        }
                        (Some((benc, _)), Some(e)) => {
                            match benc.as_slice().cmp(e.encoded_pk.as_slice()) {
                                std::cmp::Ordering::Less => {
                                    let (_, row) = m.pending.take().expect("checked Some");
                                    return Ok(Some((row, false)));
                                }
                                std::cmp::Ordering::Equal => {
                                    // The transaction rewrote or deleted
                                    // this base row.
                                    m.pending = None;
                                    m.pos += 1;
                                    if let Some(row) = &e.row {
                                        return Ok(Some((row.clone(), true)));
                                    }
                                }
                                std::cmp::Ordering::Greater => {
                                    m.pos += 1;
                                    if let Some(row) = &e.row {
                                        return Ok(Some((row.clone(), true)));
                                    }
                                }
                            }
                        }
                    }
                },
            },
        }
    }
}

impl<'a> Operator<'a> for AccessOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        loop {
            ec.check()?;
            let sample = self.obs.as_mut().and_then(|o| o.start());
            let Some((row, from_overlay)) = self.fetch_next(ec)? else {
                if let Some(o) = &self.obs {
                    o.finish(ec);
                }
                return Ok(None);
            };
            if let Some(o) = self.obs.as_mut() {
                o.rows += 1;
            }
            let row_ctx = self.outer.merged(&RowContext::single(self.table_ref, row));
            let predicate = if from_overlay {
                &self.overlay_predicate
            } else {
                &self.residual
            };
            let keep = match predicate {
                None => true,
                Some(p) => eval_predicate(p, &row_ctx, ec)?.is_true(),
            };
            if let Some(o) = self.obs.as_mut() {
                o.stop(sample);
            }
            if keep {
                return Ok(Some(Tuple {
                    ctx: row_ctx,
                    projected: Vec::new(),
                }));
            }
            ec.metrics.record_rows_filtered(1);
        }
    }
}
