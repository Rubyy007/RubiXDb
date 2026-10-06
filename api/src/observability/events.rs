//! In-memory, bounded operational and security event rings (the "recent events" view).
//!
//! Two rings, never mixed: one for security events and one for operational events, so a flood
//! of one kind (for example thousands of rejected credentials) cannot evict the other kind.
//! Every textual field of an [`Event`] is a `&'static str` taken from the closed sets below, so
//! user input (SQL text, names, keys, paths, error strings) cannot reach an event by
//! construction. Events are ephemeral: they live in memory and are lost on restart; durable
//! security events go to the bounded `security.log` file as before.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

/// Capacity of each ring.
pub const EVENT_RING_CAP: usize = 256;
/// Largest number of events one request may return.
pub const MAX_EVENTS_RETURNED: usize = 200;

/// Event types (closed set).
pub mod kind {
    // security
    pub const AUTH_FAILURE: &str = "auth.failure";
    pub const AUTH_FORBIDDEN: &str = "auth.forbidden";
    pub const AUTH_RATE_LIMITED: &str = "auth.rate_limited";
    pub const ADMIN_ACTION: &str = "admin.action";
    pub const CATALOG_DDL: &str = "catalog.ddl";
    // operational
    pub const INSTANCE_START: &str = "instance.start";
    pub const QUERY_FAILED: &str = "query.failed";
    pub const QUERY_TIMEOUT: &str = "query.timeout";
    pub const QUERY_CANCELLED: &str = "query.cancelled";
    pub const SESSION_REJECTED: &str = "session.rejected";
    pub const INDEX_RECOVERY: &str = "index_recovery";
    pub const SAMPLER_STATE: &str = "sampler.state";
    pub const HTTP_SERVER_ERROR: &str = "http.server_error";
}

pub mod severity {
    pub const INFO: &str = "info";
    pub const WARNING: &str = "warning";
    pub const ERROR: &str = "error";
}

/// `ok` | `denied` | `refused` | `failed` | `timeout` | `cancelled`, plus the recovery / sampler
/// state names listed in their own modules; always a `'static` literal.
#[derive(Debug, Clone)]
pub struct Event {
    pub ts_unix_ms: u64,
    pub event_type: &'static str,
    pub severity: &'static str,
    pub operation_class: &'static str,
    pub result: &'static str,
    pub duration_ms: Option<f64>,
    pub request_id: Option<u64>,
    pub session_id: Option<Uuid>,
    pub error_class: Option<&'static str>,
}

impl Event {
    pub fn new(
        event_type: &'static str,
        severity: &'static str,
        operation_class: &'static str,
        result: &'static str,
    ) -> Self {
        Event {
            ts_unix_ms: now_unix_ms(),
            event_type,
            severity,
            operation_class,
            result,
            duration_ms: None,
            request_id: None,
            session_id: None,
            error_class: None,
        }
    }
    pub fn request(mut self, id: Option<u64>) -> Self {
        self.request_id = id;
        self
    }
    pub fn error_class(mut self, c: &'static str) -> Self {
        self.error_class = Some(c);
        self
    }
    pub fn duration(mut self, ms: f64) -> Self {
        self.duration_ms = Some(ms);
        self
    }
    pub fn session(mut self, id: Option<Uuid>) -> Self {
        self.session_id = id;
        self
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
pub struct EventLog {
    security: Mutex<VecDeque<Event>>,
    operational: Mutex<VecDeque<Event>>,
}

fn push(ring: &Mutex<VecDeque<Event>>, e: Event) {
    let mut r = ring.lock().unwrap_or_else(|p| p.into_inner());
    if r.len() >= EVENT_RING_CAP {
        r.pop_front();
    }
    r.push_back(e);
}

fn last(ring: &Mutex<VecDeque<Event>>, n: usize) -> Vec<Event> {
    let r = ring.lock().unwrap_or_else(|p| p.into_inner());
    r.iter().rev().take(n).cloned().collect()
}

impl EventLog {
    pub fn push_security(&self, e: Event) {
        push(&self.security, e);
    }
    pub fn push_operational(&self, e: Event) {
        push(&self.operational, e);
    }
    /// Newest first, at most `n.min(MAX_EVENTS_RETURNED)`.
    pub fn security(&self, n: usize) -> Vec<Event> {
        last(&self.security, n.min(MAX_EVENTS_RETURNED))
    }
    pub fn operational(&self, n: usize) -> Vec<Event> {
        last(&self.operational, n.min(MAX_EVENTS_RETURNED))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rings_are_bounded_separate_and_newest_first() {
        let log = EventLog::default();
        for i in 0..(EVENT_RING_CAP * 3) {
            log.push_security(
                Event::new(kind::AUTH_FAILURE, severity::WARNING, "auth", "denied")
                    .request(Some(i as u64)),
            );
        }
        log.push_operational(Event::new(
            kind::QUERY_FAILED,
            severity::ERROR,
            "query",
            "failed",
        ));
        // The security flood did not evict the operational event.
        assert_eq!(log.operational(10).len(), 1);
        let sec = log.security(1000);
        assert_eq!(sec.len(), MAX_EVENTS_RETURNED, "clamped to the maximum");
        assert_eq!(sec[0].request_id, Some((EVENT_RING_CAP * 3 - 1) as u64));
        assert!(sec.windows(2).all(|p| p[0].request_id > p[1].request_id));
        assert_eq!(log.security.lock().unwrap().len(), EVENT_RING_CAP);
    }
}
