//! The background sampler: one thread, one coherent immutable [`Snapshot`] per tick.
//!
//! Rules this module keeps (and tests):
//! * one thread (`rubixdb-sampler`), started with the API server and joined when its
//!   [`SamplerHandle`] is dropped; no thread survives a stop;
//! * a panic in one tick is caught; the sampler goes `degraded`/`failed`, the server is
//!   unaffected, and the next tick runs normally;
//! * it never runs SQL, never writes, never takes a lock that the request path or the engine's
//!   write path holds across work: it reads relaxed atomics and short, uncontended registry
//!   locks (each held for a few instructions) and the engine's own cheap accessors;
//! * it publishes by swapping an `Arc<Snapshot>` behind an `RwLock` held only for the swap;
//!   request handlers clone the `Arc`. Neither side ever waits on the other for more than that;
//! * a value that cannot be measured is `None` (rendered `null`), never `0`;
//! * every snapshot carries its own timestamp and generation, so a reader can always tell how old
//!   it is, and the response layer marks a stale or dead sampler as such.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::events::{kind, now_unix_ms, severity, Event};
use super::probe::{OsProbe, RealProbe};
use super::ring::{SeriesStore, SERIES_NAMES};
use crate::resources::{self, IoCounters};
use crate::state::AppState;

/// Default sampling period.
pub const TICK: Duration = Duration::from_secs(1);
/// Directory sizes (a recursive walk) are refreshed this often, not every tick.
const SIZES_EVERY: Duration = Duration::from_secs(10);
/// A snapshot older than this many milliseconds is reported as stale.
pub const STALE_AFTER_MS: u64 = 5_000;
/// A snapshot older than this is reported as a failed sampler even if its thread claims to run.
pub const DEAD_AFTER_MS: u64 = 10_000;
/// Consecutive panicking ticks after which the sampler reports `failed`.
const FAIL_AFTER_PANICS: u32 = 5;
/// Readiness continuously false for longer than this makes health `failed`.
const NOT_READY_FAILS_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplerState {
    NotStarted,
    Running,
    Degraded,
    Failed,
}

impl SamplerState {
    pub fn as_str(self) -> &'static str {
        match self {
            SamplerState::NotStarted => "not_started",
            SamplerState::Running => "running",
            SamplerState::Degraded => "degraded",
            SamplerState::Failed => "failed",
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            1 => SamplerState::Running,
            2 => SamplerState::Degraded,
            3 => SamplerState::Failed,
            _ => SamplerState::NotStarted,
        }
    }
}

/// One coherent measurement set. Built completely, then published; never mutated afterwards.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub generation: u64,
    pub taken_unix_ms: u64,
    pub taken_at: Instant,
    pub uptime_secs: f64,

    pub cpu_percent: Option<f64>,
    pub cpu_peak_percent: Option<f64>,
    pub vcpu_count: Option<u32>,

    pub rss_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub system_total_bytes: Option<u64>,
    pub system_used_bytes: Option<u64>,

    pub disk_total_bytes: Option<u64>,
    pub disk_free_bytes: Option<u64>,
    pub db_bytes: Option<u64>,
    pub wal_bytes: Option<u64>,
    pub sstable_bytes: Option<u64>,
    pub wal_segments: Option<u64>,
    /// When the directory sizes above were measured (they are refreshed every 10 s).
    pub sizes_taken_unix_ms: Option<u64>,
    pub read_iops: Option<f64>,
    pub write_iops: Option<f64>,
    pub read_mb_per_sec: Option<f64>,
    pub write_mb_per_sec: Option<f64>,

    pub http_requests_per_sec: Option<f64>,
    pub sql_queries_per_sec: Option<f64>,
    pub write_commits_per_sec: Option<f64>,
    pub active_connections: i64,
    pub active_sessions: u64,
    pub active_transactions: u64,
    pub active_queries: u64,

    pub query_p50_ms: Option<f64>,
    pub query_p95_ms: Option<f64>,
    pub query_p99_ms: Option<f64>,

    pub wal_state: String,
    pub wal_pending_waiters: u64,
    pub compaction_running: bool,
    pub compaction_cycles: u64,
    pub live_sstable_count: u64,
    pub last_compaction_ms: Option<f64>,
    pub flush_queue_depth: u64,
    pub index_build_state: &'static str,

    pub storage_state: &'static str,
    pub readiness: &'static str,
    pub health: &'static str,

    pub auth_failures: u64,
    pub auth_forbidden: u64,
    pub rate_limited: u64,
    pub admin_actions: u64,
    pub last_admin_action: Option<(u64, String, &'static str)>,
    pub sessions_rejected: u64,
    pub http_server_errors: u64,
    pub wal_backpressure_rejections: u64,
    pub wal_write_errors: u64,
    pub wal_sync_failures: u64,
    pub sql_resource_limit_hits: u64,
    pub sql_errors: u64,
}

/// State shared between the sampler thread and the request handlers.
pub struct SamplerShared {
    latest: RwLock<Option<Arc<Snapshot>>>,
    state: AtomicU8,
    generation: AtomicU64,
    series: Mutex<SeriesStore>,
    tick_ms: AtomicU64,
    panics: AtomicU64,
    ticks: AtomicU64,
    thread_running: AtomicBool,
}

impl Default for SamplerShared {
    fn default() -> Self {
        SamplerShared {
            latest: RwLock::new(None),
            state: AtomicU8::new(0),
            generation: AtomicU64::new(0),
            series: Mutex::new(SeriesStore::new()),
            tick_ms: AtomicU64::new(TICK.as_millis() as u64),
            panics: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            thread_running: AtomicBool::new(false),
        }
    }
}

impl SamplerShared {
    /// The newest published snapshot, if any tick has completed.
    pub fn latest(&self) -> Option<Arc<Snapshot>> {
        self.latest
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The sampler's own state as stored (see [`Self::effective_state`] for what a reader sees).
    pub fn state(&self) -> SamplerState {
        SamplerState::from_u8(self.state.load(Ordering::SeqCst))
    }

    /// What a client is told: a sampler that claims to run but has not published for
    /// [`DEAD_AFTER_MS`] is `failed`, never silently "running".
    pub fn effective_state(&self, age_ms: Option<u64>) -> SamplerState {
        match (self.state(), age_ms) {
            (SamplerState::NotStarted, _) => SamplerState::NotStarted,
            (_, Some(a)) if a > DEAD_AFTER_MS.max(self.tick_ms.load(Ordering::Relaxed) * 10) => {
                SamplerState::Failed
            }
            (s, _) => s,
        }
    }

    pub fn tick_ms(&self) -> u64 {
        self.tick_ms.load(Ordering::Relaxed)
    }
    pub fn panics(&self) -> u64 {
        self.panics.load(Ordering::Relaxed)
    }
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    /// Feeds one tick into the time-series rings with an injected clock. **For tests only**
    /// (simulating hours of history without waiting); the sampler thread never calls it.
    #[doc(hidden)]
    pub fn inject_series_tick(&self, now_ms: u64, values: [Option<f64>; 8]) {
        self.series
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .on_tick(now_ms, values);
    }

    /// Bytes currently held by the series store (constant after construction).
    pub fn series_memory_bytes(&self) -> usize {
        self.with_series(|s| s.memory_bytes())
    }

    /// Time-series access for the timeseries endpoint.
    pub fn with_series<R>(&self, f: impl FnOnce(&SeriesStore) -> R) -> R {
        let g = self.series.lock().unwrap_or_else(|p| p.into_inner());
        f(&g)
    }

    fn set_state(&self, s: SamplerState) -> SamplerState {
        let v = match s {
            SamplerState::NotStarted => 0,
            SamplerState::Running => 1,
            SamplerState::Degraded => 2,
            SamplerState::Failed => 3,
        };
        SamplerState::from_u8(self.state.swap(v, Ordering::SeqCst))
    }

    fn publish(&self, s: Snapshot) {
        *self.latest.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(s));
    }
}

/// Counters the previous tick left behind, to turn cumulative counters into rates.
#[derive(Default)]
struct Prev {
    at: Option<Instant>,
    cpu_seconds: Option<f64>,
    io: Option<IoCounters>,
    http_total: u64,
    sql_total: u64,
    commits_total: u64,
    peak_cpu: Option<f64>,
    not_ready_since: Option<Instant>,
    sizes_at: Option<Instant>,
    sizes: Sizes,
    /// Probes that have answered at least once; if one later stops answering the sampler is
    /// `degraded` (a probe that never existed on this platform is just `null`).
    ever_ok: [bool; 6],
}

#[derive(Clone, Copy, Default)]
struct Sizes {
    db: Option<u64>,
    wal: Option<u64>,
    sstable: Option<u64>,
    wal_segments: Option<u64>,
    taken_unix_ms: Option<u64>,
}

fn rate(cur: u64, prev: u64, dt_s: f64) -> Option<f64> {
    (dt_s > 0.0 && cur >= prev).then(|| (cur - prev) as f64 / dt_s)
}

fn walk_sizes(data_dir: &std::path::Path) -> Sizes {
    let du = resources::disk_usage(data_dir);
    let wal_dir = data_dir.join("wal");
    let segments = std::fs::read_dir(&wal_dir).ok().map(|rd| {
        rd.flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("wal-") && n.ends_with(".log"))
            })
            .count() as u64
    });
    Sizes {
        db: Some(du.data_dir_bytes),
        wal: Some(du.wal_bytes),
        sstable: Some(du.sstable_bytes),
        wal_segments: segments,
        taken_unix_ms: Some(now_unix_ms()),
    }
}

/// One measurement pass. Pure with respect to the product: reads counters and the OS, builds a
/// [`Snapshot`], never writes anywhere but `prev` (and the caller publishes the result).
fn collect(
    state: &AppState,
    probe: &dyn OsProbe,
    prev: &mut Prev,
    generation: u64,
    degraded_out: &mut bool,
) -> Snapshot {
    let now = Instant::now();
    let dt_s = prev.at.map(|p| now.duration_since(p).as_secs_f64());

    // ---- OS probes ----
    let vcpu = probe.vcpu_count();
    let cpu_seconds = probe.process_cpu_seconds();
    let rss = probe.process_rss();
    let sysmem = probe.system_memory();
    let disk = probe.disk_capacity(&state.config.data_dir);
    let io = probe.process_io();
    let answered = [
        vcpu.is_some(),
        cpu_seconds.is_some(),
        rss.is_some(),
        sysmem.is_some(),
        io.is_some(),
        disk.is_some(),
    ];
    for (k, a) in answered.iter().enumerate() {
        if *a {
            prev.ever_ok[k] = true;
        } else if prev.ever_ok[k] {
            *degraded_out = true;
        }
    }

    let cpu_percent = match (cpu_seconds, prev.cpu_seconds, dt_s, vcpu) {
        (Some(c), Some(p), Some(dt), Some(n)) if dt > 0.0 && n > 0 && c >= p => {
            Some(((c - p) / dt / f64::from(n) * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    if let Some(c) = cpu_percent {
        prev.peak_cpu = Some(prev.peak_cpu.map_or(c, |p| p.max(c)));
    }
    let (read_iops, write_iops, read_mbs, write_mbs) = match (io, prev.io, dt_s) {
        (Some(c), Some(p), Some(dt)) if dt > 0.0 => (
            rate(c.read_ops, p.read_ops, dt),
            rate(c.write_ops, p.write_ops, dt),
            rate(c.read_bytes, p.read_bytes, dt).map(|b| b / 1_000_000.0),
            rate(c.write_bytes, p.write_bytes, dt).map(|b| b / 1_000_000.0),
        ),
        _ => (None, None, None, None),
    };

    // ---- directory sizes (every SIZES_EVERY) ----
    if prev
        .sizes_at
        .is_none_or(|t| now.duration_since(t) >= SIZES_EVERY)
    {
        prev.sizes = walk_sizes(&state.config.data_dir);
        prev.sizes_at = Some(now);
    }

    // ---- product counters (relaxed atomics / short registry locks) ----
    let ps = state.engine.pool_stats();
    let g = &ps.committer_stats;
    let cm = state.engine.compaction_metrics();
    let storage = state.engine.storage_state();
    let storage_state = match storage {
        rubixdb::lsm::StorageState::Healthy => "Healthy",
        rubixdb::lsm::StorageState::StoragePressure => "StoragePressure",
        rubixdb::lsm::StorageState::StorageFull => "StorageFull",
    };
    let sql_api = state.sql.api_metrics.snapshot();
    let http_total = state.metrics.total_requests();
    let commits_total = ps.completed_ok;
    let (http_rps, sql_qps, commits_ps) = match dt_s {
        Some(dt) => (
            rate(http_total, prev.http_total, dt),
            rate(sql_api.requests, prev.sql_total, dt),
            rate(commits_total, prev.commits_total, dt),
        ),
        None => (None, None, None),
    };
    let lat = state.metrics.route_percentiles("POST /v1/sql");
    let txn = state.sql.txm.metrics();
    let exec = state.sql.exec_metrics.snapshot();
    let plan = state.sql.planner_metrics.snapshot();
    let c = &state.obs.counters;

    // ---- readiness / health (computed once per tick, stable between ticks) ----
    let poisoned = g.sync_failures() > 0
        || matches!(
            ps.state,
            rubixdb::execution::batch_coordinator::PoolState::Failed
        );
    let ready = ps.coordinator_alive && !poisoned;
    if ready {
        prev.not_ready_since = None;
    } else if prev.not_ready_since.is_none() {
        prev.not_ready_since = Some(now);
    }
    let not_ready_too_long = prev
        .not_ready_since
        .is_some_and(|t| now.duration_since(t) > NOT_READY_FAILS_AFTER);
    let lock_lost = state.obs.lock_probe().map(|held| !held()).unwrap_or(false);
    let low_disk = match (disk, state.obs.disk_low_percent()) {
        (Some((total, free)), pct) if total > 0 => (free as f64) < (total as f64) * pct / 100.0,
        _ => false,
    };
    let health =
        if not_ready_too_long || storage == rubixdb::lsm::StorageState::StorageFull || lock_lost {
            "failed"
        } else if storage == rubixdb::lsm::StorageState::StoragePressure || low_disk {
            "degraded"
        } else {
            "healthy"
        };

    let snap = Snapshot {
        generation,
        taken_unix_ms: now_unix_ms(),
        taken_at: now,
        uptime_secs: crate::state::uptime(state).as_secs_f64(),
        cpu_percent,
        cpu_peak_percent: prev.peak_cpu,
        vcpu_count: vcpu,
        rss_bytes: rss.map(|r| r.0),
        peak_rss_bytes: rss.map(|r| r.1),
        system_total_bytes: sysmem.map(|m| m.0),
        system_used_bytes: sysmem.map(|m| m.0.saturating_sub(m.1)),
        disk_total_bytes: disk.map(|d| d.0),
        disk_free_bytes: disk.map(|d| d.1),
        db_bytes: prev.sizes.db,
        wal_bytes: prev.sizes.wal,
        sstable_bytes: prev.sizes.sstable,
        wal_segments: prev.sizes.wal_segments,
        sizes_taken_unix_ms: prev.sizes.taken_unix_ms,
        read_iops,
        write_iops,
        read_mb_per_sec: read_mbs,
        write_mb_per_sec: write_mbs,
        http_requests_per_sec: http_rps,
        sql_queries_per_sec: sql_qps,
        write_commits_per_sec: commits_ps,
        active_connections: state.obs.connections.load(Ordering::Relaxed),
        active_sessions: state.sql.sessions.open_count() as u64,
        active_transactions: txn.active_transactions,
        active_queries: state.obs.queries.active() as u64,
        query_p50_ms: lat.map(|l| l.0),
        query_p95_ms: lat.map(|l| l.1),
        query_p99_ms: lat.map(|l| l.2),
        wal_state: format!("{:?}", ps.state),
        wal_pending_waiters: g.pending_waiters as u64,
        compaction_running: state.engine.compaction_running(),
        compaction_cycles: cm.cycles_completed,
        live_sstable_count: state.engine.sstable_count() as u64,
        last_compaction_ms: cm
            .last_cycle
            .as_ref()
            .map(|cy| cy.duration.as_secs_f64() * 1000.0),
        flush_queue_depth: state.engine.immutable_count() as u64,
        index_build_state: state.index_recovery.state().as_str(),
        storage_state,
        readiness: if ready { "ready" } else { "not_ready" },
        health,
        auth_failures: c.auth_failures.load(Ordering::Relaxed),
        auth_forbidden: c.auth_forbidden.load(Ordering::Relaxed),
        rate_limited: c.rate_limited.load(Ordering::Relaxed),
        admin_actions: c.admin_actions.load(Ordering::Relaxed),
        last_admin_action: c.last_admin_action(),
        sessions_rejected: c.sessions_rejected.load(Ordering::Relaxed),
        http_server_errors: c.http_server_errors.load(Ordering::Relaxed),
        wal_backpressure_rejections: ps.rejected_backpressure,
        wal_write_errors: ps.completed_err,
        wal_sync_failures: g.sync_failures(),
        sql_resource_limit_hits: exec.aggregate_resource_limit_hits + plan.plan_resource_limit_hits,
        sql_errors: sql_api.errors,
    };

    prev.at = Some(now);
    prev.cpu_seconds = cpu_seconds;
    prev.io = io;
    prev.http_total = http_total;
    prev.sql_total = sql_api.requests;
    prev.commits_total = commits_total;
    snap
}

/// Series values of a snapshot, in `SERIES_NAMES` order.
fn series_values(s: &Snapshot) -> [Option<f64>; 8] {
    debug_assert_eq!(SERIES_NAMES.len(), 8);
    [
        s.cpu_percent,
        s.rss_bytes.map(|v| v as f64),
        s.read_iops,
        s.write_iops,
        s.read_mb_per_sec,
        s.write_mb_per_sec,
        s.sql_queries_per_sec,
        Some(s.active_queries as f64),
    ]
}

struct Worker {
    state: Arc<AppState>,
    probe: Arc<dyn OsProbe>,
    prev: Prev,
    consecutive_panics: u32,
}

impl Worker {
    /// One tick: collect, publish, record the series, update the sampler state. Panics inside
    /// are caught; the tick then counts as failed and the next one starts fresh.
    fn tick(&mut self) {
        let shared = &self.state.obs.sampler;
        let generation = shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut degraded = false;
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            collect(
                &self.state,
                self.probe.as_ref(),
                &mut self.prev,
                generation,
                &mut degraded,
            )
        }));
        match outcome {
            Ok(snap) => {
                self.consecutive_panics = 0;
                let values = series_values(&snap);
                let t = snap.taken_unix_ms;
                shared.publish(snap);
                shared
                    .series
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .on_tick(t, values);
                let new = if degraded {
                    SamplerState::Degraded
                } else {
                    SamplerState::Running
                };
                let old = shared.set_state(new);
                if old != new && old != SamplerState::NotStarted {
                    self.state.obs.events.push_operational(Event::new(
                        kind::SAMPLER_STATE,
                        if degraded {
                            severity::WARNING
                        } else {
                            severity::INFO
                        },
                        "sampler",
                        new.as_str(),
                    ));
                }
            }
            Err(_) => {
                shared.panics.fetch_add(1, Ordering::Relaxed);
                self.consecutive_panics += 1;
                let new = if self.consecutive_panics >= FAIL_AFTER_PANICS {
                    SamplerState::Failed
                } else {
                    SamplerState::Degraded
                };
                let old = shared.set_state(new);
                if old != new {
                    self.state.obs.events.push_operational(Event::new(
                        kind::SAMPLER_STATE,
                        severity::ERROR,
                        "sampler",
                        new.as_str(),
                    ));
                }
            }
        }
        shared.ticks.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Stop {
    flag: Mutex<bool>,
    cv: Condvar,
}

/// Owns the sampler thread. Dropping it stops and joins the thread.
pub struct SamplerHandle {
    stop: Arc<Stop>,
    join: Option<JoinHandle<()>>,
    state: Arc<AppState>,
}

#[derive(Debug)]
pub enum StartError {
    AlreadyRunning,
    Spawn(std::io::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::AlreadyRunning => write!(f, "the sampler is already running"),
            StartError::Spawn(e) => write!(f, "could not start the sampler thread: {e}"),
        }
    }
}
impl std::error::Error for StartError {}

/// Starts the 1 Hz sampler with the real platform probe.
pub fn start(state: &Arc<AppState>) -> Result<SamplerHandle, StartError> {
    start_with(state, Arc::new(RealProbe), TICK)
}

/// Starts the sampler with an explicit probe and period (tests inject failing probes and a
/// short period). The first measurement is taken before this returns, so a snapshot exists as
/// soon as the server is up.
pub fn start_with(
    state: &Arc<AppState>,
    probe: Arc<dyn OsProbe>,
    interval: Duration,
) -> Result<SamplerHandle, StartError> {
    let shared = &state.obs.sampler;
    if shared
        .thread_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(StartError::AlreadyRunning);
    }
    shared
        .tick_ms
        .store(interval.as_millis().max(1) as u64, Ordering::Relaxed);
    let mut worker = Worker {
        state: Arc::clone(state),
        probe,
        prev: Prev::default(),
        consecutive_panics: 0,
    };
    worker.tick(); // first snapshot before the server reports ready
    let stop = Arc::new(Stop::default());
    let stop2 = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("rubixdb-sampler".to_string())
        .spawn(move || {
            let mut next = Instant::now() + interval;
            loop {
                let wait = next.saturating_duration_since(Instant::now());
                {
                    let g = stop2.flag.lock().unwrap_or_else(|p| p.into_inner());
                    let (g, _) = stop2
                        .cv
                        .wait_timeout_while(g, wait, |stopped| !*stopped)
                        .unwrap_or_else(|p| p.into_inner());
                    if *g {
                        return;
                    }
                }
                worker.tick();
                next += interval;
                let now = Instant::now();
                if next < now {
                    next = now + interval; // never burst to catch up
                }
            }
        })
        .map_err(|e| {
            shared.thread_running.store(false, Ordering::SeqCst);
            StartError::Spawn(e)
        })?;
    Ok(SamplerHandle {
        stop,
        join: Some(join),
        state: Arc::clone(state),
    })
}

impl SamplerHandle {
    /// Stops the thread and waits for it. Idempotent (also run by `Drop`).
    pub fn stop(&mut self) {
        if let Some(j) = self.join.take() {
            *self.stop.flag.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.stop.cv.notify_all();
            let _ = j.join();
            let shared = &self.state.obs.sampler;
            shared.set_state(SamplerState::NotStarted);
            shared.thread_running.store(false, Ordering::SeqCst);
        }
    }
}

impl Drop for SamplerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}
