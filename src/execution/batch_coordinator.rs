//! Approach B (`PHASE2B_ARCHITECTURE_B.md`): **Dedicated Batch
//! Coordinator**.
//!
//! ```text
//!   Logical callers (10s/100s/1,000+)
//!           |  submit(op) -> Completion
//!           v
//!   bounded ingress queue (Mutex<VecDeque> + 2 Condvars)
//!           |  drained ONLY by the one coordinator thread
//!           v
//!   The one Batch Coordinator thread
//!           |  drain everything queued, append each (fast), ONE
//!           |  await_durable() for the whole batch, complete all
//!           v
//!   Arc<GroupCommitter>  --------> WAL
//! ```
//!
//! # Hypothesis under test
//!
//! `leader_drain` (Approach A) tests "let whichever worker is active
//! drain everything" with `N` *interchangeable* workers, coordinated at
//! runtime (Attempt A2's `draining_active` flag) so only one is ever
//! active at a time. This module tests a structurally different
//! question: **is that runtime coordination even necessary, or is a
//! design with exactly one dedicated coordinator thread — decided at
//! construction time, not negotiated at runtime — simpler and at least
//! as fast?** Producers/callers never call into `GroupCommitter`
//! themselves and never compete to become a leader; there is exactly
//! one thread in the whole system that ever does either. This directly
//! matches the operating brief's own Approach B framing: separate
//! *request scheduling* (many producer threads, a queue) from *WAL
//! leader execution* (one coordinator) as two structurally distinct
//! roles, rather than one role a variable number of interchangeable
//! workers all can play.
//!
//! # Trade-off this design makes explicitly, by construction
//!
//! **No redundancy.** Unlike `leader_drain` with `worker_count > 1`
//! (Attempt A2), there is no standby thread to take over if the one
//! coordinator dies — by design, there is only ever one. If it panics,
//! the pool transitions to `PoolState::Failed` immediately and stays
//! there (a fresh `BatchCoordinatorPool` — and, per this module's own
//! `PHASE2B_FAILURE_MODEL.md` entry, likely a fresh `GroupCommitter` too
//! — is the only recovery path). This is a deliberate simplicity/
//! availability trade-off this module measures and reports honestly
//! (`PHASE2B_FINAL_TEST_RESULTS.md`), not an oversight — see `PHASE2B_
//! ADR.md` for the explicit decision on whether this trade-off is
//! acceptable relative to Approach A's measured redundancy cost.
//!
//! # Ordering, durability, and ownership
//!
//! Identical to `leader_drain.rs`'s own sections of the same name —
//! `GroupCommitter::append` remains the sole assigner of `seq`; queue
//! order is not sequence order; every request's bytes are copied
//! exactly once (at `submit()`); no sequence-allocation logic exists
//! here at all; the coordinator calls exactly the same `append`/`await_
//! durable` sequence any Phase 1 direct caller already could.

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
pub struct BatchCoordinatorConfig {
    pub queue_capacity: usize,
    pub max_queued_bytes: usize,
    pub submission_timeout: Duration,
    pub shutdown_drain_bound: Duration,
    pub await_retry_budget: Duration,
    pub max_drain_per_batch: usize,
}

impl Default for BatchCoordinatorConfig {
    fn default() -> Self {
        BatchCoordinatorConfig {
            queue_capacity: 8192,
            max_queued_bytes: 64 * 1024 * 1024,
            submission_timeout: Duration::from_secs(2),
            shutdown_drain_bound: Duration::from_secs(30),
            await_retry_budget: Duration::from_secs(5),
            max_drain_per_batch: 65536,
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
pub struct BatchCoordinatorStats {
    pub state: PoolState,
    pub submitted: u64,
    pub completed_ok: u64,
    pub completed_err: u64,
    pub rejected_backpressure: u64,
    pub queue_depth: usize,
    pub queued_bytes: usize,
    pub coordinator_alive: bool,
    pub queue_wait_ns_total: u64,
    pub processing_ns_total: u64,
    pub drain_batches: u64,
    pub drain_entries_total: u64,
    pub committer_stats: GroupCommitStats,
}

#[derive(Debug, Clone)]
pub struct ShutdownReportBC {
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
    /// `true`/`false`, not a count — there is exactly one coordinator
    /// thread by construction (unlike `leader_drain`'s `workers_alive:
    /// usize`, which can be any configured count).
    coordinator_alive: bool,
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

/// Simpler than `leader_drain`'s `WorkerAliveGuard`: with exactly one
/// coordinator, "the coordinator is gone" and "every worker is gone"
/// are the same event — there is no multi-worker case to distinguish
/// ("was this the *last* one?" is always trivially "yes").
struct CoordinatorAliveGuard {
    shared: Arc<PoolShared>,
}

impl Drop for CoordinatorAliveGuard {
    fn drop(&mut self) {
        let mut guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        guard.coordinator_alive = false;
        let requested_shutdown = matches!(guard.pool_state, PoolState::Draining);
        if requested_shutdown {
            guard.pool_state = PoolState::Stopped;
        } else if guard.pool_state != PoolState::Stopped {
            guard.pool_state = PoolState::Failed;
            while let Some(entry) = guard.entries.pop_front() {
                guard.queued_bytes = guard.queued_bytes.saturating_sub(entry.approx_bytes);
                entry.completion.complete(Err(EngineError::WalUnavailable {
                    detail: "BatchCoordinatorPool: the coordinator thread terminated \
                             unexpectedly (panicked) before this request could be \
                             dequeued — this architecture has no standby to take over \
                             (see PHASE2B_ARCHITECTURE_B.md)"
                        .to_string(),
                }));
            }
        }
        drop(guard);
        self.shared.not_empty.notify_all();
        self.shared.not_full.notify_all();
    }
}

pub struct BatchCoordinatorPool {
    shared: Arc<PoolShared>,
    committer: Arc<GroupCommitter>,
    config: BatchCoordinatorConfig,
    join_handle: Mutex<Option<JoinHandle<()>>>,
}

impl BatchCoordinatorPool {
    pub fn new(committer: GroupCommitter, config: BatchCoordinatorConfig) -> Result<Self> {
        let shared = Arc::new(PoolShared {
            queue: Mutex::new(QueueState {
                entries: VecDeque::new(),
                queued_bytes: 0,
                pool_state: PoolState::Running,
                coordinator_alive: false,
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

        let shared_clone = Arc::clone(&shared);
        let committer_clone = Arc::clone(&committer);
        let await_retry_budget = config.await_retry_budget;
        let max_drain_per_batch = config.max_drain_per_batch;
        let spawn_result = thread::Builder::new()
            .name("rubixdb-batch-coordinator".to_string())
            .spawn(move || {
                coordinator_loop(
                    shared_clone,
                    committer_clone,
                    await_retry_budget,
                    max_drain_per_batch,
                )
            });
        let join_handle = match spawn_result {
            Ok(handle) => {
                shared
                    .queue
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .coordinator_alive = true;
                Some(handle)
            }
            Err(io_err) => return Err(EngineError::Io(io_err)),
        };

        Ok(BatchCoordinatorPool {
            shared,
            committer,
            config,
            join_handle: Mutex::new(join_handle),
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
                        detail: "BatchCoordinatorPool is shutting down; new submissions \
                                 are rejected"
                            .to_string(),
                    });
                }
                PoolState::Failed => {
                    self.shared
                        .rejected_backpressure
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(EngineError::WalUnavailable {
                        detail: "BatchCoordinatorPool has failed (the coordinator thread \
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

    /// Same three-step sequence as `leader_drain::LeaderDrainPool::
    /// shutdown`/`write_pool::WriteWorkerPool::shutdown` — see either's
    /// doc comment for the rationale (unchanged: the underlying
    /// `GroupCommitter` is only finalized once the coordinator has
    /// fully exited).
    pub fn shutdown(&self) -> ShutdownReportBC {
        {
            let mut guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            match guard.pool_state {
                PoolState::Stopped | PoolState::Failed => {
                    return ShutdownReportBC {
                        fully_drained: !guard.coordinator_alive,
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
        while guard.coordinator_alive {
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
        let coordinator_alive = guard.coordinator_alive;
        if !coordinator_alive && guard.pool_state == PoolState::Draining {
            guard.pool_state = PoolState::Stopped;
        }
        let pool_state = guard.pool_state;
        drop(guard);

        let committer_report = if !coordinator_alive {
            if let Some(handle) = self
                .join_handle
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
            {
                let _ = handle.join();
            }
            Some(self.committer.shutdown())
        } else {
            None
        };

        ShutdownReportBC {
            fully_drained: !coordinator_alive,
            pool_state,
            committer_report,
            queue_depth_at_shutdown,
        }
    }

    pub fn stats(&self) -> BatchCoordinatorStats {
        let guard = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        let queue_depth = guard.entries.len();
        let queued_bytes = guard.queued_bytes;
        let state = guard.pool_state;
        let coordinator_alive = guard.coordinator_alive;
        drop(guard);
        BatchCoordinatorStats {
            state,
            submitted: self.shared.submitted.load(Ordering::Relaxed),
            completed_ok: self.shared.completed_ok.load(Ordering::Relaxed),
            completed_err: self.shared.completed_err.load(Ordering::Relaxed),
            rejected_backpressure: self.shared.rejected_backpressure.load(Ordering::Relaxed),
            queue_depth,
            queued_bytes,
            coordinator_alive,
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
            detail: "BatchCoordinatorPool::into_inner: the underlying GroupCommitter still \
                     had an outstanding reference after shutdown() drained the coordinator"
                .to_string(),
        })
    }
}

impl Drop for BatchCoordinatorPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn drain_available(guard: &mut QueueState, max_drain: usize) -> Vec<QueueEntry> {
    let take = guard.entries.len().min(max_drain.max(1));
    let drained: Vec<QueueEntry> = guard.entries.drain(..take).collect();
    let drained_bytes: usize = drained.iter().map(|e| e.approx_bytes).sum();
    guard.queued_bytes = guard.queued_bytes.saturating_sub(drained_bytes);
    drained
}

fn coordinator_loop(
    shared: Arc<PoolShared>,
    committer: Arc<GroupCommitter>,
    await_retry_budget: Duration,
    max_drain_per_batch: usize,
) {
    let _alive_guard = CoordinatorAliveGuard {
        shared: Arc::clone(&shared),
    };
    loop {
        let batch = {
            let mut guard = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if !guard.entries.is_empty() {
                    break drain_available(&mut guard, max_drain_per_batch);
                }
                if matches!(
                    guard.pool_state,
                    PoolState::Draining | PoolState::Stopped | PoolState::Failed
                ) {
                    break Vec::new();
                }
                guard = shared
                    .not_empty
                    .wait(guard)
                    .unwrap_or_else(|p| p.into_inner());
            }
        };
        if batch.is_empty() {
            return;
        }
        shared.not_full.notify_all();
        process_batch(batch, &committer, &shared, await_retry_budget);
    }
}

fn respread_error(e: &EngineError) -> EngineError {
    EngineError::WalUnavailable {
        detail: format!("batch-coordinator batch outcome (shared across this batch): {e}"),
    }
}

/// Identical structure to `leader_drain::process_batch` — see that
/// function's doc comment for the full rationale (append every entry
/// sequentially, stop at the first failure; one shared `await_durable`
/// for the whole batch, with every entry's `CompletionGuard` constructed
/// *before* that call so a panic during it still resolves every entry).
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
    for entry in batch_iter {
        let guard = CompletionGuard::new(&entry.completion);
        shared.completed_err.fetch_add(1, Ordering::Relaxed);
        guard.complete(Err(EngineError::Aborted {
            detail: "batch-coordinator batch: an earlier entry in this same batch failed \
                     to append; this entry was never attempted"
                .to_string(),
        }));
    }

    if let Some((_, max_position)) = appended.last() {
        let max_seq = max_position.seq;
        let guards: Vec<CompletionGuard> = appended
            .iter()
            .map(|(entry, _)| CompletionGuard::new(&entry.completion))
            .collect();
        let outcome = await_durable_retrying(committer, max_seq, await_retry_budget);
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

    let processing_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    shared
        .processing_ns_total
        .fetch_add(processing_ns, Ordering::Relaxed);
}

fn await_durable_retrying(
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
        let path = std::env::temp_dir().join(format!("rubixdb_batch_coord_ut_{tag}_{nanos}"));
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

    fn small_config() -> BatchCoordinatorConfig {
        BatchCoordinatorConfig {
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
        let pool = BatchCoordinatorPool::new(test_committer(&dir), small_config()).unwrap();
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

    fn submit_retrying(pool: &BatchCoordinatorPool, key: &[u8], value: &[u8]) -> Completion {
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
        let pool =
            Arc::new(BatchCoordinatorPool::new(test_committer(&dir), small_config()).unwrap());
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
        let config = BatchCoordinatorConfig {
            queue_capacity: 2,
            submission_timeout: Duration::from_millis(50),
            ..small_config()
        };
        let pool = BatchCoordinatorPool::new(test_committer(&dir), config).unwrap();
        // The coordinator drains quickly on its own, so to reliably
        // observe a full queue this test submits a burst larger than
        // capacity from many threads at once rather than relying on
        // timing a single-threaded submit sequence against a live
        // coordinator (which — correctly — would usually just drain it).
        let pool = Arc::new(pool);
        let mut any_rejected = false;
        for _ in 0..20 {
            let mut handles = Vec::new();
            for i in 0..50 {
                let pool = Arc::clone(&pool);
                handles.push(thread::spawn(move || {
                    pool.submit(WalOpOwned::Put {
                        key: format!("k{i}").into_bytes(),
                        value: b"v".to_vec(),
                    })
                }));
            }
            for h in handles {
                if matches!(h.join().unwrap(), Err(EngineError::Timeout { .. })) {
                    any_rejected = true;
                }
            }
            if any_rejected {
                break;
            }
        }
        assert!(
            any_rejected,
            "a burst larger than queue_capacity must eventually observe backpressure, \
             not silently accept unbounded work"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_is_idempotent() {
        let dir = temp_dir("idempotent");
        let pool = BatchCoordinatorPool::new(test_committer(&dir), small_config()).unwrap();
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
        let pool = BatchCoordinatorPool::new(committer, small_config()).unwrap();
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

    /// No standby to fail over to, by design (this architecture's own
    /// explicit trade-off) — a coordinator panic must still fail the
    /// in-flight request safely and transition the pool to `Failed`,
    /// rejecting further work cleanly rather than accepting requests
    /// into a queue nothing will ever drain again.
    #[test]
    fn coordinator_panicking_fails_safely_and_rejects_further_work() {
        let dir = temp_dir("coordinator_panic");
        let committer = test_committer(&dir);
        committer.install_fsync_fault_hook(|| panic!("injected coordinator panic"));
        let pool = BatchCoordinatorPool::new(committer, small_config()).unwrap();
        let completion = pool
            .submit(WalOpOwned::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            })
            .unwrap();
        assert!(completion.wait().is_err());
        std::thread::sleep(Duration::from_millis(50));
        let rejected = pool.submit(WalOpOwned::Put {
            key: b"k2".to_vec(),
            value: b"v".to_vec(),
        });
        let is_expected_rejection = matches!(rejected, Err(EngineError::WalUnavailable { .. }));
        assert!(
            is_expected_rejection,
            "with no standby, the pool must reject further work cleanly once Failed"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
