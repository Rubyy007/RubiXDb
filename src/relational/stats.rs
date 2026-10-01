//! Increment 17: bounded, in-memory **runtime statistics** for cost-based
//! access-path selection (`PHASE_RUBIXDB_INCREMENT17_COST_MODEL_
//! ARCHITECTURE.md`).
//!
//! Design constraints, each enforced here rather than merely intended:
//!
//! - **Statistics may affect performance only, never results.** Nothing in
//!   this module is consulted by any read or write to decide *what* a query
//!   returns -- only the SQL executor's choice between two result-equivalent
//!   access paths. A wrong, stale, or absent statistic can therefore only
//!   cost time.
//! - **No persisted state, no write-path cost beyond one relaxed atomic
//!   add** (and none at all for a table with no estimate): nothing here
//!   touches the WAL, Manifest, SSTables, or catalog, so there is nothing to
//!   recover, migrate, or corrupt.
//! - **Bounded:** at most `MAX_TRACKED_TABLES` per-table records (two
//!   atomics each) and one fixed-size cost record per process. No per-index,
//!   per-value, or per-query state exists.
//! - **Self-describing staleness:** every row-count estimate carries a
//!   rigorous `drift` bound -- the number of row mutations applied through
//!   this process since the estimate was taken -- so the model never has to
//!   guess how old an estimate is.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Hard bound on tracked tables (each record is two atomics); a table
/// beyond the bound simply has no estimate and is planned exactly as
/// before this increment.
pub const MAX_TRACKED_TABLES: usize = 4096;

/// Measured defaults (`PHASE_RUBIXDB_INCREMENT17_PERFORMANCE.md`): the cost
/// of producing one row through a sequential scan (storage decode plus
/// predicate evaluation) and through the index path (entry enumeration,
/// point read, decode). Starting points only -- observed costs replace them
/// (clamped to `[default / COST_CLAMP, default * COST_CLAMP]`).
pub const DEFAULT_SEQ_NS_PER_ROW: u64 = 3_400;
pub const DEFAULT_INDEX_NS_PER_ROW: u64 = 14_500;
pub const COST_CLAMP: u64 = 8;

/// A row-count estimate and how far it may have drifted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowEstimate {
    /// The count observed at the last observation.
    pub rows: u64,
    /// Row mutations applied through this process since: an upper bound on
    /// `|true_count - rows|` (one mutation changes the count by at most
    /// one).
    pub drift: u64,
}

#[derive(Default)]
struct TableStats {
    rows: AtomicU64,
    drift: AtomicU64,
}

/// The per-process cost parameters, in nanoseconds per produced row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostParams {
    pub seq_ns_per_row: u64,
    pub index_ns_per_row: u64,
}

pub struct RuntimeStats {
    tables: RwLock<HashMap<u32, Arc<TableStats>>>,
    seq_ns: AtomicU64,
    index_ns: AtomicU64,
}

impl Default for RuntimeStats {
    fn default() -> Self {
        RuntimeStats {
            tables: RwLock::new(HashMap::new()),
            seq_ns: AtomicU64::new(DEFAULT_SEQ_NS_PER_ROW),
            index_ns: AtomicU64::new(DEFAULT_INDEX_NS_PER_ROW),
        }
    }
}

impl RuntimeStats {
    /// Records an exact (or freshly observed) row count and resets the
    /// drift bound. Called when a scan visits an entire table, when an
    /// index backfill enumerates one, and after an explicit count.
    pub fn observe_row_count(&self, table_id: u32, rows: u64) {
        {
            let tables = self.tables.read().unwrap_or_else(|p| p.into_inner());
            if let Some(t) = tables.get(&table_id) {
                t.rows.store(rows, Ordering::Relaxed);
                t.drift.store(0, Ordering::Relaxed);
                return;
            }
        }
        let mut tables = self.tables.write().unwrap_or_else(|p| p.into_inner());
        if tables.len() >= MAX_TRACKED_TABLES && !tables.contains_key(&table_id) {
            return; // bounded: an untracked table is planned as before
        }
        let t = tables.entry(table_id).or_default();
        t.rows.store(rows, Ordering::Relaxed);
        t.drift.store(0, Ordering::Relaxed);
    }

    /// The current estimate for `table_id`, if one has been observed.
    pub fn row_estimate(&self, table_id: u32) -> Option<RowEstimate> {
        let tables = self.tables.read().unwrap_or_else(|p| p.into_inner());
        tables.get(&table_id).map(|t| RowEstimate {
            rows: t.rows.load(Ordering::Relaxed),
            drift: t.drift.load(Ordering::Relaxed),
        })
    }

    /// Notes `n` row mutations against `table_id`. A table with no estimate
    /// is not tracked, so for it this is a read-lock and a hash probe.
    #[inline]
    pub fn note_mutations(&self, table_id: u32, n: u64) {
        let tables = self.tables.read().unwrap_or_else(|p| p.into_inner());
        if let Some(t) = tables.get(&table_id) {
            t.drift.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub fn cost_params(&self) -> CostParams {
        CostParams {
            seq_ns_per_row: self.seq_ns.load(Ordering::Relaxed),
            index_ns_per_row: self.index_ns.load(Ordering::Relaxed),
        }
    }

    /// Folds one observed sequential-scan cost into the estimate: an
    /// exponentially weighted average (weight 1/8) clamped to
    /// `[default / COST_CLAMP, default * COST_CLAMP]`, so a single outlier
    /// cannot move the model far and a pathological observation cannot
    /// drive it out of range.
    pub fn observe_seq_cost(&self, rows: u64, total_ns: u64) {
        Self::fold(&self.seq_ns, DEFAULT_SEQ_NS_PER_ROW, rows, total_ns);
    }

    /// As `observe_seq_cost`, for the index path.
    pub fn observe_index_cost(&self, rows: u64, total_ns: u64) {
        Self::fold(&self.index_ns, DEFAULT_INDEX_NS_PER_ROW, rows, total_ns);
    }

    fn fold(cell: &AtomicU64, default: u64, rows: u64, total_ns: u64) {
        // Too few rows to amortize fixed per-scan costs: not a per-row signal.
        if rows < MIN_COST_SAMPLE_ROWS {
            return;
        }
        let observed = (total_ns / rows).clamp(default / COST_CLAMP, default * COST_CLAMP);
        let old = cell.load(Ordering::Relaxed);
        let new = (old * 7 + observed) / 8;
        cell.store(
            new.clamp(default / COST_CLAMP, default * COST_CLAMP),
            Ordering::Relaxed,
        );
    }

    /// Number of tracked tables (diagnostics / bound tests).
    pub fn tracked_tables(&self) -> usize {
        self.tables.read().unwrap_or_else(|p| p.into_inner()).len()
    }
}

/// Minimum rows behind a cost observation.
pub const MIN_COST_SAMPLE_ROWS: u64 = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_drift_and_reset() {
        let s = RuntimeStats::default();
        assert_eq!(s.row_estimate(1), None);
        s.note_mutations(1, 5); // untracked: no effect, no allocation
        assert_eq!(s.tracked_tables(), 0);
        s.observe_row_count(1, 100);
        s.note_mutations(1, 7);
        s.note_mutations(1, 3);
        assert_eq!(
            s.row_estimate(1),
            Some(RowEstimate {
                rows: 100,
                drift: 10
            })
        );
        s.observe_row_count(1, 90);
        assert_eq!(s.row_estimate(1), Some(RowEstimate { rows: 90, drift: 0 }));
    }

    #[test]
    fn tracked_tables_are_bounded() {
        let s = RuntimeStats::default();
        for t in 0..(MAX_TRACKED_TABLES as u32 + 100) {
            s.observe_row_count(t, 1);
        }
        assert_eq!(s.tracked_tables(), MAX_TRACKED_TABLES);
        assert_eq!(s.row_estimate(MAX_TRACKED_TABLES as u32 + 50), None);
    }

    #[test]
    fn cost_estimates_stay_clamped_and_ignore_tiny_samples() {
        let s = RuntimeStats::default();
        s.observe_seq_cost(10, 1); // too few rows: ignored
        assert_eq!(s.cost_params().seq_ns_per_row, DEFAULT_SEQ_NS_PER_ROW);
        for _ in 0..200 {
            s.observe_seq_cost(1_000, 1); // absurdly cheap
            s.observe_index_cost(1_000, u64::MAX / 4); // absurdly dear
        }
        let p = s.cost_params();
        // Integer EWMA converges to within rounding of the clamp bounds and
        // never beyond them.
        let (lo, hi) = (
            DEFAULT_SEQ_NS_PER_ROW / COST_CLAMP,
            DEFAULT_INDEX_NS_PER_ROW * COST_CLAMP,
        );
        assert!(p.seq_ns_per_row >= lo && p.seq_ns_per_row <= lo + lo / 100);
        assert!(p.index_ns_per_row <= hi && p.index_ns_per_row >= hi - hi / 100);
    }
}

#[cfg(test)]
mod hook_bench {
    use super::*;
    use std::time::Instant;

    /// Direct cost of the write-path hook (`note_mutations`): a tracked
    /// table (read lock + hash probe + one relaxed atomic add) and an
    /// untracked one (read lock + hash probe), single-threaded and with 8
    /// contending threads. End-to-end write benchmarks cannot resolve this
    /// -- a durable write costs milliseconds of fsync.
    ///   cargo test --release -p rubixdb --lib hook_bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn note_mutations_cost() {
        let s = Arc::new(RuntimeStats::default());
        s.observe_row_count(1, 1_000);
        for (label, table, threads) in [
            ("tracked table,   1 thread ", 1u32, 1usize),
            ("untracked table, 1 thread ", 2u32, 1),
            ("tracked table,   8 threads", 1u32, 8),
            ("untracked table, 8 threads", 2u32, 8),
        ] {
            let per = 5_000_000u64;
            let t = Instant::now();
            let hs: Vec<_> = (0..threads)
                .map(|_| {
                    let s = Arc::clone(&s);
                    std::thread::spawn(move || {
                        for _ in 0..per {
                            s.note_mutations(table, 1);
                        }
                    })
                })
                .collect();
            for h in hs {
                h.join().unwrap();
            }
            let ns = t.elapsed().as_nanos() as f64 / (per * threads as u64) as f64 * threads as f64;
            println!(
                "{label}: {:>6.1} ns per call per thread ({} calls)",
                ns,
                per * threads as u64
            );
        }
    }
}
