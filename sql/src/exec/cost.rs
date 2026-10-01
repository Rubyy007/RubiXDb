//! Increment 17: the cost model behind index-versus-table-scan selection
//! (`PHASE_RUBIXDB_INCREMENT17_COST_MODEL_ARCHITECTURE.md`).
//!
//! # What is modelled
//!
//! Two candidate ways of producing the rows of one table access:
//!
//! ```text
//!   cost_seq   = N * seq_ns_per_row                 (decode + predicate, every row)
//!   cost_index = K * index_ns_per_row               (enumerate entry + point read + decode)
//! ```
//!
//! `K` is the **exact** number of matching index entries (the executor
//! enumerates entries -- ~0.6us each, key-only -- before deciding), `N` the
//! table's row count (an estimate with a rigorous drift bound, see
//! `rubixdb::relational::stats`), and the two per-row costs are measured
//! defaults refined by exponentially-weighted observation of the executor's
//! own scans. The break-even match count is therefore
//!
//! ```text
//!   K* = N_hi * seq_ns_per_row / index_ns_per_row
//! ```
//!
//! and the index is abandoned for a table scan only when `K > K*`. There is
//! **no hard-coded selectivity percentage**: the crossover moves with the
//! observed cost ratio (it differs between a MemTable-resident and an
//! SSTable-resident table, which a constant could not capture).
//!
//! # Conservative by construction
//!
//! The two mistakes are not symmetric. Choosing the index when a scan is
//! cheaper costs a bounded factor (the per-row cost ratio, ~4x). Choosing a
//! scan when the index is cheaper costs `N / K` -- unbounded. Every
//! uncertainty is therefore resolved toward the index:
//!
//! - `N_hi = rows + drift` (the largest the table can be given the
//!   mutations since the estimate), which makes a scan look *more*
//!   expensive;
//! - with no estimate at all the index is used unless the match count
//!   reaches `PROBE_FLOOR`, and only then is the table counted (once; the
//!   count is cached) -- so a point-ish lookup never pays for statistics;
//! - an ordered scan (an eliminated `Sort` relies on it) never abandons the
//!   index, since its free ordering is part of its value.
//!
//! Statistics can change *which* of two result-equivalent paths runs, never
//! the result.

use rubixdb::relational::stats::{CostParams, RowEstimate};

/// Match count at which a table with no known size is counted so the
/// decision can be made. Below it the index path costs at most
/// `PROBE_FLOOR * index_ns_per_row` (~2ms) in the worst case, which bounds
/// the loss of ignoring statistics for small results.
pub const PROBE_FLOOR: usize = 128;

/// How the executor chooses between an `IndexScan` and a table scan.
/// `Auto` is the cost model; the two `Force*` modes exist for benchmarking
/// and diagnostics (they select between result-equivalent paths only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessPathMode {
    #[default]
    Auto,
    ForceIndex,
    /// Increment 18: run the PK-range candidate if the access has one
    /// (benchmarking; otherwise behaves as `Auto`).
    ForcePkRange,
    ForceSeq,
}

/// `K*`: the largest match count for which the index path is still
/// estimated no more expensive than a full table scan. Saturating, never
/// zero (a result of one row always goes to the index).
pub fn break_even_entries(est: RowEstimate, p: CostParams) -> usize {
    let n_hi = est.rows.saturating_add(est.drift) as u128;
    let k = n_hi * p.seq_ns_per_row as u128 / p.index_ns_per_row.max(1) as u128;
    (k.min(usize::MAX as u128) as usize).max(1)
}

/// The entry limit the executor probes with: abandon the index once more
/// than this many entries match.
pub fn entry_limit(est: Option<RowEstimate>, p: CostParams) -> usize {
    match est {
        None => PROBE_FLOOR,
        Some(e) => break_even_entries(e, p),
    }
}

/// Is the table-size knowledge too weak to abandon the index on? An absent
/// estimate, or one that may have drifted by more than a quarter of its
/// size, is refreshed by an exact count before a scan is chosen. (The count
/// resets the drift, so a table is recounted at most once per quarter-table
/// of mutations -- amortized constant work per write.)
pub fn estimate_is_unreliable(est: Option<RowEstimate>) -> bool {
    match est {
        None => true,
        Some(e) => e.drift > e.rows / 4 + 64,
    }
}

/// The model's pure prediction, used by tests and by the validation
/// harness: which path is predicted cheaper for `k` matches in a table of
/// `n` rows.
pub fn predict_prefers_scan(k: u64, n: u64, p: CostParams) -> bool {
    (k as u128) * (p.index_ns_per_row as u128) > (n as u128) * (p.seq_ns_per_row as u128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rubixdb::relational::stats::{DEFAULT_INDEX_NS_PER_ROW, DEFAULT_SEQ_NS_PER_ROW};

    fn p() -> CostParams {
        CostParams {
            seq_ns_per_row: DEFAULT_SEQ_NS_PER_ROW,
            index_ns_per_row: DEFAULT_INDEX_NS_PER_ROW,
            index_open_ns: rubixdb::relational::stats::DEFAULT_INDEX_OPEN_NS,
        }
    }

    #[test]
    fn no_estimate_probes_to_the_floor_only() {
        assert_eq!(entry_limit(None, p()), PROBE_FLOOR);
        assert!(estimate_is_unreliable(None));
    }

    #[test]
    fn break_even_scales_with_table_size_and_cost_ratio() {
        let e = RowEstimate {
            rows: 100_000,
            drift: 0,
        };
        let k = break_even_entries(e, p());
        // 100,000 * 3,400 / 14,500 ~= 23,448 -- the crossover Increment 16
        // measured (~23%), derived, not hard-coded.
        assert_eq!(k, 23_448);
        // A cheaper index relative to scan (e.g. a memtable-resident table)
        // moves the crossover up, with no code change.
        let fast_index = CostParams {
            seq_ns_per_row: 2_700,
            index_ns_per_row: 3_650,
            index_open_ns: 100_000,
        };
        assert!(break_even_entries(e, fast_index) > 70_000);
    }

    #[test]
    fn drift_widens_the_bound_toward_the_index() {
        let fresh = RowEstimate {
            rows: 1_000,
            drift: 0,
        };
        let drifted = RowEstimate {
            rows: 1_000,
            drift: 1_000,
        };
        assert!(break_even_entries(drifted, p()) > break_even_entries(fresh, p()));
    }

    #[test]
    fn a_tiny_or_empty_estimate_never_yields_zero() {
        let e = RowEstimate { rows: 0, drift: 0 };
        assert_eq!(break_even_entries(e, p()), 1);
        let huge = RowEstimate {
            rows: u64::MAX,
            drift: u64::MAX,
        };
        assert!(break_even_entries(huge, p()) > 0); // saturates, no overflow
    }

    #[test]
    fn reliability_requires_bounded_drift() {
        assert!(!estimate_is_unreliable(Some(RowEstimate {
            rows: 10_000,
            drift: 2_000
        })));
        assert!(estimate_is_unreliable(Some(RowEstimate {
            rows: 10_000,
            drift: 3_000
        })));
        // small tables tolerate a little absolute drift
        assert!(!estimate_is_unreliable(Some(RowEstimate {
            rows: 100,
            drift: 60
        })));
    }

    #[test]
    fn prediction_matches_the_break_even() {
        let n = 100_000;
        let k_star = break_even_entries(RowEstimate { rows: n, drift: 0 }, p()) as u64;
        assert!(!predict_prefers_scan(k_star, n, p()));
        assert!(predict_prefers_scan(k_star + 1, n, p()));
    }
}
