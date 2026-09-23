//! Bounded-cardinality SQL parse/bind metrics (item 44). Counters only —
//! no raw SQL text, table/schema name, or parameter value is ever a
//! label or held anywhere in this module (the input types below are
//! incapable of holding any of that, structurally, not by convention:
//! every recorder method takes only already-classified, small-enum-
//! shaped arguments). Not wired into `/v1/metrics` yet (D33's `sql` key
//! is a future increment's job, once an API surface calls this crate at
//! all, item 48) — usable standalone via `SqlMetrics::snapshot`.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct SqlMetrics {
    parsed_statements: AtomicU64,
    parse_errors: AtomicU64,
    bound_statements: AtomicU64,
    bind_errors: AtomicU64,
    unsupported_statements: AtomicU64,
    authorization_denials: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlMetricsSnapshot {
    pub parsed_statements: u64,
    pub parse_errors: u64,
    pub bound_statements: u64,
    pub bind_errors: u64,
    pub unsupported_statements: u64,
    pub authorization_denials: u64,
}

impl SqlMetrics {
    pub fn record_parse_success(&self) {
        self.parsed_statements.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_parse_error(&self) {
        self.parse_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bind_success(&self) {
        self.bound_statements.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bind_error(&self) {
        self.bind_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_unsupported(&self) {
        self.unsupported_statements.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_authorization_denial(&self) {
        self.authorization_denials.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> SqlMetricsSnapshot {
        SqlMetricsSnapshot {
            parsed_statements: self.parsed_statements.load(Ordering::Relaxed),
            parse_errors: self.parse_errors.load(Ordering::Relaxed),
            bound_statements: self.bound_statements.load(Ordering::Relaxed),
            bind_errors: self.bind_errors.load(Ordering::Relaxed),
            unsupported_statements: self.unsupported_statements.load(Ordering::Relaxed),
            authorization_denials: self.authorization_denials.load(Ordering::Relaxed),
        }
    }
}
