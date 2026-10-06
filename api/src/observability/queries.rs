//! Registry of SQL statements: the ones running now and the most recent finished ones.
//!
//! Records carry only closed-set labels, counts and times. **No SQL text, parameter, table or
//! column name, principal name or error message is ever stored**; the statement class and the
//! error class are `&'static str` values chosen by the caller from fixed lists.
//!
//! Bounds: at most [`RECENT_CAP`] finished records (oldest evicted first) and at most
//! [`IN_FLIGHT_CAP`] running records (a statement that cannot be tracked is counted in
//! `untracked`, never queued). Memory is therefore bounded regardless of traffic.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use super::events::{kind, now_unix_ms, severity, Event, EventLog};

pub const RECENT_CAP: usize = 200;
pub const IN_FLIGHT_CAP: usize = 2048;
pub const MAX_QUERIES_RETURNED: usize = 200;

/// Statement classes (closed set).
pub mod class {
    pub const UNPARSED: &str = "unparsed";
    pub const SELECT: &str = "select";
    pub const INSERT: &str = "insert";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
    pub const DDL: &str = "ddl";
    pub const TRANSACTION: &str = "transaction";
    pub const EXPLAIN: &str = "explain";
    pub const OTHER: &str = "other";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

impl QueryState {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryState::Running => "running",
            QueryState::Succeeded => "succeeded",
            QueryState::Failed => "failed",
            QueryState::Cancelled => "cancelled",
            QueryState::TimedOut => "timed_out",
        }
    }
}

#[derive(Debug, Clone)]
struct Rec {
    id: u64,
    class: &'static str,
    state: QueryState,
    start_unix_ms: u64,
    started: Instant,
    duration_ms: Option<f64>,
    rows_affected: Option<u64>,
    rows_returned: Option<u64>,
    error_class: Option<&'static str>,
}

/// What a client sees.
#[derive(Debug, Clone)]
pub struct QueryView {
    pub id: u64,
    pub class: &'static str,
    pub state: &'static str,
    pub start_unix_ms: u64,
    pub duration_ms: f64,
    pub rows_affected: Option<u64>,
    pub rows_returned: Option<u64>,
    pub timeout_state: &'static str,
    pub cancellation_state: &'static str,
    pub error_class: Option<&'static str>,
}

impl Rec {
    fn view(&self) -> QueryView {
        QueryView {
            id: self.id,
            class: self.class,
            state: self.state.as_str(),
            start_unix_ms: self.start_unix_ms,
            duration_ms: self
                .duration_ms
                .unwrap_or_else(|| self.started.elapsed().as_secs_f64() * 1000.0),
            rows_affected: self.rows_affected,
            rows_returned: self.rows_returned,
            timeout_state: if self.state == QueryState::TimedOut {
                "deadline_exceeded"
            } else {
                "none"
            },
            cancellation_state: if self.state == QueryState::Cancelled {
                "cancelled"
            } else {
                "none"
            },
            error_class: self.error_class,
        }
    }
}

#[derive(Default)]
struct Inner {
    in_flight: HashMap<u64, Rec>,
    recent: VecDeque<Rec>,
}

#[derive(Default)]
pub struct QueryRegistry {
    inner: Mutex<Inner>,
    next_id: AtomicU64,
    /// Running statements, readable without the lock (the sampler's `active_queries`).
    active: AtomicUsize,
    untracked: AtomicU64,
}

pub struct QueryGuard<'a> {
    reg: &'a QueryRegistry,
    id: u64,
    tracked: bool,
    finished: bool,
    /// Where a client-disconnect cancellation (the guard dropped unfinished) is reported.
    disconnect_events: Option<&'a EventLog>,
}

impl QueryRegistry {
    pub fn start(&self) -> QueryGuard<'_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let tracked = inner.in_flight.len() < IN_FLIGHT_CAP;
        if tracked {
            inner.in_flight.insert(
                id,
                Rec {
                    id,
                    class: class::UNPARSED,
                    state: QueryState::Running,
                    start_unix_ms: now_unix_ms(),
                    started: Instant::now(),
                    duration_ms: None,
                    rows_affected: None,
                    rows_returned: None,
                    error_class: None,
                },
            );
        } else {
            self.untracked.fetch_add(1, Ordering::Relaxed);
        }
        drop(inner);
        self.active.fetch_add(1, Ordering::Relaxed);
        QueryGuard {
            reg: self,
            id,
            tracked,
            finished: false,
            disconnect_events: None,
        }
    }

    /// Statements running right now (includes any that could not be tracked individually).
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub fn untracked(&self) -> u64 {
        self.untracked.load(Ordering::Relaxed)
    }

    /// Running statements first by start time, then finished ones, newest start first; at most
    /// `limit.min(MAX_QUERIES_RETURNED)`.
    pub fn list(&self, limit: usize) -> Vec<QueryView> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut all: Vec<&Rec> = inner
            .in_flight
            .values()
            .chain(inner.recent.iter())
            .collect();
        all.sort_by(|a, b| b.start_unix_ms.cmp(&a.start_unix_ms).then(b.id.cmp(&a.id)));
        all.into_iter()
            .take(limit.min(MAX_QUERIES_RETURNED))
            .map(Rec::view)
            .collect()
    }
}

impl<'a> QueryGuard<'a> {
    /// A statement whose guard is dropped before it finished (the client disconnected) also
    /// leaves an operational `query.cancelled` event in `events`.
    pub fn log_disconnect_to(mut self, events: &'a EventLog) -> Self {
        self.disconnect_events = Some(events);
        self
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn set_class(&self, class: &'static str) {
        if !self.tracked {
            return;
        }
        let mut inner = self.reg.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(r) = inner.in_flight.get_mut(&self.id) {
            r.class = class;
        }
    }

    fn finish_with(
        mut self,
        state: QueryState,
        rows_returned: Option<u64>,
        rows_affected: Option<u64>,
        error_class: Option<&'static str>,
    ) {
        self.finish_inner(state, rows_returned, rows_affected, error_class);
    }

    fn finish_inner(
        &mut self,
        state: QueryState,
        rows_returned: Option<u64>,
        rows_affected: Option<u64>,
        error_class: Option<&'static str>,
    ) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.reg.active.fetch_sub(1, Ordering::Relaxed);
        if !self.tracked {
            return;
        }
        let mut inner = self.reg.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(mut r) = inner.in_flight.remove(&self.id) {
            r.state = state;
            r.duration_ms = Some(r.started.elapsed().as_secs_f64() * 1000.0);
            r.rows_returned = rows_returned;
            r.rows_affected = rows_affected;
            r.error_class = error_class;
            if inner.recent.len() >= RECENT_CAP {
                inner.recent.pop_front();
            }
            inner.recent.push_back(r);
        }
    }

    pub fn succeeded(self, rows_returned: Option<u64>, rows_affected: Option<u64>) {
        self.finish_with(QueryState::Succeeded, rows_returned, rows_affected, None);
    }

    pub fn failed(self, error_class: &'static str) {
        self.finish_with(QueryState::Failed, None, None, Some(error_class));
    }

    pub fn timed_out(self) {
        self.finish_with(QueryState::TimedOut, None, None, Some("SQL_TIMEOUT"));
    }

    pub fn cancelled(self) {
        self.finish_with(QueryState::Cancelled, None, None, Some("CANCELLED"));
    }
}

impl Drop for QueryGuard<'_> {
    fn drop(&mut self) {
        // The request future was dropped before the statement finished (client disconnected).
        if self.finished {
            return;
        }
        let started = self
            .reg
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .in_flight
            .get(&self.id)
            .map(|r| r.started);
        self.finish_inner(
            QueryState::Cancelled,
            None,
            None,
            Some("CLIENT_DISCONNECTED"),
        );
        if let Some(events) = self.disconnect_events {
            let mut e = Event::new(
                kind::QUERY_CANCELLED,
                severity::WARNING,
                "query",
                "cancelled",
            )
            .error_class("CLIENT_DISCONNECTED");
            if let Some(t) = started {
                e = e.duration(t.elapsed().as_secs_f64() * 1000.0);
            }
            events.push_operational(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_then_finished_with_counts_and_closed_labels() {
        let r = QueryRegistry::default();
        let g = r.start();
        g.set_class(class::SELECT);
        assert_eq!(r.active(), 1);
        let v = r.list(10);
        assert_eq!((v[0].state, v[0].class), ("running", "select"));
        g.succeeded(Some(3), None);
        assert_eq!(r.active(), 0);
        let v = r.list(10);
        assert_eq!((v[0].state, v[0].rows_returned), ("succeeded", Some(3)));
    }

    #[test]
    fn a_dropped_guard_is_a_cancelled_query_not_a_leak() {
        let r = QueryRegistry::default();
        drop(r.start());
        assert_eq!(r.active(), 0);
        let v = r.list(10);
        assert_eq!(v[0].state, "cancelled");
        assert_eq!(v[0].error_class, Some("CLIENT_DISCONNECTED"));
    }

    #[test]
    fn recent_and_in_flight_are_bounded() {
        let r = QueryRegistry::default();
        for _ in 0..(RECENT_CAP * 3) {
            r.start().succeeded(None, None);
        }
        assert_eq!(r.list(10_000).len(), MAX_QUERIES_RETURNED);
        assert_eq!(r.inner.lock().unwrap().recent.len(), RECENT_CAP);
        let held: Vec<_> = (0..IN_FLIGHT_CAP + 10).map(|_| r.start()).collect();
        assert_eq!(r.active(), IN_FLIGHT_CAP + 10);
        assert_eq!(r.untracked(), 10);
        assert_eq!(r.inner.lock().unwrap().in_flight.len(), IN_FLIGHT_CAP);
        drop(held);
        assert_eq!(r.active(), 0);
    }
}
