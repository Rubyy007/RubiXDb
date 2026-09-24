//! Bounded-cardinality write metrics — item 55/106/107. Same discipline
//! as `crate::exec::ExecMetrics`/`crate::plan::PlannerMetrics`: counters
//! only, every recorder method takes only already-classified,
//! small-enum-shaped or numeric arguments, so there is no way to pass
//! SQL text, a table/schema/index name, or a principal into a label —
//! structurally, not by convention (item 107).

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct WriteMetrics {
    insert_statements: AtomicU64,
    update_statements: AtomicU64,
    delete_statements: AtomicU64,
    ddl_statements: AtomicU64,
    rows_inserted: AtomicU64,
    rows_updated: AtomicU64,
    rows_deleted: AtomicU64,
    write_conflicts: AtomicU64,
    dml_errors: AtomicU64,
    ddl_errors: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteMetricsSnapshot {
    pub insert_statements: u64,
    pub update_statements: u64,
    pub delete_statements: u64,
    pub ddl_statements: u64,
    pub rows_inserted: u64,
    pub rows_updated: u64,
    pub rows_deleted: u64,
    /// item 106: `rows_affected` is not tracked as its own counter --
    /// it is always exactly `rows_inserted + rows_updated + rows_
    /// deleted` for DML, computed at read time rather than risking a
    /// second, independently-incremented counter drifting from the
    /// other three.
    pub write_conflicts: u64,
    pub dml_errors: u64,
    pub ddl_errors: u64,
}

impl WriteMetricsSnapshot {
    pub fn rows_affected(&self) -> u64 {
        self.rows_inserted + self.rows_updated + self.rows_deleted
    }
}

impl WriteMetrics {
    pub fn record_insert_statement(&self) {
        self.insert_statements.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_update_statement(&self) {
        self.update_statements.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_delete_statement(&self) {
        self.delete_statements.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_ddl_statement(&self) {
        self.ddl_statements.fetch_add(1, Ordering::Relaxed);
    }
    /// item 119: only ever called once a row's mutation has actually
    /// been buffered into the transaction's own write-set by `Transaction
    /// ::put_row` returning `Ok` — never before, and never for a
    /// statement whose transaction ultimately aborts (this crate has no
    /// way to know that at buffer-time; see `PHASE_RELATIONAL_WRITE_
    /// EXECUTOR_ARCHITECTURE.md` §9 for the full "counted at buffer-
    /// time, not at commit-time" accounting decision and its honest
    /// limitation).
    pub fn record_rows_inserted(&self, n: u64) {
        self.rows_inserted.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_rows_updated(&self, n: u64) {
        self.rows_updated.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_rows_deleted(&self, n: u64) {
        self.rows_deleted.fetch_add(n, Ordering::Relaxed);
    }
    pub fn record_write_conflict(&self) {
        self.write_conflicts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_dml_error(&self) {
        self.dml_errors.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_ddl_error(&self) {
        self.ddl_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> WriteMetricsSnapshot {
        WriteMetricsSnapshot {
            insert_statements: self.insert_statements.load(Ordering::Relaxed),
            update_statements: self.update_statements.load(Ordering::Relaxed),
            delete_statements: self.delete_statements.load(Ordering::Relaxed),
            ddl_statements: self.ddl_statements.load(Ordering::Relaxed),
            rows_inserted: self.rows_inserted.load(Ordering::Relaxed),
            rows_updated: self.rows_updated.load(Ordering::Relaxed),
            rows_deleted: self.rows_deleted.load(Ordering::Relaxed),
            write_conflicts: self.write_conflicts.load(Ordering::Relaxed),
            dml_errors: self.dml_errors.load(Ordering::Relaxed),
            ddl_errors: self.ddl_errors.load(Ordering::Relaxed),
        }
    }
}
