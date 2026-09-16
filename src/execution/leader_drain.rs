//! Approach A (`PHASE2B_ARCHITECTURE_A.md`): **Leader Queue Drain**.
//!
//! ```text
//!   Logical callers (10s/100s/1,000+)
//!           |  submit(op) -> Completion
//!           v
//!   bounded queue (Mutex<VecDeque> + 2 Condvars: not_empty, not_full)
//!           |  ONE worker drains the ENTIRE currently-available queue
//!           |  at once (not one entry at a time — see "# Why this
//!           |  differs from the rejected Phase 2 WriteWorkerPool")
//!           v
//!   N worker threads, each capable of becoming a drain-leader
//!           |  append() every drained entry (fast, sequential,
//!           |  microseconds each), then ONE await_durable() for the
//!           |  whole drained batch
//!           v
//!   Arc<GroupCommitter>  --------> WAL
//! ```
//!
//! # Hypothesis under test
//!
//! `PHASE2_ADR.md` ADR-P2-5: the rejected `WriteWorkerPool` capped batch
//! size at `worker_count` because each worker processed exactly one
//! request per loop iteration — a `GroupCommitter` batch can only ever
//! contain requests that have already reached `append()`, and only
//! `worker_count` requests were ever "in" `append()` at once. This
//! module tests the ADR's own named alternative: let whichever worker
//! is currently active **drain every request already queued** (not just
//! one) into its own batch before syncing. A single thread's `append()`
//! call costs microseconds (`FINAL_WAL_ANALYSIS.md` §6/§14: 2.9–9.3µs
//! per record, single-threaded, no lock contention within one thread's
//! own sequential loop) — draining and appending hundreds of queued
//! records sequentially is therefore cheap relative to the ~4–6ms
//! `fsync` the batch will pay regardless, so a *small* number of
//! draining workers can expose a *large* number of records to
//! `GroupCommitter`'s batching window, without needing one OS thread per
//! logical writer (Phase 1's own model) or one OS thread per in-flight
//! request (Phase 2's rejected model).
//!
//! # Why this differs from the rejected Phase 2 `WriteWorkerPool`
//!
//! The only structural change from `write_pool.rs` is **what a worker
//! does per loop iteration**: `write_pool`'s worker pops exactly one
//! `QueueEntry` and calls `append_durable` (append + wait) on it alone.
//! This module's worker pops **every** entry currently in the queue (a
//! `VecDeque::drain(..)` under the same lock, not a repeated single-pop
//! loop — see `drain_available`) into a local batch, appends each one,
//! and issues exactly one `await_durable` call for the whole batch. The
//! queue, completion mechanism (`super::common`), bounded backpressure,
//! and shutdown state machine are otherwise unchanged from that module's
//! own design — this is a deliberate, minimal, single-variable change so
//! the resulting measurement can be attributed to the one thing that
//! changed, not confounded with unrelated redesign.
//!
//! # Ordering, durability, and ownership
//!
//! Identical to `write_pool.rs`'s own "# Ordering"/"# Memory ownership"
//! sections — this module reuses those arguments verbatim rather than
//! restating them: `GroupCommitter::append` remains the sole assigner of
//! `seq`; queue order is not sequence order; every request's bytes are
//! copied exactly once (at `submit()`); no sequence-allocation logic
//! exists here at all.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub use super::common::{Completion, RequestId};
use super::common::{CompletionGuard, CompletionSlot};
use crate::error::{EngineError, Result};
use crate::wal::group_commit::{estimate_frame_len, GroupCommitStats, ShutdownReport};
use crate::wal::{GroupCommitter, WalOpOwned, WalPosition};

#[derive(Debug, Clone)]
pub struct LeaderDrainConfig {
    pub worker_count: usize,
    pub queue_capacity: usize,
    pub max_queued_bytes: usize,
    pub submission_timeout: Duration,
    pub shutdown_drain_bound: Duration,
    /// Total extra wall-clock budget spent retrying `await_durable` on
    /// `Timeout` for one drained batch before giving up — same
    /// rationale as `write_pool::WriteWorkerPoolConfig::await_retry_
    /// budget` (`PHASE2_ADR.md` ADR-P2-4): retrying a pure wait can
    /// never duplicate a record, since `append` for every entry in the
    /// batch has already completed before this retry loop begins.
    pub await_retry_budget: Duration,
    /// Upper bound on how many entries one drain step will take from the
    /// queue in a single pass, purely as a fairness/latency-variance
    /// safety valve (an unbounded drain under extreme sustained load
    /// could in principle keep one worker busy appending for an
    /// unusually long stretch before it ever calls `await_durable`) —
    /// not a throughput lever; `PHASE2_TEST_RESULTS.md` measures whether
    /// this value matters in practice.
    pub max_drain_per_batch: usize,
}

impl Default for LeaderDrainConfig {
    fn default() -> Self {
        LeaderDrainConfig {
            worker_count: 8,
            queue_capacity: 8192,
            max_queued_bytes: 64 * 1024 * 1024,
            submission_timeout: Duration::from_secs(2),
            shutdown_drain_bound: Duration::from_secs(30),
            await_retry_budget: Duration::from_secs(5),
            max_drain_per_batch: 4096,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolState {
    Running,
    Draining,
    Stopped,
    Failed,
}

#[derive(Debug, Clone)]
pub struct LeaderDrainStats {
    pub state: PoolState,
    pub submitted: u64,
    pub completed_ok: u64,
    pub completed_err: u64,
    pub rejected_backpressure: u64,
    pub queue_depth: usize,
    pub queued_bytes: usize,
    pub workers_alive: usize,
    pub queue_wait_ns_total: u64,
    pub processing_ns_total: u64,
    /// Number of drain steps that pulled more than one entry — the
    /// direct, measured signal of whether draining is actually
    /// happening, not just single-item pops in disguise.
    pub drain_batches: u64,
    /// Sum of every drain step's entry count (`drain_batches` > 0 ⇒
    /// `drain_entries_total / drain_batches` is the mean drain size).
    pub drain_entries_total: u64,
    pub committer_stats: GroupCommitStats,
}

#[derive(Debug, Clone)]
pub struct ShutdownReportLD {
    pub fully_drained: bool,
    pub pool_state: PoolState,
    pub committer_report: Option<ShutdownReport>,
    pub queue_depth_at_shutdown: usize,
}

struct QueueEntry {
    op: WalOpOwned,
    completion: Arc<CompletionSlot>,
    approx_bytes: usize,
    enqueued_at: Instant,
}

struct QueueState {
    entries: VecDeque<QueueEntry>,
    queued_bytes: usize,
    pool_state: PoolState,
    workers_alive: usize,
    /// **Attempt A2** (`PHASE2B_ARCHITECTURE_A.md`): at most one worker
    /// actively drains/processes a batch at a time. Attempt A1's
    /// benchmark showed `worker_count > 1` measurably *fragmenting*
    /// throughput at 1,000 writers (worker_count=1: 85–98K ops/sec;
    /// worker_count=2, no coordination: 66–70K) — every worker capable
    /// of draining concurrently splits one large batch into several
    /// smaller ones. This flag turns extra workers into hot standbys
    /// (redundancy if the active drainer dies) rather than concurrent
    /// drainers (which cost throughput here) — see `DrainLeaderGuard`.
    draining_active: bool,
}

struct PoolShared {
    queue: Mutex<QueueState>,
    not_empty: Condvar,
    not_full: Condvar,
    next_request_id: AtomicU64,
    submitted: std::sync::atomic::AtomicU64,
    completed_ok: std::sync::atomic::AtomicU64,
    completed_err: std::sync::atomic::AtomicU64,
    rejected_backpressure: std::sync::atomic::AtomicU64,
    queue_wait_ns_total: std::sync::atomic::AtomicU64,
    processing_ns_total: std::sync::atomic::AtomicU64,
    drain_batches: std::sync::atomic::AtomicU64,
    drain_entries_total: std::sync::atomic::AtomicU64,
}

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
                guard.pool_state = PoolState::Stopped;
            } else if guard.pool_state != PoolState::Stopped {
                guard.pool_state = PoolState::Failed;
                while let Some(entry) = guard.entries.pop_front() {
                    guard.queued_bytes = guard.queued_bytes.saturating_sub(entry.approx_bytes);
                    entry.completion.complete(Err(EngineError::WalUnavailable {
                        detail: "LeaderDrainPool: every worker thread terminated \
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

pub struct LeaderDrainPool {
    shared: Arc<PoolShared>,
    committer: Arc<GroupCommitter>,
    config: LeaderDrainConfig,
    join_handles: Mutex<Option<Vec<JoinHandle<()>>>>,
}

impl LeaderDrainPool {
    pub fn new(committer: GroupCommitter, config: LeaderDrainConfig) -> Result<Self> {
        let shared = Arc::new(PoolShared {
            queue: Mutex::new(QueueState {
                entries: VecDeque::new(),
                queued_bytes: 0,
                pool_state: PoolState::Running,
                workers_alive: 0,
                draining_active: false,
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
            drain_batches: std::sync::atomic::AtomicU64::new(0),
            drain_entries_total: std::sync::atomic::AtomicU64::new(0),
        });
        let committer = Arc::new(committer);

        let mut handles = Vec::with_capacity(config.worker_count);
        for i in 0..config.worker_count {
            let shared_clone = Arc::clone(&shared);
            let committer_clone = Arc::clone(&committer);
            let await_retry_budget = config.await_retry_budget;
            let max_drain_per_batch = config.max_drain_per_batch;
            let spawn_result = thread::Builder::new()
                .name(format!("rubixdb-leader-drain-{i}"))
                .spawn(move || {
                    worker_loop(
                        shared_clone,
                        committer_clone,
                        await_retry_budget,
                        max_drain_per_batch,
                    )
                });
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
                    let pool = LeaderDrainPool {
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

        Ok(LeaderDrainPool {
            shared,
            committer,
            config,
            join_handles: Mutex::new(Some(handles)),
        })
    }

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
                        detail: "LeaderDrainPool is shutting down; new submissions are \
                                 rejected"
                            .to_string(),
                    });
                }
                PoolState::Failed => {
                    self.shared
                        .rejected_backpressure
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(EngineError::WalUnavailable {
                        detail: "LeaderDrainPool has failed (every worker thread \
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
        guard.queued_bytes += approx_bytes;
        drop(guard);
        self.shared.submitted.fetch_add(1, Ordering::Relaxed);
        self.shared.not_empty.notify_one();

        Ok(Completion {
            slot: completion_slot,
            request_id,
        })
    }

    /// Same three-step sequence as `write_pool::WriteWorkerPool::shutdown`
    /// — see that method's doc comment for the full rationale (unchanged
    /// here: draining must fully finish, and every worker must have
    /// exited, before the underlying `GroupCommitter` is told to shut
    /// down, or every legitimately-pre-shutdown queued request's own
    /// `append()` would fail immediately).
    pub fn shutdown(&self) -> ShutdownReportLD {
        {
            let mut guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            match guard.pool_state {
                PoolState::Stopped | PoolState::Failed => {
                    return ShutdownReportLD {
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
        if workers_alive == 0 && guard.pool_state == PoolState::Draining {
            guard.pool_state = PoolState::Stopped;
        }
        let pool_state = guard.pool_state;
        drop(guard);

        let committer_report = if workers_alive == 0 {
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

        ShutdownReportLD {
            fully_drained: workers_alive == 0,
            pool_state,
            committer_report,
            queue_depth_at_shutdown,
        }
    }

    pub fn stats(&self) -> LeaderDrainStats {
        let guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        let queue_depth = guard.entries.len();
        let queued_bytes = guard.queued_bytes;
        let state = guard.pool_state;
        let workers_alive = guard.workers_alive;
        drop(guard);
        LeaderDrainStats {
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
            drain_batches: self.shared.drain_batches.load(Ordering::Relaxed),
            drain_entries_total: self.shared.drain_entries_total.load(Ordering::Relaxed),
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

    pub fn into_inner(self) -> Result<GroupCommitter> {
        self.shutdown();
        let committer = Arc::clone(&self.committer);
        drop(self);
        Arc::try_unwrap(committer).map_err(|_| EngineError::WalUnavailable {
            detail: "LeaderDrainPool::into_inner: the underlying GroupCommitter still had \
                     outstanding references after shutdown() drained every worker"
                .to_string(),
        })
    }
}

impl Drop for LeaderDrainPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Pops every entry currently in `guard.entries` (up to `max_drain`),
/// under the caller's already-held lock — a single `VecDeque::drain`
/// call, not a loop of individual `pop_front`s, so the whole drain is
/// one O(n) operation rather than n lock-protected steps (the lock is
/// already held for the whole call either way; this only affects
/// constant-factor cost, not lock hold duration relative to a pop-loop).
fn drain_available(guard: &mut QueueState, max_drain: usize) -> Vec<QueueEntry> {
    let take = guard.entries.len().min(max_drain.max(1));
    let drained: Vec<QueueEntry> = guard.entries.drain(..take).collect();
    let drained_bytes: usize = drained.iter().map(|e| e.approx_bytes).sum();
    guard.queued_bytes = guard.queued_bytes.saturating_sub(drained_bytes);
    drained
}

/// RAII guard held by whichever worker currently holds `QueueState::
/// draining_active` — see that field's own doc comment. `Drop` always
/// clears the flag and wakes standby workers, whether this worker
/// finishes normally or panics mid-batch (the same "never leave shared
/// coordination state stuck" reasoning as `WorkerAliveGuard`/
/// `CompletionGuard` elsewhere in this module): without this, a drain-
/// leader that panics would leave `draining_active` stuck `true`
/// forever, and no standby worker could ever take over — a *second*,
/// coordination-level version of the exact "state left wedged by an
/// unwinding panic" class of bug `CompletionGuard` already exists to
/// prevent at the per-request level.
struct DrainLeaderGuard<'a> {
    shared: &'a PoolShared,
}

impl Drop for DrainLeaderGuard<'_> {
    fn drop(&mut self) {
        let mut guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        guard.draining_active = false;
        drop(guard);
        self.shared.not_empty.notify_all();
    }
}

fn worker_loop(
    shared: Arc<PoolShared>,
    committer: Arc<GroupCommitter>,
    await_retry_budget: Duration,
    max_drain_per_batch: usize,
) {
    let _alive_guard = WorkerAliveGuard {
        shared: Arc::clone(&shared),
    };
    loop {
        let batch = {
            let mut guard = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if !guard.entries.is_empty() && !guard.draining_active {
                    guard.draining_active = true;
                    break drain_available(&mut guard, max_drain_per_batch);
                }
                if guard.entries.is_empty()
                    && matches!(
                        guard.pool_state,
                        PoolState::Draining | PoolState::Stopped | PoolState::Failed
                    )
                {
                    break Vec::new();
                }
                // Either the queue is empty (and not shutting down — go
                // back to sleep) or it's non-empty but another worker is
                // already the active drain-leader (a standby: also go
                // back to sleep, woken again by that leader's own
                // `DrainLeaderGuard::drop` once it finishes its batch).
                guard = shared
                    .not_empty
                    .wait(guard)
                    .unwrap_or_else(|p| p.into_inner());
            }
        };
        if batch.is_empty() {
            // Only reachable via the shutdown-and-empty exit path above
            // — `draining_active` was never set on this path, so no
            // `DrainLeaderGuard` needs to run.
            return;
        }
        // Held for the whole batch: releases and wakes standbys via its
        // own `Drop`, including if `process_batch` below panics.
        let _leader_guard = DrainLeaderGuard { shared: &shared };
        shared.not_full.notify_all();
        process_batch(batch, &committer, &shared, await_retry_budget);
    }
}

/// Reconstructs an equivalent `EngineError` for a *different* request
/// than the one that originally observed `e` — `EngineError` does not
/// implement `Clone` (`std::io::Error` inside `Io(_)` cannot be cloned
/// portably), so every entry sharing one batch-wide outcome after the
/// first needs its own, freshly-constructed value carrying the same
/// information rather than a literal copy.
fn respread_error(e: &EngineError) -> EngineError {
    EngineError::WalUnavailable {
        detail: format!("leader-drain batch outcome (shared across this batch): {e}"),
    }
}

/// Processes one drained batch: `append()` every entry (stopping at the
/// first failure — see below), then **one** `await_durable` call for the
/// whole batch's highest assigned `seq` (durable_through is monotone, so
/// one wait covers every lower `seq` in the same call — `PHASE1_GROUP_
/// COMMIT.md` §2), then delivers each entry's own result.
///
/// If `append` fails partway through the batch (only realistically
/// reachable via a shutdown racing this drain, or a genuine I/O error —
/// `GroupCommitter::append` does not itself check `poisoned`, only
/// `await_durable` does), entries already appended before the failure
/// still have real, assigned sequence numbers and are awaited normally;
/// the failing entry and every entry after it in this batch (never
/// attempted) are completed immediately with an error — safe, since
/// "never attempted" means "definitely not appended," no wait needed.
fn process_batch(
    batch: Vec<QueueEntry>,
    committer: &GroupCommitter,
    shared: &PoolShared,
    await_retry_budget: Duration,
) {
    let batch_len = batch.len() as u64;
    shared.drain_batches.fetch_add(1, Ordering::Relaxed);
    shared
        .drain_entries_total
        .fetch_add(batch_len, Ordering::Relaxed);

    let started = Instant::now();
    let mut appended: Vec<(QueueEntry, WalPosition)> = Vec::with_capacity(batch.len());
    let mut batch_iter = batch.into_iter();
    for entry in batch_iter.by_ref() {
        let queue_wait_ns =
            u64::try_from(entry.enqueued_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
        shared
            .queue_wait_ns_total
            .fetch_add(queue_wait_ns, Ordering::Relaxed);
        match committer.append(entry.op.as_wal_op()) {
            Ok(position) => appended.push((entry, position)),
            Err(e) => {
                let guard = CompletionGuard::new(&entry.completion);
                shared.completed_err.fetch_add(1, Ordering::Relaxed);
                guard.complete(Err(e));
                break;
            }
        }
    }
    // Anything left in `batch_iter` was never attempted (the loop above
    // broke out on the first append failure) — fail it immediately, no
    // wait needed, per this function's own doc comment.
    for entry in batch_iter {
        let guard = CompletionGuard::new(&entry.completion);
        shared.completed_err.fetch_add(1, Ordering::Relaxed);
        guard.complete(Err(EngineError::Aborted {
            detail: "leader-drain batch: an earlier entry in this same batch failed to \
                     append; this entry was never attempted"
                .to_string(),
        }));
    }

    if let Some((_, max_position)) = appended.last() {
        let max_seq = max_position.seq;
        // Every guard is created **before** the one shared `await_
        // durable` call, and kept alive across it — not created fresh
        // per entry afterward. If that call itself panics (e.g. a fault
        // hook injected for testing, or in principle any bug reachable
        // from inside `GroupCommitter`), every entry in this batch still
        // resolves via its own guard's `Drop` fallback as the panic
        // unwinds past this whole function — not just "whichever entry
        // happened to be processed last," since there is no such thing
        // here: one call covers the entire batch. Building the guards
        // *after* the call (the first version of this function did) left
        // every entry in the batch with no guard at all at the moment a
        // panic could occur — found by this module's own `one_worker_
        // panicking_fails_only_its_own_request_and_does_not_lose_others`
        // test hanging; see `PHASE2B_TEST_RESULTS`/ADR for the account.
        let guards: Vec<CompletionGuard> = appended
            .iter()
            .map(|(entry, _)| CompletionGuard::new(&entry.completion))
            .collect();
        let outcome = append_batch_await_durable_retrying(committer, max_seq, await_retry_budget);
        for ((_, position), guard) in appended.iter().zip(guards) {
            let result = match &outcome {
                Ok(()) => Ok(*position),
                Err(e) => Err(respread_error(e)),
            };
            match &result {
                Ok(_) => shared.completed_ok.fetch_add(1, Ordering::Relaxed),
                Err(_) => shared.completed_err.fetch_add(1, Ordering::Relaxed),
            };
            guard.complete(result);
        }
    }

    // Recorded once per *batch*, not multiplied by `batch_len`: dividing
    // this total by the completed-request count elsewhere yields each
    // request's fair share of the batch's total processing cost, which
    // is what "mean processing time" should mean here — multiplying by
    // `batch_len` would double-count it.
    let processing_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    shared
        .processing_ns_total
        .fetch_add(processing_ns, Ordering::Relaxed);
}

/// `await_durable`, retrying only on `Timeout`, up to `retry_budget` —
/// same pattern and rationale as `write_pool::append_then_await_
/// durable_retrying` (`PHASE2_ADR.md` ADR-P2-4), applied here to one
/// shared `seq` covering an entire drained batch rather than one
/// request.
fn append_batch_await_durable_retrying(
    committer: &GroupCommitter,
    max_seq: u64,
    retry_budget: Duration,
) -> Result<()> {
    let deadline = Instant::now() + retry_budget;
    loop {
        match committer.await_durable(max_seq) {
            Ok(()) => return Ok(()),
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
        let path = std::env::temp_dir().join(format!("rubixdb_leader_drain_ut_{tag}_{nanos}"));
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

    fn small_config(worker_count: usize) -> LeaderDrainConfig {
        LeaderDrainConfig {
            worker_count,
            queue_capacity: 8,
            max_queued_bytes: 1024 * 1024,
            submission_timeout: Duration::from_millis(500),
            shutdown_drain_bound: Duration::from_secs(5),
            await_retry_budget: Duration::from_secs(2),
            max_drain_per_batch: 4096,
        }
    }

    #[test]
    fn single_submit_completes_durably_and_recovers() {
        let dir = temp_dir("single");
        let pool = LeaderDrainPool::new(test_committer(&dir), small_config(2)).unwrap();
        let completion = pool
            .submit(WalOpOwned::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        let position = completion.wait().unwrap();
        assert_eq!(position.seq, 1);
        let report = pool.shutdown();
        assert!(report.fully_drained);
        assert_eq!(report.pool_state, PoolState::Stopped);
        drop(pool);

        let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        assert!(replay.corrupted_segments.is_empty());
        assert_eq!(replay.records.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    fn submit_retrying(pool: &LeaderDrainPool, key: &[u8], value: &[u8]) -> Completion {
        const MAX_RETRIES: u32 = 200;
        for _ in 0..MAX_RETRIES {
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
        let pool = Arc::new(LeaderDrainPool::new(test_committer(&dir), small_config(4)).unwrap());
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
        assert!(
            stats.drain_batches > 0 && stats.drain_entries_total >= stats.drain_batches,
            "drain stats must reflect real batching activity: {stats:?}"
        );

        let report = pool.shutdown();
        assert!(report.fully_drained);
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
        let config = LeaderDrainConfig {
            worker_count: 0,
            queue_capacity: 2,
            max_queued_bytes: 1024 * 1024,
            submission_timeout: Duration::from_millis(100),
            shutdown_drain_bound: Duration::from_secs(5),
            await_retry_budget: Duration::from_secs(2),
            max_drain_per_batch: 4096,
        };
        let pool = LeaderDrainPool::new(test_committer(&dir), config).unwrap();
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
        assert_eq!(pool.stats().queue_depth, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_is_idempotent() {
        let dir = temp_dir("idempotent");
        let pool = LeaderDrainPool::new(test_committer(&dir), small_config(2)).unwrap();
        let r1 = pool.shutdown();
        let r2 = pool.shutdown();
        assert_eq!(r1.pool_state, PoolState::Stopped);
        assert_eq!(r2.pool_state, PoolState::Stopped);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fsync_failure_propagates_to_every_entry_in_the_batch() {
        let dir = temp_dir("fsync_fault");
        let committer = test_committer(&dir);
        committer.install_fsync_fault_hook(|| Err(std::io::Error::other("injected fsync failure")));
        let pool = LeaderDrainPool::new(committer, small_config(2)).unwrap();
        let c1 = pool
            .submit(WalOpOwned::Put {
                key: b"k1".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        let c2 = pool
            .submit(WalOpOwned::Put {
                key: b"k2".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        assert!(c1.wait().is_err());
        assert!(c2.wait().is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_worker_panicking_fails_only_its_own_request_and_does_not_lose_others() {
        let dir = temp_dir("worker_panic");
        let committer = test_committer(&dir);
        committer.install_fsync_fault_hook(|| panic!("injected worker panic"));
        let pool = LeaderDrainPool::new(committer, small_config(2)).unwrap();
        let completion = pool
            .submit(WalOpOwned::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        assert!(completion.wait().is_err());
        // **Phase 3 P0 fix** (`src/wal/group_commit.rs`'s `LeaderFailureGuard`,
        // `PHASE3_FAILURE_MODEL.md`): a leader that panics mid-`fsync`
        // (this test's injected fault) no longer leaves `GroupCommitter`'s
        // `leader_active` permanently `true`. The guard's `Drop` clears it
        // and poisons the committer during the unwind itself, so `pool.
        // shutdown()` below now returns promptly instead of paying the
        // full fixed `SHUTDOWN_DRAIN_BOUND` (5s) this test used to
        // document as expected, pre-fix, Phase 1 behavior.
        let shutdown_started = Instant::now();
        let report = pool.shutdown();
        assert!(matches!(
            report.pool_state,
            PoolState::Stopped | PoolState::Failed
        ));
        assert!(
            shutdown_started.elapsed() < Duration::from_secs(1),
            "shutdown must return promptly once the leader-panic guard has cleared \
             leader_active — no more waiting out SHUTDOWN_DRAIN_BOUND, took {:?}",
            shutdown_started.elapsed()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// **Updated for the Phase 3 P0 fix** (`PHASE3_FAILURE_MODEL.md`): a
    /// standby worker (Attempt A2's redundancy) still cannot rescue a
    /// *later* request after the active drain-leader panics mid-`fsync` —
    /// that was never the failure this test documents. What changed is
    /// *why* the second request fails and *how fast*: pre-fix,
    /// `GroupCommitter`'s `leader_active` flag was left permanently
    /// `true`, so every later caller became a follower forever and only
    /// failed after riding out its own timeout. Post-fix,
    /// `LeaderFailureGuard` poisons the `GroupCommitter` the moment the
    /// leader panics, so the second request now fails **immediately**
    /// (a poisoned-committer error, not a timeout) rather than after
    /// `await_retry_budget`. Either way, self-healing still requires
    /// reconstructing the `GroupCommitter` (Phase 1's own documented
    /// recovery model, `PHASE1_GROUP_COMMIT.md` — unchanged here, since
    /// this module has no documented reason to alter it) — this test
    /// locks in that the system behaves *safely* (bounded, not hanging,
    /// not duplicating, not falsely acknowledging) either way.
    #[test]
    fn a_second_request_after_the_leader_panics_still_fails_safely_not_permanently_blocked() {
        let dir = temp_dir("worker_panic_second_request");
        let committer = test_committer(&dir);
        committer.install_fsync_fault_hook(|| panic!("injected worker panic"));
        let config = LeaderDrainConfig {
            await_retry_budget: Duration::from_millis(300),
            ..small_config(2)
        };
        let pool = LeaderDrainPool::new(committer, config).unwrap();

        let first = pool
            .submit(WalOpOwned::Put {
                key: b"k1".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        assert!(
            first.wait().is_err(),
            "the panicking leader's own request must fail, not hang"
        );

        // Give the panicked worker's WorkerAliveGuard/DrainLeaderGuard a
        // moment to finish running (they run during unwind, on that
        // worker's own thread, asynchronously with respect to this one).
        std::thread::sleep(Duration::from_millis(50));

        let second = pool
            .submit(WalOpOwned::Put {
                key: b"k2".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        let second_started = Instant::now();
        let second_result = second.wait();
        assert!(
            second_result.is_err(),
            "a second request cannot become durable once the underlying GroupCommitter is \
             poisoned by the leader panic — it must still fail cleanly, not hang: {second_result:?}"
        );
        assert!(
            second_started.elapsed() < Duration::from_secs(1),
            "post-fix, a poisoned GroupCommitter must fail a new request immediately, not after \
             riding out await_retry_budget — took {:?}",
            second_started.elapsed()
        );

        // This test's own teardown (implicit `pool` `Drop` below) is now
        // fast: `LeaderDrainPool::shutdown()` finalizes the underlying
        // `GroupCommitter` once `workers_alive` reaches 0, and that call
        // no longer waits out `SHUTDOWN_DRAIN_BOUND` (5s) — `leader_active`
        // was already cleared by `LeaderFailureGuard` when the leader
        // panicked. Pre-Phase-3, this teardown cost ~5s; that is no longer
        // expected or correct.
        let _ = fs::remove_dir_all(&dir);
    }
}
