//! Phase 1: Group Commit. `GroupCommitter` wraps a `FileWal` configured with
//! `SyncMode::GroupCommit` and coalesces concurrent callers' durability
//! waits into batched `fsync` calls: one caller per batch becomes the
//! *leader* (waits a short window for more work to arrive, then calls
//! `fsync` once on behalf of everyone whose bytes are already written),
//! every other concurrent caller is a *follower* (waits for the leader to
//! publish a monotone `durable_through` watermark, or for a bounded
//! timeout). This implements the extension point WAL Spec §4/§9 name but
//! does not itself define in byte-level detail — no on-disk format changes,
//! no change to `Wal`'s method signatures, no change to `FileWal`'s
//! single-writer internal model.
//!
//! See `PROCESS.md` §1 for the full design log, written before this file.
//! The short version:
//!
//! - **Two independent locks, never held across an `fsync`.** `wal`
//!   (`Mutex<FileWal>`) is held only for memory-speed operations —
//!   `append()`, `rotate()`, and snapshotting `(cloned segment handle,
//!   batch_max_seq)` — never across the `fsync` syscall itself. `batch`
//!   (`Mutex<BatchState>`) guards only leader-election/poison bookkeeping.
//!   The actual `fsync` happens on a `std::fs::File::try_clone`'d handle,
//!   entirely outside both locks — `fsync` is a property of the underlying
//!   file, not of the handle used to invoke it (this crate's own
//!   `tests/crash_consistency.rs` documents the same OS-level fact from
//!   the opposite direction), so this is safe without `unsafe` and without
//!   a new dependency.
//! - **No per-waiter bookkeeping.** Every waiter (leader or follower)
//!   re-derives "am I durable yet" from the single `AtomicU64
//!   durable_through` watermark, which only ever moves forward
//!   (`fetch_max`, `Ordering::Release`) after a *completed, successful*
//!   `fsync`. A failed `fsync` only ever sets `poisoned` — it never moves
//!   `durable_through` backward or resets it.
//! - **Rotation needs no special-case logic** (M1.5): `seq` is WAL-wide,
//!   not segment-scoped, and a leader's sync target is snapshotted
//!   atomically with respect to `rotate()` (both require the same `wal`
//!   lock), so an in-flight leader's cloned handle always refers to a
//!   file `rotate()` can seal around it but never invalidates. See
//!   `PROCESS.md` §1.5 for the full linearization argument.
//! - **`GroupCommitter::new` performs one real, synchronous `fsync` before
//!   returning** (a "warm-up" probe — see its own doc comment for the
//!   cold-start problem this solves). Construction therefore blocks for
//!   roughly one `fsync`'s worth of latency (this machine's own WAL
//!   benchmark: 3–10 ms for `Immediate`-mode `append_sync`, see
//!   `PROGRESS.md`) — a one-time cost, not a per-call one, but real enough
//!   that a caller on the hot path (rather than at startup) should not
//!   construct a fresh `GroupCommitter` per request.

use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::error::{EngineError, Result};
use crate::wal::metrics::FsyncLatencyTracker;
use crate::wal::{FileWal, SyncMode, Wal, WalOp, WalPosition};

/// Default backpressure bound (§11) for `GroupCommitter::new` — see
/// `with_max_pending_waiters`'s doc comment. Chosen to be far above any
/// realistic concurrent-writer count this crate's own test suite exercises
/// (M1.3: 1,000) while still being a real, finite bound rather than
/// `usize::MAX` masquerading as "unbounded" — a caller with an unusual
/// concurrency profile should call `with_max_pending_waiters` explicitly
/// rather than rely on this default being "big enough" by accident.
pub const DEFAULT_MAX_PENDING_WAITERS: usize = 65_536;

/// How long `GroupCommitter::shutdown` waits for an already-in-flight
/// leader batch to finish before giving up and returning a snapshot
/// anyway — see `shutdown`'s doc comment. `shutdown()` itself must never
/// hang, so this is a real bound, not best-effort.
const SHUTDOWN_DRAIN_BOUND: Duration = Duration::from_secs(5);

/// The leader's batch window is `min(max_wait_cap, EMA / WINDOW_EMA_
/// DIVISOR)`. Originally `10`; changed to `1` (i.e. `EMA / 1`, the EMA
/// itself) after a controlled window-size sweep (`PHASE1_TEST_RESULTS.md`,
/// window-size sweep section; decision recorded in `PHASE1_ADR.md`
/// ADR-12) showed throughput scaling substantially with window size on
/// this machine — `/10` was leaving most of the available batching
/// headroom on the table, not merely respecting a hardware limit. At `/1`,
/// the window on a machine whose `fsync` latency matches this
/// development environment's (~2.8ms) lands close to the sweep's
/// empirically strongest configuration (~3ms, tested via the experiment
/// override, not this constant) while still shrinking proportionally on
/// faster storage (the whole reason this is EMA-relative rather than a
/// flat constant): a disk with 50µs `fsync` latency yields a ~50µs window
/// here, not a fixed multi-millisecond one paid regardless of how fast
/// the disk actually is.
const WINDOW_EMA_DIVISOR: u64 = 1;

/// The demand-adaptive probe duration `spin_wait_for_batch_window` waits
/// before deciding whether to extend toward the full `window` — see that
/// function's doc comment. Set to the *original* `WINDOW_EMA_DIVISOR = 10`
/// era's effective default (~200µs on this development machine), since
/// that was already empirically shown (the window-size sweep's own
/// baseline configuration) to be enough time for a follower to join under
/// genuine contention, while being short enough that a lone writer barely
/// notices paying it.
const PROBE_WINDOW: Duration = Duration::from_micros(200);

/// How often `spin_wait_for_batch_window`'s production wait mechanism
/// calls `yield_now()` instead of `spin_loop()` — see that function's doc
/// comment. Shared with `phase1_waitmode_experiment`'s `Spin` mode (the
/// experiment's own reduction to exactly this production behavior), so
/// the two can never silently drift apart the way an independently
/// hardcoded divisor once did (`PHASE1_TEST_RESULTS.md` §17 finding #8).
const YIELD_EVERY: u32 = 10_000;

/// The production wait step: `spin_loop()` every iteration except every
/// `YIELD_EVERY`th, which yields instead. Factored out of `spin_wait_for_
/// batch_window`'s loop body so `phase1_waitmode_experiment`'s `Spin` mode
/// can call the exact same code rather than a duplicated copy of it.
#[cfg_attr(feature = "phase1-waitmode-experiment", allow(dead_code))]
fn wait_step_production(iterations: u32) {
    if iterations.is_multiple_of(YIELD_EVERY) {
        std::thread::yield_now();
    } else {
        std::hint::spin_loop();
    }
}

/// A point-in-time snapshot of batching observability counters —
/// `GroupCommitter::stats()`. All counts are cumulative since
/// construction; none of them participate in any correctness decision
/// (durability is always derived from `durable_through`/`FileWal::
/// next_seq`, never from these).
#[derive(Debug, Clone, Copy, Default)]
pub struct GroupCommitStats {
    /// Number of times a caller became leader and attempted a batch
    /// `fsync` (successful or not).
    pub sync_attempts: u64,
    /// Number of those attempts whose `fsync` returned `Ok`.
    pub sync_successes: u64,
    /// Sum of records covered across every *successful* batch.
    pub records_total: u64,
    /// The largest single successful batch's record count.
    pub max_batch_records: u64,
    /// Sum of every leader's batch-window wait duration, in nanoseconds —
    /// divide by `window_wait_samples` for the mean.
    pub window_wait_ns_total: u64,
    pub window_wait_samples: u64,
    /// `GroupCommitter::durable_through()` at the moment of this snapshot.
    pub durable_through: u64,
    /// Callers currently inside `await_durable` at the moment of this
    /// snapshot (leader or follower).
    pub pending_waiters: usize,
    /// The highest `seq` this `FileWal` has assigned so far (`next_seq()
    /// - 1`) — may exceed `durable_through` if a batch is in flight.
    pub highest_sequence: u64,
    /// Segment rotations observed since this `GroupCommitter` was
    /// constructed (explicit `rotate()` calls and `FileWal::append`'s own
    /// automatic mid-append rotation both count identically — see
    /// `initial_segment_id`'s doc comment).
    pub segment_rotations: u64,
}

impl GroupCommitStats {
    /// `sync_attempts - sync_successes`.
    pub fn sync_failures(&self) -> u64 {
        self.sync_attempts.saturating_sub(self.sync_successes)
    }

    /// Mean records per successful batch, or `0.0` if none have completed.
    pub fn avg_batch_records(&self) -> f64 {
        if self.sync_successes == 0 {
            0.0
        } else {
            (self.records_total as f64) / (self.sync_successes as f64)
        }
    }

    /// Mean leader batch-window wait, in nanoseconds, or `0.0` if no batch
    /// has run yet.
    pub fn avg_window_wait_ns(&self) -> f64 {
        if self.window_wait_samples == 0 {
            0.0
        } else {
            (self.window_wait_ns_total as f64) / (self.window_wait_samples as f64)
        }
    }
}

/// The outcome of `GroupCommitter::shutdown` — see its doc comment.
#[derive(Debug, Clone, Copy)]
pub struct ShutdownReport {
    /// The durability watermark at the moment of this snapshot.
    pub durable_through: u64,
    /// The highest `seq` this `FileWal` had assigned at the moment of this
    /// snapshot — may exceed `durable_through` if a batch was still in
    /// flight (or never got the chance to start) when `shutdown()` was
    /// called.
    pub highest_assigned_seq: u64,
}

impl ShutdownReport {
    /// `true` if any assigned `seq` was not yet proven durable at the
    /// moment of this snapshot — the caller-visible signal §10 requires
    /// ("If pending writes are not durable at shutdown, the implementation
    /// must report that state explicitly").
    pub fn has_undurable_pending(&self) -> bool {
        self.highest_assigned_seq > self.durable_through
    }
}

/// Why a `GroupCommitter` is poisoned (`BatchState::poisoned`) — see
/// `PHASE3_FAILURE_MODEL.md` §2 for the full leader-failure state machine.
/// Both variants are permanent and terminal: a poisoned `GroupCommitter`
/// stays poisoned until it is dropped and a fresh one is constructed
/// (`FileWal::open_for_recovery` re-scans from disk, per this project's
/// standing "no in-process repair" rule — `wal::mod`'s poison-on-rollback-
/// failure precedent).
#[derive(Debug, Clone, Copy)]
enum PoisonReason {
    /// The leader's `fsync` call returned `Err` (a real I/O failure, not a
    /// panic) — the original Phase 1 poisoning path.
    FsyncFailed(io::ErrorKind),
    /// The leader thread unwound (panicked) somewhere between being
    /// elected and calling `finish_batch_ok`/`finish_batch_with_error` —
    /// detected by `LeaderFailureGuard::drop`, not by any `Result` the
    /// leader itself returned (it never got the chance to return one).
    /// Whether the underlying `fsync` syscall itself had already
    /// completed at the moment of the panic is unknown and unknowable
    /// from here — see `LeaderFailureGuard`'s doc comment for why
    /// poisoning unconditionally, rather than trying to guess, is the
    /// only fail-closed choice.
    LeaderPanicked,
}

/// Coordination state guarded by `GroupCommitter::batch`. Deliberately
/// minimal (`PROCESS.md` §1.10): no per-waiter registry is needed, since
/// every waiter independently re-derives its own outcome from
/// `durable_through`/`poisoned` after every wake.
#[derive(Debug, Default)]
struct BatchState {
    /// `true` while some thread is between "elected leader" and "finished
    /// this batch" (success or failure). Only ever set by a thread that
    /// just transitioned `false -> true` under this lock; only ever
    /// cleared by that same thread once its batch concludes — including
    /// via `LeaderFailureGuard` if the leader thread panics instead of
    /// returning, so this can never be left permanently `true` (the P0
    /// leader-failure issue `PHASE3_FAILURE_MODEL.md` fixes).
    leader_active: bool,
    /// Set once, permanently, the first time a leader's batch fails
    /// (`fsync` error or leader panic). Never cleared — see `PoisonReason`.
    poisoned: Option<PoisonReason>,
}

/// **P0 fix (Phase 3): a panicking leader must not leave `leader_active`
/// stuck `true` forever.** Armed the instant a thread is elected leader
/// (`await_durable`, under `batch`, before `run_as_leader` is called) and
/// disarmed only after `run_as_leader` returns normally — by which point
/// it has *already* called `finish_batch_ok`/`finish_batch_with_error`
/// itself, so a normal return makes this guard's own `Drop` a deliberate
/// no-op. If the leader thread instead panics anywhere inside `run_as_
/// leader` (a real, exercised scenario — see `do_leader_fsync`'s test-only
/// fault-injection hook, and Phase 2B's own finding that a leader
/// panicking mid-`fsync` left `leader_active` stuck: `PHASE2B_FAILURE_
/// MODEL.md` §3), the guard is still armed when Rust unwinds through it,
/// and its `Drop` runs the same "conclude this batch" bookkeeping `finish_
/// batch_with_error` would have — clearing `leader_active` and poisoning
/// the committer — during the unwind itself, before the panic propagates
/// any further up the caller's stack. Mirrors `execution::common::
/// CompletionGuard`'s existing pattern exactly (armed-unless-explicitly-
/// disarmed, fallback logic in `Drop`) — not a new abstraction.
///
/// **Why poison unconditionally, rather than just clearing `leader_active`
/// and letting a new leader be elected?** A panic mid-`run_as_leader` can
/// land before the `fsync` call, during it, or after it succeeded but
/// before `durable_through` was published — this guard cannot distinguish
/// those cases (the panic could originate from a test-injected hook at
/// any of them, or a genuine future bug). Poisoning is the same fail-
/// closed answer this module already gives a *failed* `fsync`
/// (`finish_batch_with_error`): every waiter gets a prompt, bounded,
/// clearly-labeled error instead of silently racing a next leader against
/// a batch whose outcome was never actually confirmed. Recovery relies on
/// the existing WAL recovery contract (discard this `GroupCommitter`,
/// reopen via `FileWal::open_for_recovery`, which re-scans from disk and
/// only ever trusts bytes a `fsync` actually completed) — no new recovery
/// mechanism is introduced here, per this project's own standing rule
/// that `GroupCommitter` must not duplicate `FileWal`'s recovery logic.
struct LeaderFailureGuard<'a> {
    committer: &'a GroupCommitter,
    armed: bool,
}

impl<'a> LeaderFailureGuard<'a> {
    fn new(committer: &'a GroupCommitter) -> Self {
        LeaderFailureGuard {
            committer,
            armed: true,
        }
    }

    /// Called only after `run_as_leader` has returned normally (`Ok` or
    /// `Err`) — both of those paths already called `finish_batch_ok`/
    /// `finish_batch_with_error` themselves, so disarming here just
    /// prevents this guard's `Drop` from redundantly repeating that work,
    /// not from correcting it.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for LeaderFailureGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Reached only via unwind (panic) — `run_as_leader` never returns
        // without disarming first. Same shape as `finish_batch_with_error`,
        // deliberately not a call to it: that function takes an `io::
        // ErrorKind` this call site does not have (there was no `io::Error`
        // — there was no return at all).
        {
            let mut guard = self.committer.lock_batch();
            guard.leader_active = false;
            if guard.poisoned.is_none() {
                guard.poisoned = Some(PoisonReason::LeaderPanicked);
            }
        }
        self.committer.condvar.notify_all();
    }
}

/// RAII guard for one reserved slot in `GroupCommitter::pending_waiters` —
/// see `acquire_waiter_permit`. Releases the slot on drop, including on
/// every early-return path through `await_durable` and on unwind.
struct WaiterPermit<'a> {
    committer: &'a GroupCommitter,
}

impl Drop for WaiterPermit<'_> {
    fn drop(&mut self) {
        self.committer
            .pending_waiters
            .fetch_sub(1, Ordering::AcqRel);
    }
}

/// Wraps a `FileWal` (WAL Spec §5) opened with `SyncMode::GroupCommit {
/// max_wait, max_batch_bytes }` and implements the batching algorithm those
/// fields describe. `Send + Sync` (unlike the bare `FileWal` it wraps,
/// which is deliberately `!Sync` — see `wal::mod`'s "# Safety" section):
/// that is the entire point of this type — see this module's doc comment.
pub struct GroupCommitter {
    wal: Mutex<FileWal>,
    /// Highest `seq` this committer has itself proven durable via a
    /// completed, successful leader `fsync` (or, at construction, via
    /// `FileWal`'s own recovered `next_seq()` — see `new`'s doc comment).
    /// `0` means "nothing yet" and is never a real `seq` (WAL Spec §3:
    /// `seq` starts at `1`), so no separate `Option`/"initialized" flag is
    /// needed. Monotone: only ever advanced via `fetch_max`, never
    /// decreased, never reset short of dropping this `GroupCommitter`.
    durable_through: AtomicU64,
    /// `wal.current_segment_id()` at construction — `stats()`'s
    /// `segment_rotations` is `current_segment_id() - this`, both
    /// monotonically non-decreasing, so the delta is exactly the number
    /// of rotations (explicit `rotate()` calls or `FileWal::append`'s own
    /// automatic mid-append rotation, WAL Spec §3.4 — both advance
    /// `active_id` identically) this `GroupCommitter` has observed since
    /// it was constructed, not since the WAL directory was first created.
    initial_segment_id: u64,
    batch: Mutex<BatchState>,
    condvar: Condvar,
    latency: FsyncLatencyTracker,
    /// From `SyncMode::GroupCommit.max_wait` — the leader's wait-window
    /// *cap* (the algorithm's literal "200 µs" is this value's default
    /// when a caller wants the spec's exact numbers, not a hardcoded
    /// constant) and, per `PROCESS.md` §1.11 item 1, the floor under a
    /// follower's derived `10 * EMA` timeout.
    max_wait_cap: Duration,
    /// From `SyncMode::GroupCommit.max_batch_bytes` — the "256 KB" payload
    /// threshold that lets a leader stop waiting early.
    max_batch_bytes: usize,
    /// Approximate bytes appended since the current batch opened (reset to
    /// `0` whenever a new leader is elected). A heuristic only — see
    /// `estimate_frame_len`'s doc comment for why it need not be exact.
    batch_bytes: AtomicUsize,
    /// Backpressure (§11): the number of callers currently inside
    /// `await_durable` (leader or follower), bounded by
    /// `max_pending_waiters`. See `acquire_waiter_permit`.
    pending_waiters: AtomicUsize,
    max_pending_waiters: usize,
    /// Set once by `shutdown()`, never cleared. Checked by `append` and by
    /// every iteration of `await_durable`'s wait loop so no caller — new or
    /// already waiting — can start or remain blocked on a batch that will
    /// never be allowed to begin after shutdown was requested.
    shutting_down: AtomicBool,
    /// Observability counters (§16/§24) — see `stats()`. All `Relaxed`:
    /// purely descriptive, never used to make a correctness decision.
    stat_sync_attempts: AtomicU64,
    stat_sync_successes: AtomicU64,
    stat_records_total: AtomicU64,
    stat_max_batch_records: AtomicU64,
    stat_window_wait_ns_total: AtomicU64,
    stat_window_wait_samples: AtomicU64,
    /// Test-only `fsync` fault-injection seam — see `do_leader_fsync`'s doc
    /// comment for why this exists (the leader bypasses `wal::testing::
    /// FaultInjectingIo`'s layer entirely by design) and why it is a field
    /// on *this instance*, not a process-wide global: `cargo test` runs
    /// test functions concurrently in the same process by default, so a
    /// global hook would let one test's injected fault leak into another
    /// test's unrelated `GroupCommitter` running at the same time — a real
    /// failure this crate's own test suite hit during development (see
    /// `PROCESS.md`'s milestone log) before this field replaced an earlier
    /// `static`-based version.
    #[cfg(any(test, feature = "test-util"))]
    fsync_fault_hook: Mutex<Option<FsyncFaultHook>>,
    /// Temporary, test-util-gated per-batch timing diagnostic — see
    /// `batch_timing`'s module doc comment. Zero-sized and a no-op in
    /// non-`test-util` builds.
    timing: batch_timing::BatchTiming,
}

/// See `GroupCommitter::fsync_fault_hook`'s doc comment. A type alias
/// purely to keep the field declaration and `install_fsync_fault_hook`'s
/// signature within clippy's `type_complexity` comfort zone.
#[cfg(any(test, feature = "test-util"))]
type FsyncFaultHook = Box<dyn Fn() -> io::Result<()> + Send + Sync>;

/// Manual `Debug` (rather than `#[derive(Debug)]`): `fsync_fault_hook`'s
/// `dyn Fn` cannot derive `Debug`, and printing whether one is installed
/// is more useful for a test/debug seam than printing its (impossible)
/// contents would be anyway.
impl std::fmt::Debug for GroupCommitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupCommitter")
            .field(
                "durable_through",
                &self.durable_through.load(Ordering::Relaxed),
            )
            .field("max_wait_cap", &self.max_wait_cap)
            .field("max_batch_bytes", &self.max_batch_bytes)
            .field("batch_bytes", &self.batch_bytes.load(Ordering::Relaxed))
            .field(
                "pending_waiters",
                &self.pending_waiters.load(Ordering::Relaxed),
            )
            .field("max_pending_waiters", &self.max_pending_waiters)
            .field("shutting_down", &self.shutting_down.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl GroupCommitter {
    /// Wraps `wal`, which must have been opened with `SyncMode::
    /// GroupCommit { max_wait, max_batch_bytes }` (WAL Spec §5) — returns
    /// `EngineError::Unsupported` otherwise, rather than silently batching
    /// with invented defaults a caller configured with `Immediate` never
    /// asked for (the same "never silently give different behavior than
    /// configured" principle `wal::mod`'s own doc comment already applies
    /// to `SyncMode::GroupCommit` itself).
    ///
    /// **`durable_through` starts at `wal.durable_seq()`, not `0`.** If
    /// `wal` was just returned by `FileWal::open_for_recovery`, every `seq`
    /// below its `next_seq()` was found by walking bytes already on disk —
    /// by that recovery's own contract, already durable (this process
    /// would not be able to read those bytes back after a restart if they
    /// had not survived it), so `durable_seq()` (initialized from that same
    /// `next_seq` at recovery time) correctly captures it. Starting
    /// `durable_through` at `0` instead would make `await_durable` on any
    /// already-durable `seq` from before this `GroupCommitter` existed
    /// block until some *new* batch happens to reach that high-water mark —
    /// needlessly indirect at best, and an outright hang if no further
    /// writes ever arrive. Using `wal.next_seq() - 1` instead of
    /// `wal.durable_seq()` would reintroduce a real footgun: a caller that
    /// appends to `wal` without syncing before handing it to `new` would
    /// have those genuinely-not-yet-durable records silently treated as
    /// durable. `durable_seq()` cannot make that mistake — it only ever
    /// advances inside `FileWal::sync()`/`rotate()`, after a `fsync` has
    /// actually completed (see its field doc comment in `wal::mod`).
    ///
    /// **Performs one real `fsync` before returning**, to seed
    /// `FsyncLatencyTracker` with a genuine measurement rather than
    /// leaving it at `0`. Without this, the very first batch after
    /// construction would compute a follower `wait_timeout` of `10 * 0 =
    /// 0` floored only at `max_wait_cap` (microseconds) — far shorter than
    /// a real `fsync` typically takes (this machine's own WAL benchmark:
    /// 3–10 ms for `Immediate`-mode `append_sync`, see `PROGRESS.md`),
    /// which would make a concurrent follower time out while the leader's
    /// first, legitimate `fsync` is still in flight. A one-time warm-up
    /// probe removes the need to guess a fallback constant: `new` fails
    /// (fail-closed) if this probe `fsync` itself fails, on the theory
    /// that a storage layer that cannot complete a single `fsync` at
    /// startup is not one this `GroupCommitter` should silently proceed
    /// against.
    ///
    /// Equivalent to `with_max_pending_waiters(wal, DEFAULT_MAX_PENDING_
    /// WAITERS)` — see that constructor for the backpressure bound this
    /// applies.
    pub fn new(wal: FileWal) -> Result<Self> {
        Self::with_max_pending_waiters(wal, DEFAULT_MAX_PENDING_WAITERS)
    }

    /// As `new`, but with an explicit cap on how many callers may be
    /// simultaneously inside `await_durable` (leader or follower) at once —
    /// the backpressure bound (§11): once `max_pending_waiters` callers are
    /// concurrently waiting, a new `await_durable` call fails immediately
    /// with `EngineError::CapacityExceeded` rather than queuing
    /// unboundedly or blocking for room. This bounds `GroupCommitter`'s own
    /// resource usage (OS threads blocked in `condvar.wait_timeout`) under
    /// a workload spike or a misbehaving/adversarial caller that spawns
    /// unbounded concurrent writers; it does not bound anything else,
    /// since no other part of this type's state grows with waiter count
    /// (`PROCESS.md` §1.10: no per-waiter registry exists at all).
    pub fn with_max_pending_waiters(wal: FileWal, max_pending_waiters: usize) -> Result<Self> {
        let (max_wait, max_batch_bytes) = match wal.sync_mode() {
            SyncMode::GroupCommit {
                max_wait,
                max_batch_bytes,
            } => (max_wait, max_batch_bytes),
            SyncMode::Immediate => {
                return Err(EngineError::Unsupported {
                    operation: "GroupCommitter::new requires a FileWal opened with \
                                SyncMode::GroupCommit { .. }; this one was opened with \
                                SyncMode::Immediate"
                        .to_string(),
                });
            }
        };
        let initial_durable_through = wal.durable_seq();
        let initial_segment_id = wal.current_segment_id();
        let committer = GroupCommitter {
            wal: Mutex::new(wal),
            durable_through: AtomicU64::new(initial_durable_through),
            initial_segment_id,
            batch: Mutex::new(BatchState::default()),
            condvar: Condvar::new(),
            latency: FsyncLatencyTracker::new(),
            max_wait_cap: max_wait,
            max_batch_bytes,
            batch_bytes: AtomicUsize::new(0),
            pending_waiters: AtomicUsize::new(0),
            max_pending_waiters,
            shutting_down: AtomicBool::new(false),
            stat_sync_attempts: AtomicU64::new(0),
            stat_sync_successes: AtomicU64::new(0),
            stat_records_total: AtomicU64::new(0),
            stat_max_batch_records: AtomicU64::new(0),
            stat_window_wait_ns_total: AtomicU64::new(0),
            stat_window_wait_samples: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-util"))]
            fsync_fault_hook: Mutex::new(None),
            timing: batch_timing::BatchTiming::new(),
        };
        committer.warm_up_latency_estimate()?;
        Ok(committer)
    }

    /// Installs `hook` to run in place of the real `fsync` call the next
    /// time (and every time thereafter, until `clear_fsync_fault_hook` is
    /// called) *this* `GroupCommitter` elects a leader. Scoped to this one
    /// instance — see the `fsync_fault_hook` field's doc comment for why a
    /// process-wide hook is the wrong shape. Only compiled with the
    /// `test-util` feature (or under `#[cfg(test)]`), so production builds
    /// carry no trace of this seam.
    #[cfg(any(test, feature = "test-util"))]
    pub fn install_fsync_fault_hook(
        &self,
        hook: impl Fn() -> io::Result<()> + Send + Sync + 'static,
    ) {
        let mut slot = self
            .fsync_fault_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *slot = Some(Box::new(hook));
    }

    /// Removes any hook installed by `install_fsync_fault_hook`, restoring
    /// real `fsync` behavior for this instance.
    #[cfg(any(test, feature = "test-util"))]
    pub fn clear_fsync_fault_hook(&self) {
        let mut slot = self
            .fsync_fault_hook
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *slot = None;
    }

    /// One real `fsync` on the active segment, timed and folded into
    /// `latency` — see `new`'s doc comment for why. Does not touch
    /// `durable_through` (that watermark only ever advances via a batch a
    /// caller actually asked for); a warm-up probe is not itself a
    /// caller-visible durability event, even though it happens to make
    /// everything appended so far genuinely durable as a side effect.
    fn warm_up_latency_estimate(&self) -> Result<()> {
        // `_batch_max_seq` is deliberately discarded, not folded into
        // `durable_through`: even though this probe's `fsync` genuinely
        // makes those bytes durable as a side effect, publishing that here
        // would make the warm-up observable as a durability event to a
        // caller who never asked for one. The (safe, under- rather than
        // over-report) cost is that the first real caller after `new()`
        // may wait through one more, technically redundant `fsync` before
        // `durable_through` first advances — never a correctness issue,
        // since `durable_through` only ever needs to be a lower bound.
        let (cloned_file, _batch_max_seq) = self.snapshot_sync_target()?;
        let started = Instant::now();
        self.do_leader_fsync(&cloned_file)?;
        let elapsed_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.latency.record(elapsed_ns);
        Ok(())
    }

    /// Appends `op` via the wrapped `FileWal`, returning its assigned
    /// `WalPosition` (`seq` included). Does **not** wait for durability —
    /// call `await_durable(position.seq)` for that, or use
    /// `append_durable` for the combined operation. Briefly locks the
    /// wrapped `FileWal` (memory-speed: one `pwrite`, no `fsync` — see this
    /// module's doc comment), so it is never blocked by another thread's
    /// in-flight `fsync`.
    pub fn append(&self, op: WalOp) -> Result<WalPosition> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(EngineError::Aborted {
                detail: "GroupCommitter is shutting down; new appends are rejected".to_string(),
            });
        }
        let approx_len = estimate_frame_len(&op);
        let position = {
            let mut wal = self.lock_wal();
            wal.append(op)?
        };
        self.batch_bytes.fetch_add(approx_len, Ordering::Relaxed);
        Ok(position)
    }

    /// The brief's algorithm, verbatim (WAL Spec §4's `GroupCommit`
    /// extension point, restated precisely in `PROCESS.md`'s header note):
    ///
    /// 1. If `seq` is already durable, return `Ok(())` immediately.
    /// 2. Otherwise, race for leadership of the current batch under
    ///    `batch`. The winner becomes leader (see `run_as_leader`); every
    ///    other caller becomes a follower and waits on `condvar` with a
    ///    bounded timeout, re-checking `durable_through`/`poisoned` on
    ///    every wake (spurious or real).
    /// 3. A follower that times out returns `EngineError::Timeout` — bounded,
    ///    never a hang; see `PROCESS.md` §1.10/§1.11.
    pub fn await_durable(&self, seq: u64) -> Result<()> {
        if self.durable_through.load(Ordering::Acquire) >= seq {
            return Ok(());
        }
        // Checked before acquiring a waiter permit: a caller that arrives
        // after shutdown was requested, for a `seq` that is not already
        // durable, is told immediately rather than consuming a permit slot
        // it will only ever fail out of anyway.
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(Self::shutting_down_error(seq));
        }
        // Backpressure (§11): bounded, not blocking-for-room — see
        // `acquire_waiter_permit`'s doc comment. Held for the rest of this
        // call (leader or follower) via RAII; released on every return
        // path, including early returns and panics-that-never-happen.
        let _permit = self.acquire_waiter_permit()?;

        let mut guard = self.lock_batch();
        loop {
            if self.durable_through.load(Ordering::Acquire) >= seq {
                return Ok(());
            }
            if let Some(reason) = guard.poisoned {
                return Err(Self::poisoned_error(reason));
            }
            // Re-checked every iteration (not just on entry): a follower
            // already waiting when shutdown() is called is woken by its
            // notify_all() and must observe this on its very next loop
            // iteration, not keep waiting out its full timeout.
            if self.shutting_down.load(Ordering::Acquire) {
                return Err(Self::shutting_down_error(seq));
            }

            if !guard.leader_active {
                super::fire_abort_hook(super::AbortPoint::BeforeLeader);
                guard.leader_active = true;
                self.batch_bytes.store(0, Ordering::Relaxed);
                super::fire_abort_hook(super::AbortPoint::AfterLeaderElection);
                drop(guard);
                // Armed for the whole `run_as_leader` call so a leader
                // panic (not just a returned `Err`) still clears
                // `leader_active` and poisons the committer instead of
                // wedging it forever — see `LeaderFailureGuard`'s doc
                // comment (the Phase 3 P0 fix).
                let leader_failure_guard = LeaderFailureGuard::new(self);
                let result = self.run_as_leader();
                leader_failure_guard.disarm();
                return result;
            }

            let wait_timeout = self.follower_wait_timeout();
            let (new_guard, wait_result) = self
                .condvar
                .wait_timeout(guard, wait_timeout)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = new_guard;

            if wait_result.timed_out() {
                // One last check: `notify_all` and the timeout can race
                // (the batch may have completed at essentially the same
                // instant the wait gave up) — never report a spurious
                // Timeout when the real outcome already landed.
                if self.durable_through.load(Ordering::Acquire) >= seq {
                    return Ok(());
                }
                if let Some(reason) = guard.poisoned {
                    return Err(Self::poisoned_error(reason));
                }
                return Err(EngineError::Timeout {
                    detail: format!(
                        "await_durable(seq={seq}) timed out after {wait_timeout:?} \
                         waiting for a group-commit leader to publish durable_through \
                         (durable_through={}, ema_fsync_latency_ns={})",
                        self.durable_through.load(Ordering::Acquire),
                        self.latency.current_ns(),
                    ),
                });
            }
            // Spurious or real notify: loop back to the top and re-check.
            //
            // No timing-diagnostic call here (revision 1 of `batch_
            // timing` had every waiter update a shared atomic on each
            // wake; that contended write measurably degraded the very
            // system it was trying to observe under M1.3-scale load —
            // see `batch_timing`'s module doc comment). Coordination is
            // now measured entirely on the leader side instead.
        }
    }

    /// `append` then `await_durable` on the result — the common case for a
    /// caller that just wants "durably written, batched with whoever else
    /// is concurrently writing," mirroring `Wal::append_sync`'s role for
    /// `Immediate` mode.
    pub fn append_durable(&self, op: WalOp) -> Result<WalPosition> {
        let position = self.append(op)?;
        self.await_durable(position.seq)?;
        Ok(position)
    }

    /// The current durability watermark: every `seq <= durable_through()`
    /// is provably durable (a completed, successful `fsync` covered it).
    /// May be stale the instant it's read (another thread's batch can
    /// complete immediately after) — never stale in the unsafe direction:
    /// it never reports a `seq` durable before the `fsync` that made it so
    /// has actually returned `Ok`.
    pub fn durable_through(&self) -> u64 {
        self.durable_through.load(Ordering::Acquire)
    }

    /// Delegates to the wrapped `FileWal::rotate` under the same `wal`
    /// lock `append`/the leader's snapshot use — see this module's doc
    /// comment and `PROCESS.md` §1.5 for why mid-batch rotation needs no
    /// special-case handling beyond this.
    pub fn rotate(&self) -> Result<()> {
        let mut wal = self.lock_wal();
        wal.rotate()
    }

    /// Delegates to `FileWal::purge_before` (WAL Spec §10) under the same
    /// `wal` lock `append`/the leader's snapshot use — safe to call
    /// concurrently with ongoing writes: `purge_before` never removes the
    /// currently-active segment, only sealed segments whose highest `seq`
    /// is below `watermark_seq`, so it cannot race a batch's own
    /// in-flight append or `fsync`. Added for Phase 3C's long-duration
    /// soak testing (`PHASE3C_TEST_PLAN.md`), which periodically
    /// checkpoints/truncates old, already-durable segments during a
    /// multi-hour run — mirroring how a real deployment bounds its own
    /// WAL footprint — rather than letting the WAL grow unbounded for the
    /// run's entire duration (which would make the existing, known
    /// recovery-memory limitation, `PHASE3B_ADR.md` ADR-P3B-5,
    /// unavoidable at multi-hour scale).
    pub fn purge_before(&self, watermark_seq: u64) -> Result<Vec<u64>> {
        let mut wal = self.lock_wal();
        wal.purge_before(watermark_seq)
    }

    pub fn next_seq(&self) -> u64 {
        self.lock_wal().next_seq()
    }

    pub fn current_segment_id(&self) -> u64 {
        self.lock_wal().current_segment_id()
    }

    /// Read-only access to the EMA `fsync`-latency tracker driving the
    /// leader's wait window and a follower's timeout — exposed primarily
    /// for tests/observability, not required by the write path itself.
    pub fn latency_tracker(&self) -> &FsyncLatencyTracker {
        &self.latency
    }

    /// A snapshot of batching observability counters (§16/§24) —
    /// batch/sync counts, records-per-batch, durable watermark. Cheap
    /// (a handful of `Relaxed` atomic loads); safe to call at any time,
    /// including concurrently with active writers.
    pub fn stats(&self) -> GroupCommitStats {
        let current_segment_id = self.current_segment_id();
        let highest_sequence = self.next_seq().saturating_sub(1);
        GroupCommitStats {
            sync_attempts: self.stat_sync_attempts.load(Ordering::Relaxed),
            sync_successes: self.stat_sync_successes.load(Ordering::Relaxed),
            records_total: self.stat_records_total.load(Ordering::Relaxed),
            max_batch_records: self.stat_max_batch_records.load(Ordering::Relaxed),
            window_wait_ns_total: self.stat_window_wait_ns_total.load(Ordering::Relaxed),
            window_wait_samples: self.stat_window_wait_samples.load(Ordering::Relaxed),
            durable_through: self.durable_through(),
            pending_waiters: self.pending_waiters.load(Ordering::Relaxed),
            highest_sequence,
            segment_rotations: current_segment_id.saturating_sub(self.initial_segment_id),
        }
    }

    /// Shuts this `GroupCommitter` down (§10): no new `append`/
    /// `await_durable` call started after this returns will be allowed to
    /// begin a new batch — both fail fast with `EngineError::Aborted`
    /// (unless the requested `seq` is already durable, in which case
    /// `await_durable` still reports that truthfully; shutdown does not
    /// retroactively un-durable anything). A caller already blocked in
    /// `await_durable` at the moment this is called is woken (via
    /// `notify_all`) and observes the flag on its next loop iteration,
    /// rather than waiting out its full timeout.
    ///
    /// Gives any batch that is *already* leader-active a bounded chance
    /// (`SHUTDOWN_DRAIN_BOUND`) to finish — its `fsync` cannot be
    /// interrupted mid-syscall regardless, so this only affects how long
    /// `shutdown()` itself waits before returning a settled-enough
    /// snapshot; it never blocks unconditionally (`shutdown()` itself must
    /// never hang).
    ///
    /// Returns a `ShutdownReport` explicitly stating whether any assigned
    /// `seq` remains un-synced at the moment of the snapshot (§10: "If
    /// pending writes are not durable at shutdown, the implementation must
    /// report that state explicitly") — this crate never silently drops
    /// that information.
    pub fn shutdown(&self) -> ShutdownReport {
        self.shutting_down.store(true, Ordering::Release);
        self.condvar.notify_all();

        let deadline = Instant::now() + SHUTDOWN_DRAIN_BOUND;
        let mut guard = self.lock_batch();
        while guard.leader_active {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (new_guard, _) = self
                .condvar
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = new_guard;
        }
        drop(guard);

        let durable_through = self.durable_through();
        let highest_assigned_seq = self.lock_wal().next_seq().saturating_sub(1);
        ShutdownReport {
            durable_through,
            highest_assigned_seq,
        }
    }

    /// Consumes this `GroupCommitter` and returns the wrapped `FileWal`.
    /// Never panics: a poisoned `std::sync::Mutex` (only possible if a
    /// prior critical section panicked, which this module's own code never
    /// does) is recovered rather than propagated, matching `lock_wal`'s
    /// policy — see its doc comment.
    ///
    /// Also where the Phase A timing diagnostic's report is printed (if
    /// `RGC_TIMING_REPORT` is set — see `batch_timing`'s doc comment),
    /// rather than from a `Drop` impl: `GroupCommitter` deliberately does
    /// not implement `Drop`, because this very method moves `self.wal`'s
    /// inner value out of `self` by value, which Rust forbids for a type
    /// that implements `Drop` (its destructor must be able to see every
    /// field). Calling the report here instead reaches the same "end of
    /// this committer's lifecycle" point without that conflict.
    pub fn into_inner(self) -> FileWal {
        self.timing.print_report_if_requested();
        self.wal
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs the leader side of one batch: wait for the batch window (or
    /// enough accumulated bytes) to close, snapshot a sync target, `fsync`
    /// it *outside* the `wal` lock, then publish the outcome to every
    /// waiter. Called with `batch.leader_active` already `true` (set by
    /// the caller in `await_durable` under `batch`, which this function
    /// itself never re-locks until the batch concludes).
    fn run_as_leader(&self) -> Result<()> {
        let durable_through_before_batch = self.durable_through.load(Ordering::Acquire);

        // Phase A timing diagnostic (test-util-gated, zero-cost otherwise
        // — see `batch_timing`'s doc comment): `record_batch_start`
        // attributes the gap since the *previous* batch's `notify_all`
        // to that previous batch's coordination cost — computed here,
        // leader-side only, with no follower involvement.
        let t_window_started = self.timing.now_ns();
        self.timing.record_batch_start(t_window_started);

        let window_started = Instant::now();
        self.spin_wait_for_batch_window();
        let window_elapsed_ns =
            u64::try_from(window_started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.stat_window_wait_ns_total
            .fetch_add(window_elapsed_ns, Ordering::Relaxed);
        self.stat_window_wait_samples
            .fetch_add(1, Ordering::Relaxed);
        let t_window_ended = self.timing.now_ns();

        let (cloned_file, batch_max_seq) = match self.snapshot_sync_target() {
            Ok(t) => t,
            Err(e) => {
                self.stat_sync_attempts.fetch_add(1, Ordering::Relaxed);
                self.finish_batch_with_error(Self::io_kind_of(&e));
                return Err(e);
            }
        };
        let t_snapshot_ended = self.timing.now_ns();

        // `AbortPoint::BeforeSync`/`AfterSync` (`super::AbortPoint`) are
        // fired here, not just inside `FileWal::sync()` (which this leader
        // path never calls — see this module's doc comment on why the
        // `fsync` deliberately bypasses it). Both abort points describe
        // the same conceptual moment — "immediately before/after the
        // `fsync` that makes prior appends durable" — Shape B just
        // relocates *where* that moment physically happens; a crash-
        // consistency test targeting these points (`tests/group_commit/
        // crash_consistency.rs`, M1.6) needs them reachable from whichever
        // code path actually performs the group-commit leader's `fsync`.
        // `fire_abort_hook` is a zero-cost no-op without the `test-util`
        // feature, so this costs nothing in production builds.
        super::fire_abort_hook(super::AbortPoint::BeforeSync);
        self.stat_sync_attempts.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let fsync_result = self.do_leader_fsync(&cloned_file);
        let elapsed_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let t_fsync_ended = self.timing.now_ns();

        match fsync_result {
            Ok(()) => {
                super::fire_abort_hook(super::AbortPoint::AfterSync);
                self.latency.record(elapsed_ns);
                self.durable_through
                    .fetch_max(batch_max_seq, Ordering::Release);
                super::fire_abort_hook(super::AbortPoint::AfterWatermarkBeforeWake);

                let batch_records = batch_max_seq.saturating_sub(durable_through_before_batch);
                self.stat_sync_successes.fetch_add(1, Ordering::Relaxed);
                self.stat_records_total
                    .fetch_add(batch_records, Ordering::Relaxed);
                self.stat_max_batch_records
                    .fetch_max(batch_records, Ordering::Relaxed);

                let t_notify_sent = self.timing.now_ns();
                self.timing.record_batch_stages(
                    t_window_started,
                    t_window_ended,
                    t_snapshot_ended,
                    t_fsync_ended,
                    t_notify_sent,
                );
                self.finish_batch_ok();
                Ok(())
            }
            Err(io_err) => {
                let kind = io_err.kind();
                self.finish_batch_with_error(kind);
                Err(EngineError::Io(io_err))
            }
        }
    }

    /// Briefly locks `wal` to obtain a cloned handle to the active
    /// segment plus this batch's candidate high-water `seq`
    /// (`FileWal::active_segment_sync_handle`), then releases the lock —
    /// the returned `File` is `fsync`ed with no lock held at all. See this
    /// module's doc comment for why this is safe without `unsafe`.
    fn snapshot_sync_target(&self) -> Result<(File, u64)> {
        let wal = self.lock_wal();
        let (_segment_id, file, batch_max_seq) = wal.active_segment_sync_handle()?;
        Ok((file, batch_max_seq))
    }

    /// The leader's wait window: `min(max_wait_cap, EMA / WINDOW_EMA_
    /// DIVISOR)`, but returns early the moment `batch_bytes` reaches
    /// `max_batch_bytes` (WAL Spec's group-commit extension point's own
    /// "whichever comes first" rule). Implemented as a tight poll/
    /// `spin_loop` rather than `thread::sleep` — see `PROCESS.md` §1.6: at
    /// this sub-millisecond scale, `thread::sleep`'s OS timer-resolution
    /// overshoot (particularly on Windows) would cost more than the
    /// window itself is worth amortizing `fsync` latency against. Only
    /// one thread is ever the leader at a time (enforced by `batch.
    /// leader_active`), so this spin never contends with itself.
    ///
    /// `WINDOW_EMA_DIVISOR = 1` (i.e. `EMA / 1`, the EMA itself) replaces
    /// the original `/ 10` — see its own doc comment for the sweep data
    /// this is derived from (`PHASE1_TEST_RESULTS.md`'s window-size sweep
    /// section, `PHASE1_ADR.md` ADR-12).
    ///
    /// **Two-stage, demand-adaptive wait — not just a longer flat wait.**
    /// The sweep that justified the larger `window` above was run only
    /// under real concurrent load (100/1,000 writers); re-running M1.1
    /// (a single writer, no batching partner ever) with the naively
    /// larger window regressed its median latency from ~3.1ms to ~5.8ms —
    /// a real, measured regression, not a hypothetical one (`PHASE1_TEST_
    /// RESULTS.md`'s window-size sweep section records it). The root
    /// cause: the EMA/divisor formula encodes *`fsync` latency*, but
    /// nothing about whether any other caller is actually going to join
    /// this batch — a lone writer waiting a multi-millisecond window before
    /// its own `fsync` pays that latency for zero batching benefit.
    ///
    /// The fix waits only `PROBE_WINDOW` (the *original* default, ~200µs)
    /// before checking whether `batch_bytes` — reset to `0` at leader
    /// election (`await_durable`), so any nonzero value here can only be
    /// *another* caller's `append()` — shows any follower activity at all.
    /// If none has appeared by then, the batch is (so far) just the
    /// leader's own record; there is nothing to gain from waiting the rest
    /// of `window`, so it stops immediately. If a follower *has* joined,
    /// the wait extends up to the full `window`, exactly as the sweep
    /// data justifies — and empirically (same sweep data), under genuine
    /// 100–1,000-writer contention a follower reliably joins well within
    /// the first 200µs, so this probe essentially never shortens a batch
    /// that real contention would have grown.
    fn spin_wait_for_batch_window(&self) {
        super::fire_abort_hook(super::AbortPoint::DuringBatchWaitPre);
        let ema_ns = self.latency.current_ns();
        #[cfg(feature = "phase1-window-experiment")]
        let window = phase1_window_experiment::effective_window(self.max_wait_cap, ema_ns);
        #[cfg(not(feature = "phase1-window-experiment"))]
        let window = self
            .max_wait_cap
            .min(Duration::from_nanos(ema_ns / WINDOW_EMA_DIVISOR));
        if window.is_zero() {
            super::fire_abort_hook(super::AbortPoint::DuringBatchWaitPost);
            return;
        }
        let deadline = Instant::now() + window;
        let probe_deadline = Instant::now() + PROBE_WINDOW.min(window);
        // Yield the CPU periodically rather than spinning unconditionally
        // for the whole window: at the algorithm's own default (200 µs
        // cap), a pure spin costs at most ~200 µs of one core regardless
        // (already small), but `max_wait_cap` is caller-configurable
        // (`SyncMode::GroupCommit.max_wait`, WAL Spec §5) — nothing stops a
        // caller from configuring a window in the low milliseconds, at
        // which point unconditional spinning would burn a full core for
        // that entire duration on every single batch. `yield_now()` every
        // `YIELD_EVERY` iterations gives the OS scheduler a chance to run
        // other ready threads (in particular, other `append()` callers
        // trying to grow this very batch) without meaningfully coarsening
        // the wait granularity at the sub-millisecond scale this window
        // normally runs at.
        //
        // **`phase1-waitmode-experiment` (FINAL_WAL_TEST.md's spin-wait
        // A/B):** the paragraph above justified spin+periodic-yield
        // against a ~200µs window, where the cost of *any* wait mechanism
        // is small in absolute terms. The production window is now
        // milliseconds (`PHASE1_ADR.md` ADR-12), which is large enough
        // that a sleep-based wait's coarser granularity might no longer
        // cost more than the CPU/scheduling overhead of spinning for the
        // whole window saves — an empirical question, not decided here.
        // `wait_step` below is the *only* thing this experiment changes;
        // reduces to the exact spin/yield behavior above, unconditionally,
        // whenever the feature is off or its env var is unset/invalid.
        #[cfg(feature = "phase1-waitmode-experiment")]
        let wait_mode = phase1_waitmode_experiment::effective_wait_mode();
        let mut iterations: u32 = 0;
        loop {
            if self.batch_bytes.load(Ordering::Relaxed) >= self.max_batch_bytes {
                super::fire_abort_hook(super::AbortPoint::DuringBatchWaitPost);
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                super::fire_abort_hook(super::AbortPoint::DuringBatchWaitPost);
                return;
            }
            if now >= probe_deadline && self.batch_bytes.load(Ordering::Relaxed) == 0 {
                super::fire_abort_hook(super::AbortPoint::DuringBatchWaitPost);
                return;
            }
            iterations += 1;
            #[cfg(feature = "phase1-waitmode-experiment")]
            phase1_waitmode_experiment::wait_step(wait_mode, iterations, now, probe_deadline);
            #[cfg(not(feature = "phase1-waitmode-experiment"))]
            wait_step_production(iterations);
        }
    }

    /// A follower's `condvar.wait_timeout` bound: `10 * EMA`, floored at
    /// `max_wait_cap`. The floor is a general invariant, not merely a
    /// cold-start patch: even though `GroupCommitter::new`'s warm-up probe
    /// (see its doc comment) guarantees the EMA is never literally `0` by
    /// the time any caller can reach `await_durable`, a *fast* warm-up
    /// sample (e.g. an SSD/NVMe measuring tens of microseconds) can still
    /// make `10 * EMA` smaller than `max_wait_cap` itself — and a follower
    /// must never be given less time to wait than the leader's own maximum
    /// decision window, regardless of how quickly recent `fsync` calls
    /// happened to measure.
    fn follower_wait_timeout(&self) -> Duration {
        let ema_ns = self.latency.current_ns();
        let derived = Duration::from_nanos(ema_ns.saturating_mul(10));
        derived.max(self.max_wait_cap)
    }

    /// The actual `fsync` call, with a test-only interception seam
    /// (`fsync_fault_hook`) — mirrors this crate's existing `file_io::
    /// DirFsyncHook`/`AbortPoint` pattern: a small, explicit, test-only
    /// seam rather than routing production code through a generic/dynamic
    /// abstraction it doesn't otherwise need. This exists because the
    /// leader deliberately calls `sync_all()` directly on a cloned
    /// `std::fs::File` (see this module's doc comment), bypassing the
    /// `SegmentIo`/`WalFile` layer `wal::testing::FaultInjectingIo`
    /// intercepts — so that harness cannot reach this call, and this one
    /// exists in its place, scoped to exactly this call site (and, unlike
    /// a global hook, scoped to `self` — see `fsync_fault_hook`'s doc
    /// comment).
    fn do_leader_fsync(&self, file: &File) -> io::Result<()> {
        #[cfg(any(test, feature = "test-util"))]
        {
            let hook = self
                .fsync_fault_hook
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(f) = hook.as_ref() {
                return f();
            }
        }
        file.sync_all()
    }

    fn finish_batch_ok(&self) {
        {
            let mut guard = self.lock_batch();
            guard.leader_active = false;
        }
        self.condvar.notify_all();
    }

    /// Sets `poisoned` (permanently — never cleared by any later
    /// successful batch, per the algorithm) and wakes every waiter so none
    /// of them can be left blocked on a batch that will never complete.
    fn finish_batch_with_error(&self, kind: io::ErrorKind) {
        {
            let mut guard = self.lock_batch();
            guard.leader_active = false;
            guard.poisoned = Some(PoisonReason::FsyncFailed(kind));
        }
        self.condvar.notify_all();
    }

    fn poisoned_error(reason: PoisonReason) -> EngineError {
        match reason {
            PoisonReason::FsyncFailed(kind) => EngineError::Io(io::Error::new(
                kind,
                "group-commit leader's fsync failed; this GroupCommitter is \
                 poisoned and must be discarded (drop it and open a fresh one) \
                 — see PROCESS.md §1",
            )),
            PoisonReason::LeaderPanicked => EngineError::Io(io::Error::other(
                "group-commit leader thread panicked before it could complete this batch \
                 (the fsync outcome is unknown); this GroupCommitter is poisoned and must be \
                 discarded (drop it and open a fresh one, which re-scans the WAL from disk) \
                 — see PHASE3_FAILURE_MODEL.md",
            )),
        }
    }

    /// `true` once this `GroupCommitter` has been poisoned (an `fsync`
    /// failure or a leader panic) — see `PoisonReason`. Exposed for
    /// observability/tests; every caller-visible effect of poisoning is
    /// already reachable via `await_durable`'s own `Err`, so nothing in
    /// this module's own correctness depends on this accessor.
    pub fn is_poisoned(&self) -> bool {
        self.lock_batch().poisoned.is_some()
    }

    fn io_kind_of(err: &EngineError) -> io::ErrorKind {
        match err {
            EngineError::Io(e) => e.kind(),
            _ => io::ErrorKind::Other,
        }
    }

    fn shutting_down_error(seq: u64) -> EngineError {
        EngineError::Aborted {
            detail: format!(
                "GroupCommitter is shutting down; seq={seq} was not yet durable \
                 and no new batch will be started to make it so"
            ),
        }
    }

    /// Backpressure (§11): reserves one of `max_pending_waiters` slots for
    /// the duration of the returned guard, or fails immediately
    /// (`EngineError::CapacityExceeded`) if none are free — never blocks
    /// waiting for a slot to open up, which would just relocate the
    /// unbounded-queuing problem this exists to prevent from "waiters
    /// blocked in `await_durable`'s main loop" to "waiters blocked trying
    /// to enter it."
    fn acquire_waiter_permit(&self) -> Result<WaiterPermit<'_>> {
        let previous = self.pending_waiters.fetch_add(1, Ordering::AcqRel);
        if previous >= self.max_pending_waiters {
            self.pending_waiters.fetch_sub(1, Ordering::AcqRel);
            return Err(EngineError::CapacityExceeded {
                requested: (previous as u64).saturating_add(1),
                max: self.max_pending_waiters as u64,
            });
        }
        Ok(WaiterPermit { committer: self })
    }

    /// Never panics: a poisoned `std::sync::Mutex` (only reachable if a
    /// prior critical section panicked — none of this module's own code
    /// does) is recovered rather than propagated. This is a deliberate,
    /// narrow use of `PoisonError::into_inner`, not a `.unwrap()` on
    /// untrusted data: every operation performed while holding `wal` in
    /// this module returns `Result` and never panics, so recovering here
    /// trades a theoretical second-order failure mode (continuing after an
    /// unrelated bug elsewhere panicked mid-critical-section) for the
    /// practical one this crate's Non-Negotiable rules forbid outright
    /// (`.unwrap()` that can actually panic in a reachable path).
    ///
    /// **Tried and reverted: spinning on `try_lock` before falling back to
    /// a blocking `lock()`.** The theory (this lock's critical section is
    /// tiny, so a contended blocking `lock()`'s OS-level park/wake cost
    /// should dominate over just re-trying) measured *worse* under M1.2's
    /// real 100-thread load on this machine (throughput dropped from
    /// ~10,100 to ~4,500 ops/sec) — this environment has meaningfully
    /// fewer logical cores than concurrently-runnable threads, so 100
    /// threads burning CPU on `spin_loop()` starves whichever thread
    /// actually holds the lock (and the leader's own spin-wait) of the
    /// scheduler time it needs to finish and release it, a well-known
    /// failure mode of spinlocks under CPU oversubscription. Reverted;
    /// kept here as a documented negative result (`PROCESS.md`'s M1.2
    /// benchmark entry) rather than silently dropped — the *next* thing to
    /// try is reducing how much work happens per contended acquisition,
    /// not how the lock itself is acquired.
    fn lock_wal(&self) -> MutexGuard<'_, FileWal> {
        self.wal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_batch(&self) -> MutexGuard<'_, BatchState> {
        self.batch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// **Temporary, experiment-only scaffolding — see `PHASE1_ADR.md` (the
/// window-size sweep ADR) and `PHASE1_TEST_RESULTS.md`'s window-size
/// sweep section.** Only compiled with the `phase1-window-experiment`
/// Cargo feature (not enabled by default, not depended on by any other
/// feature) — a normal build or `cargo test` never sees this module at
/// all. Its one purpose is to answer, empirically, whether M1.2/M1.3's
/// throughput miss is caused by the leader's batch-window formula being
/// too conservative or by this machine's `fsync` latency being the
/// binding constraint independent of window size — a question the
/// original report's algebraic argument did not, on its own, settle.
///
/// **Fully removable**: delete this module and revert `spin_wait_for_
/// batch_window`'s one `#[cfg(feature = "phase1-window-experiment")]`
/// branch to leave only the `#[cfg(not(...))]` branch (the production
/// formula, unchanged) — a no-op relative to production behavior, since
/// `effective_window` computes exactly that same formula whenever both
/// env vars below are unset.
#[cfg(feature = "phase1-window-experiment")]
mod phase1_window_experiment {
    use std::time::Duration;

    /// `PHASE1_EXPERIMENT_MAX_WAIT_US` (microseconds, parsed as `u64`):
    /// overrides `max_wait_cap` for this process. Unset or unparseable ⇒
    /// falls back to the real `max_wait_cap` (the production value).
    ///
    /// `PHASE1_EXPERIMENT_EMA_DIVISOR` (parsed as `u64`): overrides the
    /// production `super::WINDOW_EMA_DIVISOR` applied to the EMA. `0` is a
    /// sentinel meaning "no EMA cap at all" — the window becomes exactly
    /// `max_wait`, unconditionally (models "uncap both" sweep rows).
    /// Unset or unparseable ⇒ falls back to `super::WINDOW_EMA_DIVISOR`
    /// (the production value) — kept as one shared constant, not
    /// duplicated here, so this fallback can never silently drift out of
    /// sync with the real production formula the way an independently
    /// hardcoded divisor once did (caught by `cargo clippy --all-features`
    /// flagging the constant as unused once this module stopped
    /// referencing it — see `PHASE1_TEST_RESULTS.md`'s window-size sweep
    /// section for the full account).
    pub(super) fn effective_window(max_wait_cap: Duration, ema_ns: u64) -> Duration {
        let max_wait = std::env::var("PHASE1_EXPERIMENT_MAX_WAIT_US")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_micros)
            .unwrap_or(max_wait_cap);
        let divisor = std::env::var("PHASE1_EXPERIMENT_EMA_DIVISOR")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        match divisor {
            Some(0) => max_wait,
            Some(d) => max_wait.min(Duration::from_nanos(ema_ns / d)),
            _ => max_wait.min(Duration::from_nanos(ema_ns / super::WINDOW_EMA_DIVISOR)),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // `std::env::set_var`/`remove_var` mutate process-global state,
        // not per-thread state — Rust's default parallel test runner
        // would let two tests that each set/clear these env vars race
        // each other (the exact class of bug this crate already hit once
        // with a process-wide fault-injection hook — see `PROCESS.md`'s
        // M0 entry). Both scenarios are therefore one test function, run
        // strictly sequentially within it, rather than two separate
        // `#[test]` functions that could interleave.
        #[test]
        fn effective_window_matches_env_var_overrides_or_falls_back_to_production_formula() {
            std::env::remove_var("PHASE1_EXPERIMENT_MAX_WAIT_US");
            std::env::remove_var("PHASE1_EXPERIMENT_EMA_DIVISOR");
            let cap = Duration::from_micros(200);
            let ema_ns = 2_800_000u64;
            assert_eq!(
                effective_window(cap, ema_ns),
                cap.min(Duration::from_nanos(
                    ema_ns / super::super::WINDOW_EMA_DIVISOR
                )),
                "unset env vars must reduce to the exact production formula"
            );

            std::env::set_var("PHASE1_EXPERIMENT_MAX_WAIT_US", "3000");
            std::env::set_var("PHASE1_EXPERIMENT_EMA_DIVISOR", "0");
            assert_eq!(
                effective_window(cap, ema_ns),
                Duration::from_micros(3000),
                "divisor=0 must mean 'no EMA cap': window = max_wait exactly"
            );

            std::env::remove_var("PHASE1_EXPERIMENT_MAX_WAIT_US");
            std::env::remove_var("PHASE1_EXPERIMENT_EMA_DIVISOR");
        }
    }
}

/// **Experiment-only — see `FINAL_WAL_TEST.md`'s spin-wait A/B section.**
/// Only compiled with the `phase1-waitmode-experiment` Cargo feature (not
/// enabled by default, not depended on by any other feature) — a normal
/// build or `cargo test` never sees this module. Its one purpose is to
/// answer, empirically, whether `spin_wait_for_batch_window`'s spin+
/// periodic-yield design — originally justified against a ~200µs window
/// (`PHASE1_ADR.md` ADR-12) — is still the right choice now that the
/// production window is milliseconds, by measuring two alternatives
/// against it under identical benchmark conditions.
///
/// **Fully removable**: delete this module, delete `wait_step`'s one
/// `#[cfg(feature = "phase1-waitmode-experiment")]` call site in `spin_
/// wait_for_batch_window`, and the `#[cfg_attr(..., allow(dead_code))]`
/// on `wait_step_production` — a no-op relative to production behavior,
/// since `Spin` mode below calls that exact same function.
#[cfg(feature = "phase1-waitmode-experiment")]
mod phase1_waitmode_experiment {
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum WaitMode {
        /// Reduces to exactly `wait_step_production` — the production
        /// behavior, unconditionally, whenever the env var is unset,
        /// unparseable, or explicitly `"spin"`.
        Spin,
        /// `thread::sleep(quantum)` every iteration instead of spinning —
        /// tests whether Windows' coarser sleep-timer granularity costs
        /// more than spinning saves, now that the window itself is
        /// milliseconds rather than sub-millisecond.
        Sleep,
        /// `Spin` through the demand-adaptive probe phase (mirrors the
        /// production algorithm's own early-decision point — no follower
        /// yet by `probe_deadline` means the batch is likely just the
        /// leader's own record, where spin's lower latency matters most),
        /// then `Sleep` for the remainder once the batch has committed to
        /// the full window.
        Hybrid,
    }

    /// `PHASE1_EXPERIMENT_WAIT_MODE`: `"spin"` (default/fallback,
    /// unset/unparseable also fall back here), `"sleep"`, or `"hybrid"`.
    pub(super) fn effective_wait_mode() -> WaitMode {
        match std::env::var("PHASE1_EXPERIMENT_WAIT_MODE").as_deref() {
            Ok("sleep") => WaitMode::Sleep,
            Ok("hybrid") => WaitMode::Hybrid,
            _ => WaitMode::Spin,
        }
    }

    /// `PHASE1_EXPERIMENT_SLEEP_QUANTUM_US` (microseconds, parsed as
    /// `u64`): the `thread::sleep` duration `Sleep`/`Hybrid` modes use per
    /// iteration. Unset or unparseable ⇒ `200µs` (matches `PROBE_WINDOW`,
    /// a reasonable a priori granularity — swept independently in
    /// `FINAL_WAL_TEST.md` rather than hardcoded blindly).
    fn sleep_quantum() -> Duration {
        std::env::var("PHASE1_EXPERIMENT_SLEEP_QUANTUM_US")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_micros)
            .unwrap_or(Duration::from_micros(200))
    }

    /// One wait-loop iteration's action, dispatched on `mode`. `now` and
    /// `probe_deadline` are the caller's own already-computed values (no
    /// extra `Instant::now()` call here) — `Hybrid` compares them to
    /// decide which phase it is in.
    pub(super) fn wait_step(
        mode: WaitMode,
        iterations: u32,
        now: Instant,
        probe_deadline: Instant,
    ) {
        match mode {
            WaitMode::Spin => super::wait_step_production(iterations),
            WaitMode::Sleep => std::thread::sleep(sleep_quantum()),
            WaitMode::Hybrid => {
                if now < probe_deadline {
                    super::wait_step_production(iterations);
                } else {
                    std::thread::sleep(sleep_quantum());
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // Each test below only ever sets/clears its own distinct env var
        // name, so — unlike `phase1_window_experiment`'s tests, which
        // share two vars across one scenario — there is no cross-test
        // race to guard against here even under Rust's default parallel
        // test runner; each still cleans up its own var when done, purely
        // so a later, unrelated test run never inherits a stale value.
        #[test]
        fn effective_wait_mode_matches_env_var_or_falls_back_to_spin() {
            std::env::remove_var("PHASE1_EXPERIMENT_WAIT_MODE");
            assert_eq!(effective_wait_mode(), WaitMode::Spin);

            std::env::set_var("PHASE1_EXPERIMENT_WAIT_MODE", "sleep");
            assert_eq!(effective_wait_mode(), WaitMode::Sleep);

            std::env::set_var("PHASE1_EXPERIMENT_WAIT_MODE", "hybrid");
            assert_eq!(effective_wait_mode(), WaitMode::Hybrid);

            std::env::set_var("PHASE1_EXPERIMENT_WAIT_MODE", "garbage");
            assert_eq!(effective_wait_mode(), WaitMode::Spin);

            std::env::remove_var("PHASE1_EXPERIMENT_WAIT_MODE");
        }

        #[test]
        fn sleep_quantum_matches_env_var_or_falls_back_to_200us() {
            std::env::remove_var("PHASE1_EXPERIMENT_SLEEP_QUANTUM_US");
            assert_eq!(sleep_quantum(), Duration::from_micros(200));

            std::env::set_var("PHASE1_EXPERIMENT_SLEEP_QUANTUM_US", "500");
            assert_eq!(sleep_quantum(), Duration::from_micros(500));

            std::env::remove_var("PHASE1_EXPERIMENT_SLEEP_QUANTUM_US");
        }
    }
}

/// **Temporary, test-util-gated diagnostic scaffolding — see the
/// pipelining-fix task's "Phase A — Measure" requirement and `PHASE1_TEST_
/// RESULTS.md`'s window-size sweep section (new per-batch timing
/// subsection). Not part of the stable API.** Records, per batch, when
/// the leader enters/leaves each stage (window wait, sync-target
/// snapshot, `fsync`, notify), so the batch cycle can be broken down into
/// `window + snapshot + fsync + coordination` instead of only ever being
/// visible as one opaque total.
///
/// **Revision 2 — the original design measured a real confound, not the
/// system.** The first version additionally had every *follower* call a
/// `fetch_max` on a shared `AtomicU64` each time it woke from `condvar.
/// wait_timeout`, to sample "when did the last waiter wake" for a
/// `notify`/`wake` split. Under M1.3-scale load (up to 1,000 threads),
/// that contended atomic — every wake on every thread forcing a cache-
/// line invalidation visible to every core — measurably degraded the
/// system it was trying to observe: M1.3 throughput with this
/// instrumentation compiled in measured ~9,800–11,100 ops/sec, against
/// ~62,000–63,000 ops/sec for the *identical* code and environment
/// without it (`PHASE1_TEST_RESULTS.md` §9C's revision note has the full
/// account — this was initially, incorrectly, attributed to a near-full
/// disk that turned out to be a real but secondary factor). Revision 2
/// removes all follower-side instrumentation entirely: `coordination` is
/// now computed purely from **leader-side** data — `t_window_started` of
/// batch N+1 minus `t_notify_sent` of batch N — which requires only a
/// plain atomic load/store pair, written by whichever single thread is
/// leader at the time (never more than one), so there is no meaningful
/// contention left to distort the measurement. The trade-off is losing
/// the `notify`-vs-`wake` sub-split from revision 1; `coordination` is
/// reported as one number instead, which is what the pipelining model
/// actually needs (`window + fsync + coordination`).
///
/// Two implementations, selected by `#[cfg]`, both with the identical
/// method surface — call sites throughout `GroupCommitter` never need
/// their own `#[cfg(...)]` gating, matching this crate's existing
/// `fire_abort_hook` pattern. The `test-util` build actually records and
/// can print a report (env-var-gated: `RGC_TIMING_REPORT`, checked once
/// from `GroupCommitter::into_inner`, not `Drop` — a real `Drop` impl
/// would conflict with `into_inner`'s existing `self.wal.into_inner()`
/// partial move, since a type that implements `Drop` cannot have a field
/// moved out of it by value). The non-`test-util` build is entirely
/// zero-cost: every method is an empty/constant-returning no-op that
/// optimizes away completely.
#[cfg(any(test, feature = "test-util"))]
mod batch_timing {
    use std::env;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    #[derive(Debug)]
    pub(super) struct BatchTiming {
        epoch: Instant,
        batches: AtomicU64,
        window_ns_total: AtomicU64,
        snapshot_ns_total: AtomicU64,
        fsync_ns_total: AtomicU64,
        /// Number of batches for which a *previous* batch existed to
        /// measure the inter-batch coordination gap against (every batch
        /// except the first).
        coordination_samples: AtomicU64,
        coordination_ns_total: AtomicU64,
        /// Written only by whichever thread is *currently* leader —
        /// never concurrently, since leadership is exclusive — so these
        /// two fields carry no meaningful write contention despite being
        /// shared state.
        prev_notify_sent_ns: AtomicU64,
    }

    impl BatchTiming {
        pub(super) fn new() -> Self {
            BatchTiming {
                epoch: Instant::now(),
                batches: AtomicU64::new(0),
                window_ns_total: AtomicU64::new(0),
                snapshot_ns_total: AtomicU64::new(0),
                fsync_ns_total: AtomicU64::new(0),
                coordination_samples: AtomicU64::new(0),
                coordination_ns_total: AtomicU64::new(0),
                prev_notify_sent_ns: AtomicU64::new(0),
            }
        }

        pub(super) fn now_ns(&self) -> u64 {
            u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
        }

        /// Called at the very start of `run_as_leader`, before the window
        /// wait begins. Attributes the gap since the *previous* batch
        /// called `notify_all` (i.e. finished) to that previous batch's
        /// coordination cost — purely from leader-side data, no follower
        /// involvement, no contended atomic (see this module's revision-2
        /// doc comment for why that matters).
        pub(super) fn record_batch_start(&self, t_window_started: u64) {
            let prev_notify_sent = self.prev_notify_sent_ns.load(Ordering::Relaxed);
            if prev_notify_sent == 0 {
                return; // first batch: nothing to compare against yet
            }
            self.coordination_ns_total.fetch_add(
                t_window_started.saturating_sub(prev_notify_sent),
                Ordering::Relaxed,
            );
            self.coordination_samples.fetch_add(1, Ordering::Relaxed);
        }

        /// Called once a batch's `fsync` has succeeded, immediately before
        /// `finish_batch_ok` is invoked.
        pub(super) fn record_batch_stages(
            &self,
            t_window_started: u64,
            t_window_ended: u64,
            t_snapshot_ended: u64,
            t_fsync_ended: u64,
            t_notify_sent: u64,
        ) {
            self.window_ns_total.fetch_add(
                t_window_ended.saturating_sub(t_window_started),
                Ordering::Relaxed,
            );
            self.snapshot_ns_total.fetch_add(
                t_snapshot_ended.saturating_sub(t_window_ended),
                Ordering::Relaxed,
            );
            self.fsync_ns_total.fetch_add(
                t_fsync_ended.saturating_sub(t_snapshot_ended),
                Ordering::Relaxed,
            );
            self.batches.fetch_add(1, Ordering::Relaxed);
            self.prev_notify_sent_ns
                .store(t_notify_sent, Ordering::Relaxed);
        }

        /// Prints a one-line aggregate report to stderr if `RGC_TIMING_
        /// REPORT` is set in the environment (any value) — checked once,
        /// from `GroupCommitter::into_inner`. A no-op otherwise.
        pub(super) fn print_report_if_requested(&self) {
            if env::var_os("RGC_TIMING_REPORT").is_none() {
                return;
            }
            let batches = self.batches.load(Ordering::Relaxed);
            if batches == 0 {
                eprintln!("[RGC_TIMING_REPORT] no batches recorded");
                return;
            }
            let coordination_samples = self.coordination_samples.load(Ordering::Relaxed).max(1);
            let mean_us = |total_ns: u64, n: u64| (total_ns as f64) / (n.max(1) as f64) / 1000.0;
            eprintln!(
                "[RGC_TIMING_REPORT] batches={batches} \
                 mean_window_us={:.1} mean_snapshot_us={:.1} mean_fsync_us={:.1} \
                 mean_coordination_us={:.1} coordination_samples={coordination_samples}",
                mean_us(self.window_ns_total.load(Ordering::Relaxed), batches),
                mean_us(self.snapshot_ns_total.load(Ordering::Relaxed), batches),
                mean_us(self.fsync_ns_total.load(Ordering::Relaxed), batches),
                mean_us(
                    self.coordination_ns_total.load(Ordering::Relaxed),
                    coordination_samples
                ),
            );
        }
    }
}

#[cfg(not(any(test, feature = "test-util")))]
mod batch_timing {
    #[derive(Debug, Default)]
    pub(super) struct BatchTiming;

    impl BatchTiming {
        pub(super) fn new() -> Self {
            BatchTiming
        }
        pub(super) fn now_ns(&self) -> u64 {
            0
        }
        pub(super) fn record_batch_start(&self, _t_window_started: u64) {}
        pub(super) fn record_batch_stages(&self, _: u64, _: u64, _: u64, _: u64, _: u64) {}
        pub(super) fn print_report_if_requested(&self) {}
    }
}

/// An exact accounting of one `WalOp`'s encoded on-disk frame length,
/// mirroring `format`/`ops`'s layout (`length:u32, crc32c:u32, seq:u64,
/// op:u8, op_body`) without depending on their private bounds-checking —
/// used only to decide when a batch has accumulated "enough" bytes to stop
/// waiting early (WAL Spec's group-commit "or until batch payload reaches
/// 256 KB" rule). Not required to be exact for correctness (durability is
/// always derived from `FileWal::next_seq()`, never from this counter) —
/// it happens to be exact here because the layout is simple and stable,
/// not because exactness is load-bearing.
/// `pub(crate)`, not private: also used by `execution::write_pool` to
/// account queued-request bytes against `WriteWorkerPoolConfig::max_
/// queued_bytes` using the exact same formula this module uses for its
/// own `batch_bytes`/`max_batch_bytes` accounting, rather than a second,
/// independently-maintained estimate that could silently drift from this
/// one.
pub(crate) fn estimate_frame_len(op: &WalOp<'_>) -> usize {
    const FRAME_HEADER_LEN: usize = 8; // length:u32 LE + crc32c:u32 LE
    const SEQ_AND_OP_TAG_LEN: usize = 9; // seq:u64 LE + op:u8
    let op_body_len = match op {
        WalOp::Put { key, value } => 4 + key.len() + 4 + value.len(),
        WalOp::Delete { key } => 4 + key.len(),
        WalOp::CheckpointMarker { .. } => 8,
    };
    FRAME_HEADER_LEN + SEQ_AND_OP_TAG_LEN + op_body_len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::{WalConfig, WalOpOwned};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64 as StdAtomicU64, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: StdAtomicU64 = StdAtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let path = std::env::temp_dir().join(format!("rubixdb_group_commit_{tag}_{nanos}_{n}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn group_commit_config() -> WalConfig {
        WalConfig {
            sync_mode: SyncMode::GroupCommit {
                // 5ms, not the original 200µs — see WINDOW_EMA_DIVISOR's
                // doc comment and PHASE1_TEST_RESULTS.md's window-size
                // sweep section for why.
                max_wait: Duration::from_millis(5),
                max_batch_bytes: 256 * 1024,
            },
            ..WalConfig::default()
        }
    }

    /// `EngineError::Timeout` is a *recoverable* outcome of
    /// `await_durable` — it means "this bounded wait expired," not "this
    /// record failed" (durability itself is never in question: a leader
    /// batch that hasn't finished yet is not a leader batch that failed).
    /// Under real thread-scheduling contention (many threads, a shared/
    /// virtualized CI-like machine, a busy dev box), a single
    /// `condvar.wait_timeout` call can legitimately expire without a
    /// notify landing in that exact window even though the system is
    /// making steady forward progress — the correct response, exactly
    /// like a real caller's, is to call `await_durable` again for the same
    /// `seq` (never to re-`append`, which would assign a *new* `seq`).
    /// Used by tests that need many concurrent `append_durable`-shaped
    /// calls to all eventually succeed.
    ///
    /// Bounded at `MAX_RETRIES` (not an unconditional `loop`): an
    /// unbounded retry-on-timeout would turn a genuine regression (the
    /// committer wedged for some other reason) into a hung test process
    /// rather than a clean, fast test failure — exactly the "test fails"
    /// vs. "CI hangs until the job-level timeout" distinction this crate's
    /// own fail-closed philosophy argues against papering over.
    fn await_durable_retrying_on_timeout(committer: &GroupCommitter, seq: u64) {
        const MAX_RETRIES: u32 = 1_000;
        let mut last_timeout_detail = String::new();
        for _ in 0..MAX_RETRIES {
            match committer.await_durable(seq) {
                Ok(()) => return,
                Err(EngineError::Timeout { detail }) => last_timeout_detail = detail,
                Err(e) => panic!("unexpected error awaiting durability for seq={seq}: {e}"),
            }
        }
        panic!(
            "await_durable(seq={seq}) still not satisfied after {MAX_RETRIES} retries; \
             last timeout: {last_timeout_detail}"
        );
    }

    static_assertions::assert_impl_all!(GroupCommitter: Send, Sync);

    /// §11 backpressure: once `max_pending_waiters` callers are
    /// concurrently inside `await_durable`, a new call fails immediately
    /// with `CapacityExceeded` rather than blocking or queuing. Uses
    /// `install_fsync_fault_hook` to hold the one permitted waiter's
    /// `fsync` open for a controlled duration, making the race
    /// deterministic rather than timing-dependent.
    #[test]
    fn backpressure_rejects_beyond_max_pending_waiters() {
        let dir = temp_dir("backpressure");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = Arc::new(GroupCommitter::with_max_pending_waiters(wal, 1).unwrap());

        committer.install_fsync_fault_hook(|| {
            thread::sleep(Duration::from_millis(300));
            Ok(())
        });

        let pos = committer
            .append(WalOp::Put {
                key: b"k1",
                value: b"v1",
            })
            .unwrap();
        let waiter_committer = Arc::clone(&committer);
        let handle = thread::spawn(move || waiter_committer.await_durable(pos.seq));
        // Give the spawned thread time to enter await_durable and hold the
        // one permitted waiter slot (well under the 300ms fault-hook delay
        // above, so this is not a tight race).
        thread::sleep(Duration::from_millis(50));

        let pos2 = committer
            .append(WalOp::Put {
                key: b"k2",
                value: b"v2",
            })
            .unwrap();
        let err = committer.await_durable(pos2.seq).unwrap_err();
        assert!(
            matches!(err, EngineError::CapacityExceeded { .. }),
            "expected CapacityExceeded, got {err:?}"
        );

        let first_result = handle.join().unwrap();
        assert!(
            first_result.is_ok(),
            "the one permitted waiter must still succeed"
        );

        committer.clear_fsync_fault_hook();
        let _ = fs::remove_dir_all(&dir);
    }

    /// §10 shutdown: no new batch starts after `shutdown()`, already-
    /// assigned-but-possibly-not-yet-durable state is reported explicitly
    /// (not silently dropped), and no caller is left blocked.
    #[test]
    fn shutdown_prevents_new_batches_and_reports_pending_state() {
        let dir = temp_dir("shutdown_report");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = GroupCommitter::new(wal).unwrap();

        let pos1 = committer
            .append_durable(WalOp::Put {
                key: b"k1",
                value: b"v1",
            })
            .unwrap();
        let pos2 = committer
            .append(WalOp::Put {
                key: b"k2",
                value: b"v2",
            })
            .unwrap();

        let report = committer.shutdown();
        assert!(report.durable_through >= pos1.seq);
        assert_eq!(report.highest_assigned_seq, pos2.seq);
        assert!(report.highest_assigned_seq >= report.durable_through);

        let append_err = committer
            .append(WalOp::Put {
                key: b"k3",
                value: b"v3",
            })
            .unwrap_err();
        assert!(matches!(append_err, EngineError::Aborted { .. }));

        // A seq that can never become durable (nothing beyond pos2 was
        // ever appended) must fail fast after shutdown, not hang.
        let never_appended_seq = pos2.seq + 1000;
        let await_err = committer.await_durable(never_appended_seq).unwrap_err();
        assert!(matches!(await_err, EngineError::Aborted { .. }));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `stats()` reflects real batching activity — not asserting exact
    /// counts (timing-dependent how many batches 20 sequential calls
    /// happen to produce), only that the counters move in the expected
    /// direction and stay internally consistent.
    #[test]
    fn stats_reflect_real_batching_activity() {
        let dir = temp_dir("stats");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = GroupCommitter::new(wal).unwrap();

        let before = committer.stats();
        for i in 0..20u32 {
            committer
                .append_durable(WalOp::Put {
                    key: format!("k{i}").as_bytes(),
                    value: b"v",
                })
                .unwrap();
        }
        let after = committer.stats();

        assert!(after.sync_attempts > before.sync_attempts);
        assert!(after.sync_successes > before.sync_successes);
        assert_eq!(after.sync_failures(), 0);
        assert!(after.records_total >= 20);
        assert!(after.max_batch_records >= 1);
        assert!(after.avg_batch_records() >= 1.0);
        assert_eq!(after.durable_through, 20);
        assert_eq!(after.pending_waiters, 0);
        assert_eq!(
            after.highest_sequence, 20,
            "highest_sequence must track next_seq() - 1"
        );
        assert_eq!(
            after.segment_rotations, 0,
            "no rotation occurred in this scenario"
        );

        committer.rotate().unwrap();
        let after_rotate = committer.stats();
        assert_eq!(
            after_rotate.segment_rotations, 1,
            "an explicit rotate() must be reflected in segment_rotations"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_rejects_a_wal_opened_with_immediate_sync_mode() {
        let dir = temp_dir("rejects_immediate");
        let (wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        let err = GroupCommitter::new(wal).unwrap_err();
        assert!(matches!(err, EngineError::Unsupported { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_then_await_durable_round_trips_single_threaded() {
        let dir = temp_dir("single_thread");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = GroupCommitter::new(wal).unwrap();

        for i in 0..20u32 {
            let pos = committer
                .append(WalOp::Put {
                    key: format!("k{i}").as_bytes(),
                    value: b"v",
                })
                .unwrap();
            assert_eq!(pos.seq, (i as u64) + 1);
            committer.await_durable(pos.seq).unwrap();
            assert!(committer.durable_through() >= pos.seq);
        }

        let wal = committer.into_inner();
        drop(wal);
        let (_wal, result) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        assert_eq!(result.records.len(), 20);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `append_durable`'s convenience wrapper does the same thing as the
    /// two-call form.
    #[test]
    fn append_durable_is_equivalent_to_append_then_await() {
        let dir = temp_dir("append_durable");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = GroupCommitter::new(wal).unwrap();

        let pos = committer
            .append_durable(WalOp::Put {
                key: b"k",
                value: b"v",
            })
            .unwrap();
        assert!(committer.durable_through() >= pos.seq);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Reopening a WAL that already has durable records and immediately
    /// wrapping it must not require any new write before `await_durable`
    /// on an already-durable `seq` returns `Ok` — see `new`'s doc comment.
    #[test]
    fn durable_through_starts_from_what_recovery_already_proved_durable() {
        let dir = temp_dir("resume_durable_through");
        {
            let (mut wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
            for i in 0..5u32 {
                wal.append_sync(WalOp::Put {
                    key: format!("k{i}").as_bytes(),
                    value: b"v",
                })
                .unwrap();
            }
        }
        let (wal, replay) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        assert_eq!(replay.records.len(), 5);
        let committer = GroupCommitter::new(wal).unwrap();
        assert_eq!(committer.durable_through(), 5);
        // No new write needed: an already-durable seq resolves immediately.
        committer.await_durable(5).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Regression test for a real footgun caught in review: `durable_
    /// through` must be seeded from `FileWal::durable_seq()` (only ever
    /// advanced by a genuinely successful `fsync`), not from
    /// `next_seq() - 1` (merely "assigned"). A raw, unsynced `append()`
    /// before handing the `FileWal` to `GroupCommitter::new` must *not* be
    /// silently treated as already durable.
    #[test]
    fn an_unsynced_raw_append_before_wrapping_is_not_treated_as_durable() {
        let dir = temp_dir("unsynced_append_footgun");
        let (mut wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        // append() only — deliberately never sync()ed.
        let pos = wal
            .append(WalOp::Put {
                key: b"k",
                value: b"v",
            })
            .unwrap();
        assert_eq!(pos.seq, 1);

        let committer = GroupCommitter::new(wal).unwrap();
        // The unsynced record must NOT be reported durable yet.
        assert_eq!(
            committer.durable_through(),
            0,
            "an unsynced append must not be treated as durable just because it was assigned a seq"
        );
        // It does become durable once a real batch (this committer's own
        // warm-up fsync already covers it, since the warm-up fsyncs the
        // active segment as it stands at construction time) has run.
        committer.await_durable(1).unwrap();

        let _ = fs::remove_dir_all(&dir);
    }

    /// A modest concurrency smoke test (heavier-weight scenarios live in
    /// `tests/group_commit/`): many threads appending and awaiting
    /// concurrently must all succeed, and every acknowledged record must
    /// be recoverable afterward.
    #[test]
    fn concurrent_appends_all_become_durable_and_recoverable() {
        let dir = temp_dir("concurrent_smoke");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = Arc::new(GroupCommitter::new(wal).unwrap());

        const THREADS: usize = 8;
        const PER_THREAD: usize = 50;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let committer = Arc::clone(&committer);
                thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let position = committer
                            .append(WalOp::Put {
                                key: format!("t{t}-{i}").as_bytes(),
                                value: b"v",
                            })
                            .unwrap();
                        await_durable_retrying_on_timeout(&committer, position.seq);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let committer = Arc::into_inner(committer).expect("no outstanding Arc clones remain");
        let wal = committer.into_inner();
        drop(wal);
        let (_wal, result) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        assert_eq!(result.records.len(), THREADS * PER_THREAD);
        assert!(result.corrupted_segments.is_empty());
        for (i, (seq, _)) in result.records.iter().enumerate() {
            assert_eq!(*seq, (i as u64) + 1);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Injecting a leader `fsync` failure must poison the committer
    /// permanently: the failing call's own waiter gets `Err`, and a
    /// *fresh* record appended afterward also fails `await_durable`,
    /// without ever attempting another real `fsync`.
    #[test]
    fn a_failed_leader_fsync_poisons_the_committer_permanently() {
        let dir = temp_dir("poison");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = GroupCommitter::new(wal).unwrap();

        committer
            .install_fsync_fault_hook(|| Err(io::Error::other("injected leader fsync failure")));

        let pos = committer
            .append(WalOp::Put {
                key: b"k1",
                value: b"v1",
            })
            .unwrap();
        let err = committer.await_durable(pos.seq).unwrap_err();
        assert!(matches!(err, EngineError::Io(_)));

        // A fresh record, appended after the failure, must also fail —
        // the poisoned state persists without a second real fsync attempt.
        let pos2 = committer
            .append(WalOp::Put {
                key: b"k2",
                value: b"v2",
            })
            .unwrap();
        let err2 = committer.await_durable(pos2.seq).unwrap_err();
        assert!(matches!(err2, EngineError::Io(_)));

        committer.clear_fsync_fault_hook();
        let _ = fs::remove_dir_all(&dir);
    }

    /// **Phase 3 P0 leader-panic test** (`PHASE3_FAILURE_MODEL.md` §4;
    /// the required "Leader Panic Test" this project's Phase 3 brief
    /// mandates). Deterministic — no reliance on timing races — via
    /// `install_fsync_fault_hook`'s panic injection. Verifies, in one
    /// place, every property the brief requires:
    ///
    /// 1. records durable *before* the panic remain durable (never
    ///    un-published, never lost);
    /// 2. the panicking caller's own `await_durable` returns promptly
    ///    (never hangs);
    /// 3. the committer transitions to the documented terminal state
    ///    (`is_poisoned() == true`, `PoisonReason::LeaderPanicked`);
    /// 4. a later write, submitted after the panic, fails fast (bounded,
    ///    immediate — not a multi-second wait for a leader that will
    ///    never come) rather than being silently dropped or falsely
    ///    acknowledged;
    /// 5. after discarding this poisoned committer and reopening the same
    ///    WAL directory (the documented recovery path — "process
    ///    restart"), recovery succeeds, sees exactly the pre-panic
    ///    records, gap-free and duplicate-free, and a fresh `GroupCommitter`
    ///    over the reopened `FileWal` works normally again.
    #[test]
    fn leader_panic_clears_leader_active_poisons_and_recovers_cleanly_on_reopen() {
        let dir = temp_dir("leader_panic");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = GroupCommitter::new(wal).unwrap();

        // One record durable *before* the panic — must survive it.
        let pos1 = committer
            .append_durable(WalOp::Put {
                key: b"before-panic",
                value: b"v1",
            })
            .unwrap();
        assert!(committer.durable_through() >= pos1.seq);

        // Arm the panic and become leader for a second record.
        committer.install_fsync_fault_hook(|| panic!("injected leader panic (Phase 3 P0 test)"));
        let pos2 = committer
            .append(WalOp::Put {
                key: b"during-panic",
                value: b"v2",
            })
            .unwrap();

        let leader_started = Instant::now();
        let leader_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            committer.await_durable(pos2.seq)
        }));
        assert!(
            leader_result.is_err(),
            "the injected panic must actually unwind through await_durable, not be swallowed"
        );
        assert!(
            leader_started.elapsed() < Duration::from_secs(1),
            "the panicking leader's own call must return (via unwind) promptly, not hang: {:?}",
            leader_started.elapsed()
        );

        // Property 1: the pre-panic record is still durable.
        assert!(
            committer.durable_through() >= pos1.seq,
            "a record durable before the leader panic must remain durable after it"
        );

        // Property 3: documented terminal state, not a silently-true
        // `leader_active` — `LeaderFailureGuard::drop` ran during the
        // unwind above, before `catch_unwind` even returned.
        assert!(
            committer.is_poisoned(),
            "a leader panic must poison the committer, not leave it silently usable"
        );

        // Property 2/4: a *different* caller, after the panic, must fail
        // fast — bounded and immediate, never hanging and never silently
        // treated as durable.
        let follower_started = Instant::now();
        let follower_err = committer.await_durable(pos2.seq).unwrap_err();
        assert!(
            matches!(follower_err, EngineError::Io(_)),
            "a poisoned committer must report a clear error, not Ok or a bare timeout: \
             {follower_err:?}"
        );
        assert!(
            follower_started.elapsed() < Duration::from_millis(500),
            "a poisoned committer must fail immediately, not after riding out a follower \
             timeout waiting for a leader that will never be elected again: {:?}",
            follower_started.elapsed()
        );

        // A fresh write submitted after the panic must also fail fast,
        // never silently dropped and never falsely acknowledged.
        let pos3 = committer
            .append(WalOp::Put {
                key: b"after-panic",
                value: b"v3",
            })
            .unwrap();
        assert!(committer.await_durable(pos3.seq).is_err());

        committer.clear_fsync_fault_hook();

        // Property 5: recovery via the existing WAL contract. `pos2`'s
        // and `pos3`'s bytes were `append()`ed (written, not just
        // buffered in memory) before the panic/poison, so the segment
        // file itself may contain them even though this process's own
        // `durable_through` watermark never advanced past `pos1` — that
        // is exactly why recovery re-scans from disk rather than trusting
        // any in-memory state (`wal::mod`'s own recovery contract) and
        // why this test asserts *at least* the durably-acknowledged
        // prefix survives, not that later, never-confirmed bytes are
        // absent.
        drop(committer);
        let (wal, replay) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        assert!(
            replay.corrupted_segments.is_empty(),
            "a leader panic must never leave the WAL itself corrupted"
        );
        assert!(
            !replay.records.is_empty(),
            "the pre-panic durable record must survive a real reopen, not just the in-memory \
             watermark"
        );
        for (i, (seq, _)) in replay.records.iter().enumerate() {
            assert_eq!(
                *seq,
                (i as u64) + 1,
                "recovered sequences must be gap-free and in order, panic or not"
            );
        }
        assert_eq!(
            replay.records.first().map(|(_, op)| op.clone()),
            Some(WalOpOwned::Put {
                key: b"before-panic".to_vec(),
                value: b"v1".to_vec(),
            }),
            "the durably-acknowledged pre-panic record must be exactly what was written"
        );

        // A brand-new GroupCommitter over the reopened WAL must work
        // normally — the panic/poison never permanently disables this
        // WAL directory, only the one `GroupCommitter` instance that
        // witnessed the panic.
        let fresh_committer = GroupCommitter::new(wal).unwrap();
        let pos4 = fresh_committer
            .append_durable(WalOp::Put {
                key: b"after-reopen",
                value: b"v4",
            })
            .unwrap();
        assert!(fresh_committer.durable_through() >= pos4.seq);
        assert!(!fresh_committer.is_poisoned());

        let _ = fs::remove_dir_all(&dir);
    }

    /// Multiple concurrent callers must all be woken and fail promptly
    /// when whichever one of them wins the leader race panics — not each
    /// independently discovering the poison only after riding out its own
    /// `condvar` timeout. **Which specific caller becomes leader is a
    /// runtime race** (the algorithm elects whoever's `await_durable`
    /// first observes `leader_active == false`, unrelated to `seq` order
    /// — see `await_durable`'s own doc comment), so this test does not
    /// assume it is any particular one: every caller shares the same
    /// panicking fault hook, and the assertions below only require that
    /// *exactly one* of them panics (whichever won) and every other one
    /// fails fast and cleanly (a poisoned-committer error, not a hang, not
    /// a false `Ok`).
    #[test]
    fn concurrent_followers_all_fail_fast_when_the_leader_panics() {
        let dir = temp_dir("leader_panic_followers");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = Arc::new(GroupCommitter::new(wal).unwrap());

        const CALLERS: usize = 17;
        let callers_ready = Arc::new(StdAtomicU64::new(0));
        {
            let callers_ready = Arc::clone(&callers_ready);
            committer.install_fsync_fault_hook(move || {
                // Blocks until every caller below has at least reached the
                // point of calling `append`/being spawned, so the panic
                // (fired by whichever one wins the leader race) genuinely
                // happens with the rest already concurrently in flight —
                // not a race between "spawn threads" and "the leader's own
                // fsync" that could vary run to run.
                let deadline = Instant::now() + Duration::from_secs(5);
                while callers_ready.load(AtomicOrdering::Acquire) < CALLERS as u64 {
                    if Instant::now() >= deadline {
                        panic!(
                            "callers never reached their wait before the deadline; \
                             test harness bug, not the code under test"
                        );
                    }
                    thread::yield_now();
                }
                panic!("injected leader panic with concurrent followers waiting");
            });
        }

        let mut positions = Vec::with_capacity(CALLERS);
        for i in 0..CALLERS {
            positions.push(
                committer
                    .append(WalOp::Put {
                        key: format!("caller-{i}").as_bytes(),
                        value: b"v",
                    })
                    .unwrap(),
            );
        }

        let overall_started = Instant::now();
        let handles: Vec<_> = positions
            .iter()
            .map(|pos| {
                let committer = Arc::clone(&committer);
                let callers_ready = Arc::clone(&callers_ready);
                let seq = pos.seq;
                thread::spawn(move || {
                    callers_ready.fetch_add(1, AtomicOrdering::AcqRel);
                    let started = Instant::now();
                    // `thread::spawn` already isolates a child panic —
                    // `join()` below observes it as `Err`, no explicit
                    // `catch_unwind` needed here.
                    let result = committer.await_durable(seq);
                    (result, started.elapsed())
                })
            })
            .collect();

        let mut panicked = 0usize;
        let mut failed_cleanly = 0usize;
        for h in handles {
            match h.join() {
                Err(_) => panicked += 1,
                Ok((Ok(()), _)) => {
                    panic!("no caller of a batch whose leader panicked may observe a successful Ok")
                }
                Ok((Err(err), elapsed)) => {
                    failed_cleanly += 1;
                    assert!(
                        matches!(err, EngineError::Io(_)),
                        "a caller that lost the leader race must see a clear poisoned-committer \
                         error, not a bare timeout: {err:?}"
                    );
                    assert!(
                        elapsed < Duration::from_secs(1),
                        "a follower must be woken and fail promptly once the leader panics and \
                         poisons the committer, not wait out its own condvar timeout: {elapsed:?}"
                    );
                }
            }
        }
        assert_eq!(
            panicked, 1,
            "exactly one caller wins the leader race and panics; every other one must observe \
             the poison as a normal Err, not also panic"
        );
        assert_eq!(failed_cleanly, CALLERS - 1);
        assert!(overall_started.elapsed() < Duration::from_secs(6));

        assert!(committer.is_poisoned());
        let _ = fs::remove_dir_all(&dir);
    }

    /// `rotate()` in the middle of otherwise-normal single-threaded use
    /// must not disrupt already-durable records or block future ones.
    #[test]
    fn rotate_between_batches_does_not_disrupt_durability() {
        let dir = temp_dir("rotate_single_threaded");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = GroupCommitter::new(wal).unwrap();

        let pos1 = committer
            .append_durable(WalOp::Put {
                key: b"k1",
                value: b"v1",
            })
            .unwrap();
        committer.rotate().unwrap();
        let pos2 = committer
            .append_durable(WalOp::Put {
                key: b"k2",
                value: b"v2",
            })
            .unwrap();

        assert!(committer.durable_through() >= pos2.seq);
        assert!(committer.durable_through() >= pos1.seq);

        let wal = committer.into_inner();
        drop(wal);
        let (_wal, result) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        assert_eq!(result.records.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Phase 3C: `purge_before` must be safe to call concurrently with
    /// ongoing writes (it never touches the active segment — see
    /// `FileWal::purge_before`'s own doc comment) and must genuinely
    /// bound the WAL's on-disk footprint: after purging everything below
    /// the current durable watermark, only the most recent (still-active)
    /// segment's records remain recoverable.
    #[test]
    fn purge_before_bounds_wal_footprint_under_concurrent_writes() {
        let dir = temp_dir("purge_concurrent");
        let (wal, _) = FileWal::open_for_recovery(&dir, group_commit_config()).unwrap();
        let committer = Arc::new(GroupCommitter::new(wal).unwrap());

        const THREADS: usize = 8;
        const PER_THREAD: usize = 200;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let committer = Arc::clone(&committer);
                thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let position = committer
                            .append(WalOp::Put {
                                key: format!("t{t}-{i}").as_bytes(),
                                value: b"v",
                            })
                            .unwrap();
                        await_durable_retrying_on_timeout(&committer, position.seq);
                        // Force frequent rotation so there is real,
                        // multi-segment purge work to do, not just a
                        // single active segment.
                        if i.is_multiple_of(20) {
                            let _ = committer.rotate();
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let total = THREADS * PER_THREAD;
        assert_eq!(committer.durable_through(), total as u64);

        let segments_before_purge = committer.current_segment_id();
        assert!(
            segments_before_purge > 1,
            "this test must actually exercise multiple segments to be meaningful"
        );

        // Purge everything below the current durable watermark — only
        // the active segment's own records should remain recoverable.
        let removed = committer.purge_before(committer.durable_through()).unwrap();
        assert!(
            !removed.is_empty(),
            "purging below the full durable watermark must remove at least the sealed segments"
        );

        let committer = Arc::into_inner(committer).expect("no outstanding Arc clones remain");
        let wal = committer.into_inner();
        drop(wal);
        let (_wal, result) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        assert!(result.corrupted_segments.is_empty());
        // Every remaining record must still be gap-free relative to its
        // own seq (purge never corrupts what it keeps), and every kept
        // record's seq must be one that was genuinely written.
        for (seq, _) in &result.records {
            assert!(*seq >= 1 && *seq <= total as u64);
        }
        assert!(
            result.records.len() < total,
            "purge must have actually reduced the recoverable record count \
             (before={total}, after={})",
            result.records.len()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn estimate_frame_len_matches_the_real_encoder_for_put() {
        use crate::wal::ops::encode_wal_frame;
        let op = WalOp::Put {
            key: b"hello",
            value: b"world!",
        };
        let real = encode_wal_frame(1, op, crate::wal::DEFAULT_MAX_RECORD_LEN)
            .unwrap()
            .len();
        assert_eq!(estimate_frame_len(&op), real);
    }
}
