//! Full observability for the single-node product: a background sampler publishing coherent
//! snapshots, bounded time-series rings, bounded query / event registries and the counters that
//! did not exist before. Everything here is **read-only with respect to the database**: it
//! reads counters and OS values and keeps its own small, fixed-size state.
//!
//! Three layers, kept apart (see `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md`):
//! * metrics: quantitative values (`/v1/metrics/system`, `/v1/metrics/system/timeseries`);
//! * diagnostic state: what is happening now (`/v1/observability/sessions`, `.../queries`);
//! * events: bounded records of things that happened (`/v1/observability/events`).
//!
//! Safety rules enforced by construction: every metric key and every label is a compile-time
//! constant; user input (SQL text, names, keys, paths, error strings) is never stored; every
//! structure has an explicit cap.

pub mod events;
pub mod probe;
pub mod queries;
pub mod ring;
pub mod sampler;
pub mod version;

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use events::EventLog;
use queries::QueryRegistry;
use sampler::SamplerShared;

/// Counters that are new with this layer. Everything else is read from counters that already
/// existed (engine, SQL, session, route metrics).
#[derive(Default)]
pub struct ObsCounters {
    /// Requests rejected for a missing / unknown credential.
    pub auth_failures: AtomicU64,
    /// Requests whose credential lacked the role the route needs.
    pub auth_forbidden: AtomicU64,
    /// Authenticated requests rejected by the per-principal rate limiter (HTTP 429).
    pub rate_limited: AtomicU64,
    /// `/v1/admin/*` actions (non-GET) and catalog drops that reached their handler.
    pub admin_actions: AtomicU64,
    /// `BEGIN` refused because the principal's session cap was reached.
    pub sessions_rejected: AtomicU64,
    /// Responses with a 5xx status.
    pub http_server_errors: AtomicU64,
    /// Request ids handed out (monotonic; never reused within a process).
    pub request_ids: AtomicU64,
    last_admin: Mutex<Option<(u64, String, &'static str)>>,
}

impl ObsCounters {
    pub fn next_request_id(&self) -> u64 {
        self.request_ids.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Records an admin action. `route` is a route *pattern* (a member of the closed route
    /// table), truncated defensively; it never carries a path parameter value.
    pub fn record_admin_action(&self, route: &str, outcome: &'static str) {
        self.admin_actions.fetch_add(1, Ordering::Relaxed);
        let mut r: String = route.chars().take(64).collect();
        r.shrink_to_fit();
        *self.last_admin.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((events::now_unix_ms(), r, outcome));
    }

    pub fn last_admin_action(&self) -> Option<(u64, String, &'static str)> {
        self.last_admin
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Per-request id (monotonic within the process), placed in the request extensions by the
/// authentication layer so handlers and events can refer to the same request.
#[derive(Debug, Clone, Copy)]
pub struct RequestId(pub u64);

/// What the instance-lock probe could establish. `Unavailable` means the probe could not find
/// out (an I/O error, or no probe is installed): it is reported as such and is **never** treated
/// as "not held".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    Held,
    NotHeld,
    Unavailable,
}

impl LockState {
    pub fn as_str(self) -> &'static str {
        match self {
            LockState::Held => "held",
            LockState::NotHeld => "not_held",
            LockState::Unavailable => "unavailable",
        }
    }
}

/// A probe that answers "does this process still hold the instance lock?". Installed by the
/// embedded host, which owns the lock; absent for the standalone binary (reads `unavailable`).
pub type LockProbe = Arc<dyn Fn() -> LockState + Send + Sync>;

#[derive(Default)]
pub struct Observability {
    pub sampler: SamplerShared,
    pub queries: QueryRegistry,
    pub events: EventLog,
    pub counters: ObsCounters,
    /// Open HTTP connections (maintained by the serving loop).
    pub connections: Arc<AtomicI64>,
    lock_probe: Mutex<Option<LockProbe>>,
}

impl Observability {
    /// Installs a two-valued probe (`true` = held). Convenience over [`Self::set_lock_state_probe`].
    pub fn set_lock_probe(&self, p: Arc<dyn Fn() -> bool + Send + Sync>) {
        self.set_lock_state_probe(Arc::new(move || {
            if p() {
                LockState::Held
            } else {
                LockState::NotHeld
            }
        }));
    }

    pub fn set_lock_state_probe(&self, p: LockProbe) {
        *self.lock_probe.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
    }

    pub fn lock_probe(&self) -> Option<LockProbe> {
        self.lock_probe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Threshold of the **advisory** `disk.free_advisory` field: `low` when free space on the data
    /// volume is below this percent of the volume's total. A provisional value, **not a policy and
    /// not an input of `instance.healthy`** (Decision D1, Option 3): it is echoed in the response
    /// as `disk.free_advisory_threshold_percent` so nobody mistakes it for a guarantee.
    pub fn disk_low_percent(&self) -> f64 {
        10.0
    }
}
