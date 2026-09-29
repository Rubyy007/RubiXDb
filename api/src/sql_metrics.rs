//! Bounded-cardinality SQL-endpoint metrics — item 19/71/87/88. Counters
//! only; every recorder takes an already-classified, small-enum-shaped
//! argument (or a count), structurally incapable of holding raw SQL
//! text, table/column/schema names, parameter values, row values, or a
//! principal name as a label — the same discipline `rubixdb-sql`'s own
//! `ExecMetrics`/`PlannerMetrics`/`SqlMetrics` already apply one layer
//! down, extended here to the one additional dimension they cannot see
//! from inside the SQL crate: HTTP-level outcome classification
//! (cancelled/timed-out vs. every other error).

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct SqlApiMetrics {
    requests: AtomicU64,
    success: AtomicU64,
    errors: AtomicU64,
    cancellations: AtomicU64,
    deadline_exceeded: AtomicU64,
    rows_returned: AtomicU64,
    rows_affected: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct SqlApiMetricsSnapshot {
    pub requests: u64,
    pub success: u64,
    pub errors: u64,
    pub cancellations: u64,
    pub deadline_exceeded: u64,
    pub rows_returned: u64,
    pub rows_affected: u64,
}

impl SqlApiMetrics {
    pub fn record_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_success(&self) {
        self.success.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_cancellation(&self) {
        self.cancellations.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_deadline_exceeded(&self) {
        self.deadline_exceeded.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_rows_returned(&self, n: u64) {
        self.rows_returned.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_rows_affected(&self, n: u64) {
        self.rows_affected.fetch_add(n, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> SqlApiMetricsSnapshot {
        SqlApiMetricsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            success: self.success.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            cancellations: self.cancellations.load(Ordering::Relaxed),
            deadline_exceeded: self.deadline_exceeded.load(Ordering::Relaxed),
            rows_returned: self.rows_returned.load(Ordering::Relaxed),
            rows_affected: self.rows_affected.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_independently() {
        let m = SqlApiMetrics::default();
        m.record_request();
        m.record_request();
        m.record_success();
        m.record_error();
        m.record_cancellation();
        m.record_deadline_exceeded();
        m.record_rows_returned(10);
        m.record_rows_affected(3);
        let snap = m.snapshot();
        assert_eq!(snap.requests, 2);
        assert_eq!(snap.success, 1);
        assert_eq!(snap.errors, 1);
        assert_eq!(snap.cancellations, 1);
        assert_eq!(snap.deadline_exceeded, 1);
        assert_eq!(snap.rows_returned, 10);
        assert_eq!(snap.rows_affected, 3);
    }
}
