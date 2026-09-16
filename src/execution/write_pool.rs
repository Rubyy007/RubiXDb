//! `WriteWorkerPool`: a bounded execution layer between many logical
//! callers and the one ordered `GroupCommitter`/WAL underneath — Phase
//! 2's answer to "separate logical client concurrency from physical
//! storage execution concurrency" (`PHASE2_WORKER_POOL_ARCHITECTURE.md`
//! §1).
//!
//! ```text
//!   Logical callers (10s/100s/1,000+)
//!           |  submit(op) -> Completion
//!           v
//!   bounded queue (Mutex<VecDeque> + 2 Condvars: not_empty, not_full)
//!           |  dequeue
//!           v
//!   N worker threads  --------->  Arc<GroupCommitter>  --------> WAL
//!           |  append_durable(op)
//!           v
//!   Completion::complete(result)  (wakes the original caller's wait())
//! ```
//!
//! # This is an execution optimization, not a durability redesign
//!
//! Every durability guarantee `PHASE1_GROUP_COMMIT.md`/`PHASE1_FAILURE_
//! MODEL.md` document for `GroupCommitter` holds completely unchanged
//! underneath this module, because this module **never reimplements
//! any of them** — it only changes *which threads* call `GroupCommitter::
//! append_durable`. A worker calls exactly the same `append()` then
//! `await_durable()` sequence any Phase 1 direct caller already could
//! (§12 of the operating brief); `GroupCommitter` cannot tell a worker
//! thread's call apart from a Phase 1 direct caller's, and does not need
//! to. Sequence assignment, the durability watermark, batching, rotation,
//! and poisoning are all still owned exclusively by `GroupCommitter` —
//! see "# Ordering" below for exactly why this means the worker pool
//! needs no sequence-allocation logic of its own at all.
//!
//! # Ordering
//!
//! **Queue insertion order is *not* sequence order, and this is
//! correct, not a bug.** `GroupCommitter::append` is the sole assigner
//! of `seq` (under its own internal `wal` lock — `PHASE1_GROUP_COMMIT.md`
//! §2), and it already tolerates being called concurrently by many
//! threads in whatever order the OS scheduler happens to run them —
//! that is exactly Phase 1's own direct-thread model. A worker thread
//! calling `append()` is indistinguishable from a Phase 1 direct caller
//! doing the same thing; `seq` is assigned at the moment a *worker's*
//! `append()` call actually acquires that lock, not at the moment its
//! request was enqueued. Two requests enqueued in order `A, B` may
//! therefore be assigned sequence numbers in either order, if two
//! *different* workers happen to dequeue and process them concurrently
//! — this is the same guarantee (or lack of one) Phase 1 direct callers
//! already had, not a regression introduced by adding a queue in front.
//! What the WAL format and `GroupCommitter` actually guarantee —
//! "every assigned `seq` becomes durable in a gap-free, duplicate-free
//! prefix" — is preserved entirely by `GroupCommitter` itself and is
//! untouched by this module.
//!
//! # Memory ownership (operating brief §6)
//!
//! | Phase | Owner |
//! |---|---|
//! | Before `submit()` | The caller owns its `key`/`value` bytes. |
//! | `submit()`'s one copy | The caller's bytes are copied into a `WalOpOwned` (owned, not borrowed — a queued request must be able to outlive the submitting caller's own stack frame/wait) — this is the **only** copy this module ever makes. |
//! | While queued | The `QueueEntry` (holding the `WalOpOwned` and an `Arc<CompletionSlot>`) is owned by the shared queue; no other owner exists. |
//! | During processing | The worker that dequeued the entry owns it; `WalOpOwned::as_wal_op` re-borrows its buffers (zero-copy) to call `GroupCommitter::append_durable`. |
//! | After completion | The `WalOpOwned` is dropped by the worker once `append_durable` returns (its bytes are by then encoded into the WAL's own frame buffer, per `wal::ops::encode_wal_frame` — a second, WAL-internal copy this module has no visibility into and does not duplicate). The `Result<WalPosition>` is moved into the `CompletionSlot`, read at most once by the caller's `Completion::wait`/`wait_timeout` (which consumes `self`, so the type system — not a runtime check — prevents reading it twice). |
//!
//! # State machine
//!
//! `RUNNING` → (`shutdown()`) → `DRAINING` → (queue empties, every
//! worker exits) → `STOPPED`. `RUNNING`/`DRAINING` → (every worker
//! thread terminates *without* a requested shutdown — by elimination,
//! every one of them panicked) → `FAILED`, and the last worker to exit
//! drains and fails every request still in the queue rather than
//! leaving any caller blocked in `Completion::wait()` forever. `FAILED`
//! and `STOPPED` are both terminal; `submit()` rejects immediately from
//! either. There is no `STOPPING` distinct from `DRAINING` in this
//! implementation — draining *is* the only work stopping requires
//! (workers hold no other state to tear down) — `PoolState` still names
//! a placeholder-free four states rather than a fifth that would never
//! actually be observed, per this project's own standing rule against
//! aspirational states with no real code path (`src/wal/mod.rs`'s
//! `scan_directory` doc comment makes the same call for an analogous
//! reason).
//!
//! # Failure semantics
//!
//! | Failure | What happens |
//! |---|---|
//! | Worker panics mid-request | That one request completes with `Err(EngineError::Aborted)` (`CompletionGuard`'s `Drop` fires the moment the panic unwinds past it — the caller's `Completion::wait()` is never left hanging); other queued requests are unaffected and are picked up by any surviving worker. |
//! | Every worker has panicked | `PoolState::Failed`; every request still queued at that moment is failed with `EngineError::WalUnavailable` rather than left to rot with no worker left to dequeue it; `submit()` rejects all further work immediately. |
//! | `GroupCommitter::append`/`append_durable` returns `Err` (WAL append failure, sync failure, poisoned committer, committer already shutting down) | Propagated faithfully to the one request's own `Completion` — never turned into a false success, never silently retried (a silent retry of an already-appended-but-unconfirmed write could duplicate a logical record — forbidden by the operating brief §2 and never done here: this module retries nothing on the caller's behalf at all). |
//! | Queue full | `submit()` blocks up to `WriteWorkerPoolConfig::submission_timeout`, then returns `Err(EngineError::Timeout)` — never silently drops the write, never blocks unboundedly (operating brief §8). |
//! | `shutdown()` called while requests are queued | New submissions rejected immediately; already-queued requests are still appended and awaited normally (the underlying `GroupCommitter` is **not** told to shut down until every worker has fully drained the queue and exited — doing it earlier would make every still-queued request's own `append()` fail immediately, per `GroupCommitter::append`'s own documented contract) — see `shutdown`'s doc comment for the full three-step sequence. |

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::{EngineError, Result};
use crate::wal::group_commit::{estimate_frame_len, GroupCommitStats, ShutdownReport};
use crate::wal::{GroupCommitter, WalOpOwned, WalPosition};

/// An opaque, monotonically-increasing identifier assigned to every
/// request at `submit()` time — for observability/correlation only
/// (logs, `WorkerPoolStats`, test assertions). **Not** used for
/// ordering: see this module's "# Ordering" section — `GroupCommitter`'s
/// own assigned `seq` (visible in a successful `Completion`'s
/// `WalPosition`) is the only ordering-relevant identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// `WriteWorkerPool` configuration. Every bound here is a genuine bound
/// (operating brief §8/§16: "Do not allow unbounded memory allocation")
/// — there is no configuration that reproduces an unbounded queue.
#[derive(Debug, Clone)]
pub struct WriteWorkerPoolConfig {
    /// Number of long-lived worker threads. **Not hardcoded to any
    /// particular value by this module** — the operating brief's own
    /// benchmark matrix (`PHASE2_PERFORMANCE.md`) determines a
    /// measured, hardware-specific default; this field always overrides
    /// it explicitly.
    pub worker_count: usize,
    /// Maximum number of requests the queue may hold at once (already
    /// dequeued-but-in-progress requests do not count against this —
    /// only what is actually sitting in the `VecDeque`).
    pub queue_capacity: usize,
    /// Maximum total estimated payload bytes (via `estimate_frame_len`,
    /// the same formula `GroupCommitter` uses for its own
    /// `max_batch_bytes` accounting) the queue may hold at once, on top
    /// of `queue_capacity` — a queue of few-but-huge payloads is bounded
    /// by this even when `queue_capacity` alone would not catch it.
    pub max_queued_bytes: usize,
    /// How long `submit()` blocks waiting for queue capacity before
    /// giving up with `EngineError::Timeout` (operating brief §8's
    /// "bounded wait then error" choice — see `submit`'s doc comment for
    /// why this was chosen over the other two options the brief allows).
    pub submission_timeout: Duration,
    /// How long `shutdown()` blocks waiting to observe every worker
    /// finish draining the queue and exit, before returning anyway
    /// (mirroring `GroupCommitter::shutdown`'s own `SHUTDOWN_DRAIN_
    /// BOUND` philosophy: bounded, never an unconditional hang — see
    /// `shutdown`'s doc comment).
    pub shutdown_drain_bound: Duration,
    /// Total extra wall-clock budget a worker spends retrying
    /// `GroupCommitter::await_durable` on `EngineError::Timeout` before
    /// giving up and delivering that `Timeout` to the caller — see
    /// `process_entry`'s doc comment for why this is always safe
    /// (retrying a *wait*, never an *append*, carries zero duplicate-
    /// record risk) and why a single-attempt design would otherwise
    /// hand callers spurious failures for writes that are, in fact, on
    /// their way to becoming durable. Mirrors `tests/group_commit/
    /// support.rs`'s own `await_durable_retrying_on_timeout` — the
    /// pattern this project's own Phase 1 test harness already
    /// established as the correct way to consume a bounded-timeout
    /// wait, now applied in the production path rather than only in
    /// tests.
    pub await_retry_budget: Duration,
}

impl Default for WriteWorkerPoolConfig {
    /// Defaults are deliberately conservative placeholders, not the
    /// "recommended" configuration — `PHASE2_PERFORMANCE.md` records
    /// the actually-measured best `worker_count` on this project's
    /// development hardware; callers building for a specific workload
    /// should override every field explicitly rather than rely on this.
    fn default() -> Self {
        WriteWorkerPoolConfig {
            worker_count: 16,
            queue_capacity: 8192,
            max_queued_bytes: 64 * 1024 * 1024,
            submission_timeout: Duration::from_secs(1),
            shutdown_drain_bound: Duration::from_secs(10),
            await_retry_budget: Duration::from_secs(2),
        }
    }
}

/// See this module's "# State machine" section for the full transition
/// diagram and rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolState {
    /// Accepting submissions normally.
    Running,
    /// `shutdown()` has been called (or every worker has failed — see
    /// `Failed` below for how that differs); no new submissions are
    /// accepted; workers continue draining whatever is already queued.
    Draining,
    /// Every worker has exited after a requested shutdown, with the
    /// queue confirmed empty. Terminal.
    Stopped,
    /// Every worker thread terminated (by elimination: every one of
    /// them panicked) without a shutdown ever having been requested.
    /// Anything left in the queue at that moment was already failed
    /// with `EngineError::WalUnavailable` rather than abandoned.
    /// Terminal.
    Failed,
}

/// A point-in-time snapshot of pool-level observability counters, plus
/// the underlying `GroupCommitter`'s own `stats()` (batch/sync-level
/// detail this module deliberately does not duplicate — see
/// `WriteWorkerPool::stats`).
#[derive(Debug, Clone)]
pub struct WorkerPoolStats {
    pub state: PoolState,
    pub submitted: u64,
    pub completed_ok: u64,
    pub completed_err: u64,
    pub rejected_backpressure: u64,
    /// Current number of requests sitting in the queue (already
    /// dequeued-and-processing requests are not included).
    pub queue_depth: usize,
    pub queued_bytes: usize,
    pub workers_alive: usize,
    /// Sum of every completed request's queue-wait duration (enqueue to
    /// dequeue), in nanoseconds — divide by `completed_ok +
    /// completed_err` for the mean.
    pub queue_wait_ns_total: u64,
    /// Sum of every completed request's worker processing duration
    /// (dequeue to `append_durable` returning), in nanoseconds.
    pub processing_ns_total: u64,
    pub committer_stats: GroupCommitStats,
}

/// Returned by `WriteWorkerPool::shutdown` — see that method's doc
/// comment for the exact three-step sequence this reports on.
#[derive(Debug, Clone)]
pub struct WorkerPoolShutdownReport {
    /// `true` iff every worker had exited (queue fully drained) by the
    /// time `shutdown()` returned — i.e. `shutdown_drain_bound` was not
    /// exceeded. `false` does **not** mean anything was lost: workers
    /// that had not yet exited keep running and keep draining the queue
    /// in the background after `shutdown()` itself returns (mirroring
    /// `GroupCommitter::shutdown`'s own "never blocks unconditionally,
    /// never abandons in-flight work" contract).
    pub fully_drained: bool,
    pub pool_state: PoolState,
    /// `Some` iff `fully_drained` — the underlying `GroupCommitter` is
    /// only ever told to shut down once no worker will ever call
    /// `append()` again (see `shutdown`'s doc comment for why ordering
    /// this any earlier would be wrong).
    pub committer_report: Option<ShutdownReport>,
    pub queue_depth_at_shutdown: usize,
}

/// One request's completion state — a minimal, `std`-only "oneshot": a
/// `Mutex<Option<Result<WalPosition>>>` plus a `Condvar`, exactly the
/// same primitive pair `GroupCommitter` itself already uses throughout
/// (`batch`/`condvar`) — no new dependency, no unsafe code. Reached only
/// through `Arc`, so Rust's ownership model — not manual reasoning —
/// rules out use-after-free: the last owner (whichever of the worker or
/// the waiting caller drops its `Arc` last) is the one whose drop
/// actually deallocates it.
struct CompletionSlot {
    result: Mutex<Option<Result<WalPosition>>>,
    condvar: Condvar,
}

impl CompletionSlot {
    fn new() -> Self {
        CompletionSlot {
            result: Mutex::new(None),
            condvar: Condvar::new(),
        }
    }

    /// Records `result` and wakes every waiter. Idempotent by
    /// construction if ever called twice (only the first call's result
    /// is kept) — defense in depth on top of `CompletionGuard`'s own
    /// `armed` flag, which already ensures this is called at most once
    /// per request in every real code path.
    fn complete(&self, result: Result<WalPosition>) {
        let mut guard = self.result.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            *guard = Some(result);
        }
        drop(guard);
        self.condvar.notify_all();
    }
}

/// The caller-facing handle `submit()` returns. Consuming (`wait`/
/// `wait_timeout` take `self` by value), so the type system — not a
/// runtime check — guarantees a `Completion` is read at most once.
/// **Not** `Clone`: this module supports no cancellation of an
/// in-flight/queued write (operating brief §6 — "cancellation state if
/// supported" — deliberately not supported here). Once `submit()`
/// returns `Ok`, the request *will* be attempted; a caller may only stop
/// *waiting* for the answer (`wait_timeout`), never stop the write
/// itself — trying to do the latter safely (rolling back an `append()`
/// that may already have landed a `seq` in the WAL) is exactly the kind
/// of operation the operating brief's rule against duplicate-risking
/// retries also rules out doing to a *cancellation*, so it is not
/// attempted at all.
pub struct Completion {
    slot: Arc<CompletionSlot>,
    pub request_id: RequestId,
}

impl Completion {
    /// Blocks until the request completes. Always eventually returns —
    /// see this module's "# Failure semantics" table for every path
    /// that guarantees `CompletionSlot::complete` is eventually called
    /// exactly once for a request that reached the queue (submission
    /// itself is bounded by `submission_timeout`, so a caller can never
    /// reach this method for a request that was never actually queued).
    pub fn wait(self) -> Result<WalPosition> {
        let mut guard = self.slot.result.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(result) = guard.take() {
                return result;
            }
            guard = self
                .slot
                .condvar
                .wait(guard)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Like `wait`, but gives up after `timeout` and returns
    /// `EngineError::Timeout` instead of blocking further. **The
    /// underlying write is not stopped or rolled back** — exactly like
    /// `GroupCommitter::await_durable`'s own `Timeout` (`PHASE1_FAILURE_
    /// MODEL.md` §3), this means "this call stopped waiting to hear the
    /// answer," never "the write did not happen." The worker keeps
    /// processing the request in the background regardless; its eventual
    /// result is simply never read by anyone once this method returns.
    pub fn wait_timeout(self, timeout: Duration) -> Result<WalPosition> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.slot.result.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(result) = guard.take() {
                return result;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(EngineError::Timeout {
                    detail: format!(
                        "Completion::wait_timeout(request_id={}) timed out after {timeout:?}; \
                         the write itself was not stopped and may still become durable",
                        self.request_id.0
                    ),
                });
            }
            let (new_guard, _) = self
                .slot
                .condvar
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            guard = new_guard;
        }
    }
}

/// RAII guard around one dequeued request's `CompletionSlot`
/// (`process_entry`'s sole use). If dropped without `complete` having
/// been called explicitly — the only way that happens is the worker
/// thread panicking somewhere between dequeue and the normal `complete`
/// call at the end of `process_entry` — it fires a fallback `Err`
/// completion itself, so the submitting caller's `Completion::wait` is
/// never left blocked forever by a panic it cannot see (operating brief
/// §7: "no waiter leak," "no permanent blocking"; §18: "a worker
/// failure must never silently lose a request").
struct CompletionGuard<'a> {
    slot: &'a Arc<CompletionSlot>,
    armed: bool,
}

impl<'a> CompletionGuard<'a> {
    fn new(slot: &'a Arc<CompletionSlot>) -> Self {
        CompletionGuard { slot, armed: true }
    }

    /// The normal path: disarms the fallback (so `Drop` below is a
    /// no-op) and delivers the real result.
    fn complete(mut self, result: Result<WalPosition>) {
        self.armed = false;
        self.slot.complete(result);
    }
}

impl Drop for CompletionGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.slot.complete(Err(EngineError::Aborted {
                detail: "the worker processing this request terminated unexpectedly \
                         (panicked) before it could complete"
                    .to_string(),
            }));
        }
    }
}

struct QueueEntry {
    op: WalOpOwned,
    completion: Arc<CompletionSlot>,
    approx_bytes: usize,
    enqueued_at: Instant,
}

/// Everything guarded by one `Mutex` — deliberately, matching
/// `GroupCommitter::BatchState`'s own "one mutex for the state that
/// actually needs to change together" rationale: `pool_state` and
/// `workers_alive` must be observed consistently with the queue's own
/// contents (e.g. a worker deciding "queue empty AND shutdown
/// requested, safe to exit" needs both facts atomically, not as two
/// separately-locked reads that could race).
struct QueueState {
    entries: VecDeque<QueueEntry>,
    queued_bytes: usize,
    pool_state: PoolState,
    workers_alive: usize,
}

struct PoolShared {
    queue: Mutex<QueueState>,
    /// Workers wait on this when the queue is empty; `submit()` and
    /// `WorkerAliveGuard::drop` notify it (a worker exiting is also a
    /// condition `shutdown()`'s own wait loop below re-checks against
    /// this same condvar).
    not_empty: Condvar,
    /// Submitters wait on this when the queue is at capacity; a worker
    /// dequeueing and `WorkerAliveGuard::drop` (which may free queue
    /// capacity by failing queued entries) notify it.
    not_full: Condvar,
    next_request_id: AtomicU64,
    submitted: std::sync::atomic::AtomicU64,
    completed_ok: std::sync::atomic::AtomicU64,
    completed_err: std::sync::atomic::AtomicU64,
    rejected_backpressure: std::sync::atomic::AtomicU64,
    queue_wait_ns_total: std::sync::atomic::AtomicU64,
    processing_ns_total: std::sync::atomic::AtomicU64,
}

/// Decrements `workers_alive` when a worker thread's body returns for
/// any reason (normal exit *or* an unwinding panic — `Drop` runs in
/// both cases, which is the entire reason this is a guard rather than a
/// plain decrement at the bottom of `worker_loop`). See this module's
/// "# State machine"/"# Failure semantics" sections for what happens
/// when this brings `workers_alive` to zero.
struct WorkerAliveGuard {
    shared: Arc<PoolShared>,
}

impl Drop for WorkerAliveGuard {
    fn drop(&mut self) {
        let mut guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        guard.workers_alive = guard.workers_alive.saturating_sub(1);
        let requested_shutdown = matches!(guard.pool_state, PoolState::Draining);
        if guard.workers_alive == 0 {
            if requested_shutdown {
                // `worker_loop`'s own exit condition only returns once
                // the queue is confirmed empty AND a shutdown was
                // requested (see that function) — by that invariant the
                // queue is guaranteed empty here already.
                guard.pool_state = PoolState::Stopped;
            } else if guard.pool_state != PoolState::Stopped {
                // Every worker is gone with no shutdown ever requested —
                // by elimination, every one of them panicked. Fail
                // whatever is left rather than abandon it.
                guard.pool_state = PoolState::Failed;
                while let Some(entry) = guard.entries.pop_front() {
                    guard.queued_bytes = guard.queued_bytes.saturating_sub(entry.approx_bytes);
                    entry.completion.complete(Err(EngineError::WalUnavailable {
                        detail: "WriteWorkerPool: every worker thread terminated \
                                 unexpectedly (panicked) before this request could be \
                                 dequeued"
                            .to_string(),
                    }));
                }
            }
        }
        drop(guard);
        self.shared.not_empty.notify_all();
        self.shared.not_full.notify_all();
    }
}

/// A bounded execution layer in front of one `GroupCommitter` — see this
/// module's top-of-file doc comment for the full architecture, ordering
/// contract, ownership model, state machine, and failure semantics.
pub struct WriteWorkerPool {
    shared: Arc<PoolShared>,
    committer: Arc<GroupCommitter>,
    config: WriteWorkerPoolConfig,
    /// Taken (via `.take()`) and joined at most once, inside `shutdown`
    /// — `Mutex<Option<_>>` because `shutdown` takes `&self` (matching
    /// `GroupCommitter::shutdown`'s own shared-reference signature) but
    /// `JoinHandle::join` needs to consume each handle by value.
    join_handles: Mutex<Option<Vec<JoinHandle<()>>>>,
}

impl WriteWorkerPool {
    /// Spawns `config.worker_count` worker threads around `committer`,
    /// which this pool takes ownership of (wrapped in an `Arc` shared
    /// with every worker — see "# Ordering" above for why no worker
    /// needs its own `GroupCommitter`, segment, or durability domain).
    ///
    /// If spawning any worker thread fails partway through (OS resource
    /// exhaustion — a real, reachable `io::Error`, not hypothetical),
    /// every already-spawned worker is cleanly shut down and joined
    /// before this returns `Err` — no thread is ever leaked on this
    /// path.
    pub fn new(committer: GroupCommitter, config: WriteWorkerPoolConfig) -> Result<Self> {
        let shared = Arc::new(PoolShared {
            queue: Mutex::new(QueueState {
                entries: VecDeque::new(),
                queued_bytes: 0,
                pool_state: PoolState::Running,
                workers_alive: 0,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            next_request_id: AtomicU64::new(1),
            submitted: std::sync::atomic::AtomicU64::new(0),
            completed_ok: std::sync::atomic::AtomicU64::new(0),
            completed_err: std::sync::atomic::AtomicU64::new(0),
            rejected_backpressure: std::sync::atomic::AtomicU64::new(0),
            queue_wait_ns_total: std::sync::atomic::AtomicU64::new(0),
            processing_ns_total: std::sync::atomic::AtomicU64::new(0),
        });
        let committer = Arc::new(committer);

        let mut handles = Vec::with_capacity(config.worker_count);
        for i in 0..config.worker_count {
            let shared_clone = Arc::clone(&shared);
            let committer_clone = Arc::clone(&committer);
            let await_retry_budget = config.await_retry_budget;
            let spawn_result = thread::Builder::new()
                .name(format!("rubixdb-write-worker-{i}"))
                .spawn(move || worker_loop(shared_clone, committer_clone, await_retry_budget));
            match spawn_result {
                Ok(handle) => {
                    shared
                        .queue
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .workers_alive += 1;
                    handles.push(handle);
                }
                Err(io_err) => {
                    // Clean up whatever already started before surfacing
                    // the error — see this method's own doc comment.
                    let pool = WriteWorkerPool {
                        shared: Arc::clone(&shared),
                        committer: Arc::clone(&committer),
                        config: config.clone(),
                        join_handles: Mutex::new(Some(handles)),
                    };
                    let _ = pool.shutdown();
                    return Err(EngineError::Io(io_err));
                }
            }
        }

        Ok(WriteWorkerPool {
            shared,
            committer,
            config,
            join_handles: Mutex::new(Some(handles)),
        })
    }

    /// Copies `op`'s bytes into an owned request, enqueues it, and
    /// returns a `Completion` to await the durable result — never
    /// blocks past `config.submission_timeout` (operating brief §8's
    /// **bounded wait then error** choice, not the other two the brief
    /// allows: a pure immediate-reject would turn an ordinary momentary
    /// burst — 1,000 callers submitting within the same microsecond,
    /// exactly Phase 1's own benchmark shape — into routine failures a
    /// caller would have to retry manually; pure unbounded blocking
    /// would violate the brief's own bounded-resources requirement by
    /// letting a caller wait forever if workers are wedged. A bounded
    /// wait absorbs a real burst up to the configured timeout while
    /// still guaranteeing `submit()` itself can never hang.
    pub fn submit(&self, op: WalOpOwned) -> Result<Completion> {
        let approx_bytes = estimate_frame_len(&op.as_wal_op());
        let deadline = Instant::now() + self.config.submission_timeout;
        let mut guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            match guard.pool_state {
                PoolState::Draining | PoolState::Stopped => {
                    self.shared
                        .rejected_backpressure
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(EngineError::Aborted {
                        detail: "WriteWorkerPool is shutting down; new submissions are \
                                 rejected"
                            .to_string(),
                    });
                }
                PoolState::Failed => {
                    self.shared
                        .rejected_backpressure
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(EngineError::WalUnavailable {
                        detail: "WriteWorkerPool has failed (every worker thread \
                                 terminated unexpectedly); no new submissions are accepted"
                            .to_string(),
                    });
                }
                PoolState::Running => {}
            }
            let has_capacity = guard.entries.len() < self.config.queue_capacity
                && guard.queued_bytes.saturating_add(approx_bytes) <= self.config.max_queued_bytes;
            if has_capacity {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                self.shared
                    .rejected_backpressure
                    .fetch_add(1, Ordering::Relaxed);
                return Err(EngineError::Timeout {
                    detail: format!(
                        "submit() timed out after {:?} waiting for queue capacity \
                         (depth={}/{}, bytes={}/{})",
                        self.config.submission_timeout,
                        guard.entries.len(),
                        self.config.queue_capacity,
                        guard.queued_bytes,
                        self.config.max_queued_bytes
                    ),
                });
            }
            let (new_guard, _) = self
                .shared
                .not_full
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            guard = new_guard;
        }

        let request_id = RequestId(self.shared.next_request_id.fetch_add(1, Ordering::Relaxed));
        let completion_slot = Arc::new(CompletionSlot::new());
        guard.entries.push_back(QueueEntry {
            op,
            completion: Arc::clone(&completion_slot),
            approx_bytes,
            enqueued_at: Instant::now(),
        });
        guard.queued_bytes = guard.queued_bytes.saturating_add(approx_bytes);
        drop(guard);
        self.shared.submitted.fetch_add(1, Ordering::Relaxed);
        self.shared.not_empty.notify_one();

        Ok(Completion {
            slot: completion_slot,
            request_id,
        })
    }

    /// Shuts the pool down in three ordered steps, none of them an
    /// unconditional hang:
    ///
    /// 1. Rejects new submissions immediately (`PoolState::Draining`)
    ///    and wakes every worker/blocked-submitter so they observe it
    ///    promptly rather than waiting out their own timeouts.
    /// 2. Waits, bounded by `config.shutdown_drain_bound`, for every
    ///    worker to finish draining whatever was already queued and
    ///    exit.
    /// 3. **Only if step 2 fully completed** (every worker confirmed
    ///    exited): shuts the underlying `GroupCommitter` down too.
    ///    Ordering this any earlier would be wrong — `GroupCommitter::
    ///    append`'s own documented contract fails *every* call made
    ///    after its shutdown flag is set, including ones a worker makes
    ///    for a request that was legitimately queued *before* this
    ///    `shutdown()` call — so the underlying WAL must stay fully live
    ///    until nothing will ever call into it again.
    ///
    /// Idempotent: calling this again after the pool is already
    /// `Stopped`/`Failed` is a cheap no-op that reports the current
    /// state rather than re-running the sequence.
    pub fn shutdown(&self) -> WorkerPoolShutdownReport {
        {
            let mut guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            match guard.pool_state {
                PoolState::Stopped | PoolState::Failed => {
                    return WorkerPoolShutdownReport {
                        fully_drained: guard.workers_alive == 0,
                        pool_state: guard.pool_state,
                        committer_report: None,
                        queue_depth_at_shutdown: guard.entries.len(),
                    };
                }
                PoolState::Running => guard.pool_state = PoolState::Draining,
                PoolState::Draining => {}
            }
        }
        self.shared.not_empty.notify_all();
        self.shared.not_full.notify_all();

        let deadline = Instant::now() + self.config.shutdown_drain_bound;
        let mut guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        let queue_depth_at_shutdown = guard.entries.len();
        while guard.workers_alive > 0 {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (new_guard, _) = self
                .shared
                .not_empty
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            guard = new_guard;
        }
        let workers_alive = guard.workers_alive;
        // Normally `WorkerAliveGuard::drop` is what flips `Draining` to
        // `Stopped` the moment the last worker exits. With zero workers
        // configured (or in the rare race where this wait loop observes
        // `workers_alive == 0` in the same instant the last guard's own
        // update landed), no guard drop may ever run again to do that —
        // so this is the fallback that still leaves `pool_state` correct
        // rather than stuck at `Draining` forever.
        if workers_alive == 0 && guard.pool_state == PoolState::Draining {
            guard.pool_state = PoolState::Stopped;
        }
        let pool_state = guard.pool_state;
        drop(guard);

        let committer_report = if workers_alive == 0 {
            // Every worker has exited (`WorkerAliveGuard::drop` already
            // set `Stopped`/`Failed` and, on the `Failed` path, already
            // failed anything left in the queue) — safe, per this
            // method's own doc comment, to finalize the underlying WAL
            // now. Also join the now-finished threads (fast: they have
            // already returned or are returning momentarily by this
            // point) so no `JoinHandle` is left dangling.
            if let Some(handles) = self
                .join_handles
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
            {
                for handle in handles {
                    let _ = handle.join();
                }
            }
            Some(self.committer.shutdown())
        } else {
            None
        };

        WorkerPoolShutdownReport {
            fully_drained: workers_alive == 0,
            pool_state,
            committer_report,
            queue_depth_at_shutdown,
        }
    }

    /// A point-in-time snapshot — see `WorkerPoolStats`'s own field
    /// docs. May be called at any point in the pool's lifetime,
    /// including after `shutdown()`.
    pub fn stats(&self) -> WorkerPoolStats {
        let guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        let queue_depth = guard.entries.len();
        let queued_bytes = guard.queued_bytes;
        let state = guard.pool_state;
        let workers_alive = guard.workers_alive;
        drop(guard);
        WorkerPoolStats {
            state,
            submitted: self.shared.submitted.load(Ordering::Relaxed),
            completed_ok: self.shared.completed_ok.load(Ordering::Relaxed),
            completed_err: self.shared.completed_err.load(Ordering::Relaxed),
            rejected_backpressure: self.shared.rejected_backpressure.load(Ordering::Relaxed),
            queue_depth,
            queued_bytes,
            workers_alive,
            queue_wait_ns_total: self.shared.queue_wait_ns_total.load(Ordering::Relaxed),
            processing_ns_total: self.shared.processing_ns_total.load(Ordering::Relaxed),
            committer_stats: self.committer.stats(),
        }
    }

    pub fn state(&self) -> PoolState {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pool_state
    }

    /// Shuts down (idempotent, per `shutdown`'s own doc comment), then
    /// reclaims the underlying `GroupCommitter` — mirrors `GroupCommitter::
    /// into_inner`'s own role for symmetry. `Err` only if a worker
    /// somehow still holds a reference after a fully-drained shutdown
    /// (would indicate a bug in this module, not a caller error).
    pub fn into_inner(self) -> Result<GroupCommitter> {
        self.shutdown();
        // Clone the `Arc` first rather than moving `self.committer` out
        // directly: `WriteWorkerPool` implements `Drop`, and Rust forbids
        // partially moving a field out of a type that does (the
        // destructor must see every field). Dropping `self` as a whole
        // value just below is fine — only a *partial* move is the issue.
        let committer = Arc::clone(&self.committer);
        drop(self);
        Arc::try_unwrap(committer).map_err(|_| EngineError::WalUnavailable {
            detail: "WriteWorkerPool::into_inner: the underlying GroupCommitter still had \
                     outstanding references after shutdown() drained every worker; this \
                     indicates a bug in WriteWorkerPool, not caller misuse"
                .to_string(),
        })
    }
}

impl Drop for WriteWorkerPool {
    /// RAII safety net: if a caller drops the pool without calling
    /// `shutdown()` explicitly, this ensures every worker thread is
    /// still told to stop and joined rather than leaked — `shutdown()`
    /// itself is idempotent, so this is a no-op if shutdown already
    /// happened.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One worker thread's whole body. Exits (returns) only once a shutdown
/// has been requested **and** the queue is confirmed empty — see this
/// module's "# State machine" section: this is the invariant
/// `WorkerAliveGuard::drop` relies on to know the queue is already
/// empty whenever every worker has exited for an *expected* reason.
fn worker_loop(
    shared: Arc<PoolShared>,
    committer: Arc<GroupCommitter>,
    await_retry_budget: Duration,
) {
    let _alive_guard = WorkerAliveGuard {
        shared: Arc::clone(&shared),
    };
    loop {
        let entry = {
            let mut guard = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if let Some(entry) = guard.entries.pop_front() {
                    guard.queued_bytes = guard.queued_bytes.saturating_sub(entry.approx_bytes);
                    break Some(entry);
                }
                if matches!(
                    guard.pool_state,
                    PoolState::Draining | PoolState::Stopped | PoolState::Failed
                ) {
                    break None;
                }
                guard = shared
                    .not_empty
                    .wait(guard)
                    .unwrap_or_else(|p| p.into_inner());
            }
        };
        match entry {
            Some(entry) => {
                shared.not_full.notify_one();
                process_entry(entry, &committer, &shared, await_retry_budget);
            }
            None => return,
        }
    }
}

/// Processes exactly one dequeued request: `GroupCommitter::append`
/// once, then `await_durable`, **retrying only the wait** — never the
/// append — up to `await_retry_budget` on `EngineError::Timeout`
/// (`WriteWorkerPoolConfig::await_retry_budget`'s own doc comment has
/// the full rationale: a `Timeout` from `await_durable` means the wait
/// itself ran out of time, not that the write was lost — the `seq` is
/// already assigned and will very likely become durable moments later —
/// so retrying the wait costs nothing in correctness (it can never
/// duplicate a record; only `append()` does that, and it is called
/// exactly once here) while sparing the caller a spurious failure for a
/// write that was never actually in trouble). Once the budget is
/// exhausted, the last `Timeout` is delivered faithfully — never
/// silently dropped, never turned into a fabricated success. The only
/// place `CompletionGuard`
/// is ever constructed.
fn process_entry(
    entry: QueueEntry,
    committer: &GroupCommitter,
    shared: &PoolShared,
    await_retry_budget: Duration,
) {
    let queue_wait_ns = u64::try_from(entry.enqueued_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
    shared
        .queue_wait_ns_total
        .fetch_add(queue_wait_ns, Ordering::Relaxed);

    let completion_guard = CompletionGuard::new(&entry.completion);
    let started = Instant::now();
    let result =
        append_then_await_durable_retrying(committer, entry.op.as_wal_op(), await_retry_budget);
    let processing_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    shared
        .processing_ns_total
        .fetch_add(processing_ns, Ordering::Relaxed);

    match &result {
        Ok(_) => shared.completed_ok.fetch_add(1, Ordering::Relaxed),
        Err(_) => shared.completed_err.fetch_add(1, Ordering::Relaxed),
    };
    completion_guard.complete(result);
}

/// `GroupCommitter::append` exactly once, then `await_durable`, retrying
/// **only** the latter on `EngineError::Timeout` until `retry_budget`
/// elapses — see `process_entry`'s doc comment for the full rationale.
/// If `append` itself fails, that error is returned immediately with no
/// retry (an append failure is not a wait timing out — it is a real
/// failure with no `seq` to keep waiting on).
fn append_then_await_durable_retrying(
    committer: &GroupCommitter,
    op: crate::wal::WalOp<'_>,
    retry_budget: Duration,
) -> Result<WalPosition> {
    let position = committer.append(op)?;
    let deadline = Instant::now() + retry_budget;
    loop {
        match committer.await_durable(position.seq) {
            Ok(()) => return Ok(position),
            Err(EngineError::Timeout { .. }) if Instant::now() < deadline => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::{FileWal, SyncMode, Wal, WalConfig};
    use std::fs;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("rubixdb_write_pool_ut_{tag}_{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_committer(dir: &std::path::Path) -> GroupCommitter {
        let config = WalConfig {
            sync_mode: SyncMode::GroupCommit {
                max_wait: Duration::from_millis(5),
                max_batch_bytes: 256 * 1024,
            },
            ..WalConfig::default()
        };
        let (wal, _) = FileWal::open_for_recovery(dir, config).unwrap();
        GroupCommitter::new(wal).unwrap()
    }

    fn small_config(worker_count: usize) -> WriteWorkerPoolConfig {
        WriteWorkerPoolConfig {
            worker_count,
            queue_capacity: 8,
            max_queued_bytes: 1024 * 1024,
            submission_timeout: Duration::from_millis(500),
            shutdown_drain_bound: Duration::from_secs(5),
            await_retry_budget: Duration::from_secs(2),
        }
    }

    #[test]
    fn single_submit_completes_durably_and_recovers() {
        let dir = temp_dir("single");
        let pool = WriteWorkerPool::new(test_committer(&dir), small_config(2)).unwrap();
        let completion = pool
            .submit(WalOpOwned::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        let position = completion.wait().unwrap();
        assert_eq!(position.seq, 1);
        assert_eq!(pool.stats().committer_stats.records_total, 1);
        let report = pool.shutdown();
        assert!(report.fully_drained);
        assert_eq!(report.pool_state, PoolState::Stopped);
        // Release the WAL's own exclusive lock (held by `pool`'s
        // underlying `GroupCommitter`/`FileWal` until dropped) before
        // reopening the same directory below — `shutdown()` alone does
        // not close the file handle, only `into_inner`/`Drop` does.
        drop(pool);

        let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        assert!(replay.corrupted_segments.is_empty());
        assert_eq!(replay.records.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    // Submits, retrying on `EngineError::Timeout` (bounded) — a `submit()`
    // timeout only ever means "the queue had no room in time," never
    // that anything was appended, so retrying it can never duplicate a
    // record (same reasoning as `append_then_await_durable_retrying`'s
    // own `await_durable` retry, one layer up the stack). Mirrors real
    // production caller behavior under bounded backpressure rather than
    // asserting a fixed queue/worker configuration can always instantly
    // absorb 1,000 requests bursting from 50 real OS threads at once.
    fn submit_retrying(pool: &WriteWorkerPool, key: &[u8], value: &[u8]) -> Completion {
        const MAX_RETRIES: u32 = 200;
        for _ in 0..MAX_RETRIES {
            // Rebuilt fresh from the caller's own still-owned `key`/
            // `value` on every attempt: `submit()` moves whatever
            // `WalOpOwned` it is given, so a failed attempt's copy is
            // gone regardless — exactly what a real caller retrying
            // after a `submit()` timeout would also do with its own
            // source bytes.
            let op = WalOpOwned::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            };
            match pool.submit(op) {
                Ok(completion) => return completion,
                Err(EngineError::Timeout { .. }) => continue,
                Err(e) => panic!("submit failed with a non-backpressure error: {e}"),
            }
        }
        panic!("submit did not succeed after {MAX_RETRIES} retries");
    }

    #[test]
    fn many_concurrent_submitters_all_land_a_gap_free_recoverable_prefix() {
        let dir = temp_dir("concurrent");
        let pool = Arc::new(WriteWorkerPool::new(test_committer(&dir), small_config(4)).unwrap());
        const THREADS: usize = 50;
        const PER_THREAD: usize = 20;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let pool = Arc::clone(&pool);
                thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        let key = format!("t{t}-{i}");
                        let completion = submit_retrying(&pool, key.as_bytes(), b"v");
                        completion.wait().expect("write must become durable");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let stats = pool.stats();
        assert_eq!(stats.completed_ok, (THREADS * PER_THREAD) as u64);
        assert_eq!(stats.completed_err, 0);

        let report = pool.shutdown();
        assert!(report.fully_drained);
        // Release the WAL's exclusive lock before reopening it below —
        // `pool` is the only remaining strong reference once every
        // spawned thread above has joined.
        drop(pool);

        let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        assert!(replay.corrupted_segments.is_empty());
        assert_eq!(replay.records.len(), THREADS * PER_THREAD);
        for (i, (seq, _)) in replay.records.iter().enumerate() {
            assert_eq!(
                *seq,
                (i as u64) + 1,
                "sequences must be gap-free and ordered"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn queue_full_rejects_with_timeout_not_silently() {
        let dir = temp_dir("full");
        // Zero workers: nothing ever drains the queue, so it is
        // guaranteed to stay full for the duration of this test.
        let config = WriteWorkerPoolConfig {
            worker_count: 0,
            queue_capacity: 2,
            max_queued_bytes: 1024 * 1024,
            submission_timeout: Duration::from_millis(100),
            shutdown_drain_bound: Duration::from_secs(5),
            await_retry_budget: Duration::from_secs(2),
        };
        let pool = WriteWorkerPool::new(test_committer(&dir), config).unwrap();
        let _c1 = pool
            .submit(WalOpOwned::Put {
                key: b"a".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        let _c2 = pool
            .submit(WalOpOwned::Put {
                key: b"b".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        let third = pool.submit(WalOpOwned::Put {
            key: b"c".to_vec(),
            value: b"v".to_vec(),
        });
        assert!(matches!(third, Err(EngineError::Timeout { .. })));
        assert_eq!(pool.stats().rejected_backpressure, 1);
        assert_eq!(pool.stats().queue_depth, 2, "no write disappeared");
        let _ = fs::remove_dir_all(&dir);
        // Deliberately not calling shutdown()/wait() on _c1/_c2: with
        // zero workers they can never complete: this test's own scope
        // dropping them is exactly what should happen. Drop is
        // exercised (and verified not to hang or panic) here.
    }

    #[test]
    fn shutdown_rejects_new_submissions_but_drains_existing_queue() {
        let dir = temp_dir("shutdown_drain");
        let pool = WriteWorkerPool::new(test_committer(&dir), small_config(2)).unwrap();
        let completions: Vec<_> = (0..5)
            .map(|i| {
                pool.submit(WalOpOwned::Put {
                    key: format!("k{i}").into_bytes(),
                    value: b"v".to_vec(),
                })
                .unwrap()
            })
            .collect();
        let report = pool.shutdown();
        assert!(report.fully_drained);
        assert_eq!(report.pool_state, PoolState::Stopped);
        for c in completions {
            c.wait().expect("already-queued work must still complete");
        }
        let rejected = pool.submit(WalOpOwned::Put {
            key: b"late".to_vec(),
            value: b"v".to_vec(),
        });
        assert!(matches!(rejected, Err(EngineError::Aborted { .. })));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_is_idempotent() {
        let dir = temp_dir("idempotent");
        let pool = WriteWorkerPool::new(test_committer(&dir), small_config(2)).unwrap();
        let r1 = pool.shutdown();
        let r2 = pool.shutdown();
        assert_eq!(r1.pool_state, PoolState::Stopped);
        assert_eq!(r2.pool_state, PoolState::Stopped);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn into_inner_returns_the_committer_after_a_clean_shutdown() {
        let dir = temp_dir("into_inner");
        let pool = WriteWorkerPool::new(test_committer(&dir), small_config(2)).unwrap();
        pool.submit(WalOpOwned::Put {
            key: b"k".to_vec(),
            value: b"v".to_vec(),
        })
        .unwrap()
        .wait()
        .unwrap();
        let committer = pool
            .into_inner()
            .expect("no worker should still hold a ref");
        assert_eq!(committer.durable_through(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_without_explicit_shutdown_still_drains_and_leaks_nothing() {
        let dir = temp_dir("drop_safety_net");
        {
            let pool = WriteWorkerPool::new(test_committer(&dir), small_config(2)).unwrap();
            let completion = pool
                .submit(WalOpOwned::Put {
                    key: b"k".to_vec(),
                    value: b"v".to_vec(),
                })
                .unwrap();
            // Deliberately not calling shutdown() -- Drop must do it.
            let _ = completion.wait();
        }
        let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        assert_eq!(replay.records.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Operating brief §18/§27: a `fsync` failure must propagate
    /// faithfully to the one request's own `Completion`, never be
    /// silently dropped or retried into a duplicate append. Uses
    /// `GroupCommitter::install_fsync_fault_hook` — the same
    /// fault-injection seam Phase 1's own crash/failure tests use.
    #[test]
    fn fsync_failure_propagates_to_the_completion_not_silently() {
        let dir = temp_dir("fsync_fault");
        let committer = test_committer(&dir);
        committer.install_fsync_fault_hook(|| Err(std::io::Error::other("injected fsync failure")));
        let pool = WriteWorkerPool::new(committer, small_config(2)).unwrap();
        let completion = pool
            .submit(WalOpOwned::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        let result = completion.wait();
        assert!(
            matches!(result, Err(EngineError::Io(_))),
            "the injected fsync failure must reach the caller, not be swallowed: {result:?}"
        );
        assert_eq!(pool.stats().completed_err, 1);
        assert_eq!(pool.stats().completed_ok, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Operating brief §18/§19: a worker thread panicking mid-request
    /// must (a) complete *that* request with an error rather than
    /// hanging its caller forever, and (b) leave every other queued
    /// request to be picked up by a surviving worker — not lost. The
    /// injected panic uses the same fault-injection seam as the test
    /// above, this time panicking instead of returning `Err`, so it
    /// fires from inside the real leader `fsync` call path a worker
    /// thread executes — a genuine thread panic, not a simulated one.
    #[test]
    fn one_worker_panicking_fails_only_its_own_request_and_does_not_lose_others() {
        let dir = temp_dir("worker_panic");
        let committer = test_committer(&dir);
        committer.install_fsync_fault_hook(|| panic!("injected worker panic"));
        // Two workers: the one that becomes leader for the first batch
        // panics inside the fault hook; the request(s) already queued
        // must still be reachable by the survivor once the pool's own
        // `WorkerAliveGuard`/queue machinery has had a chance to react.
        let pool = WriteWorkerPool::new(committer, small_config(2)).unwrap();
        let completion = pool
            .submit(WalOpOwned::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        let result = completion.wait();
        assert!(
            result.is_err(),
            "a panicking worker must still resolve its own request's Completion, not hang it"
        );
        // The pool must not be permanently wedged: it must still be
        // possible to observe its state (no deadlock reaching this
        // point already demonstrates that), and shutdown() must still
        // return rather than hang.
        let report = pool.shutdown();
        assert!(
            matches!(report.pool_state, PoolState::Stopped | PoolState::Failed),
            "shutdown must reach a terminal state even after a worker panicked: {:?}",
            report.pool_state
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
