//! Approach C (`PHASE2B_ARCHITECTURE_C.md`): **Sharded / Per-Core
//! Ingress With Batch Merge**.
//!
//! ```text
//!   Writer Group A ---> Shard Queue 0 ---\
//!   Writer Group B ---> Shard Queue 1 ----\
//!   Writer Group C ---> Shard Queue 2 -----+--> Coordinator --> WAL --> Sync
//!   Writer Group D ---> Shard Queue 3 ----/
//! ```
//!
//! # Scope of this cycle's evaluation (per the operating brief's own §6)
//!
//! The operating brief's own Approach C section is explicitly
//! conditional: *"If A and B fail to reach the required performance,
//! evaluate a third architecture..."* Both Approach A (`leader_drain`)
//! and Approach B (`batch_coordinator`) **met** both throughput targets
//! comfortably (`PHASE2B_FINAL_TEST_RESULTS.md` §A/§B) using a single
//! shared `Mutex<VecDeque>` ingress queue — with no evidence anywhere in
//! either approach's own measurements that this single queue was a
//! contention point (§14 of the brief: *"If the queue becomes the
//! bottleneck, only then evaluate... sharded queues"*). This module is
//! therefore implemented and measured once (**Attempt C1 only** — not
//! the full three-attempt cycle A/B each received) purely to test the
//! sharding hypothesis directly and complete the three-way comparison
//! `PHASE2B_FINAL_TEST_RESULTS.md` §19 requires, not because evidence up
//! to this point suggested sharding was needed. If C1's own measurement
//! shows no material improvement over B (the simpler, unsharded
//! baseline it's most structurally comparable to), that itself is the
//! answer, and no C2/C3 is warranted — see `PHASE2B_ADR.md`.
//!
//! # Design
//!
//! `shard_count` independent bounded queues, each its own `Mutex<VecDeque>`
//! (no cross-shard lock contention on the *producer* side — a writer
//! only ever touches its own assigned shard). Producers are assigned to
//! a shard by a simple, even hash of their `RequestId` allocation order
//! (round-robin via an atomic counter — cheap, no thread-identity lookup
//! needed, and evenly distributes load regardless of how many logical
//! caller threads exist relative to `shard_count`). **One** coordinator
//! thread drains every shard in turn each cycle (round-robin across
//! shards, not just shard 0 repeatedly) into a single merged batch,
//! appends every entry, and issues exactly one `await_durable` for the
//! whole merged batch — preserving the single durability ordering
//! boundary the operating brief requires ("there remains one durability
//! ordering boundary... do not create independent WAL durability
//! domains merely to make the benchmark faster").
//!
//! # Ordering, durability, and ownership
//!
//! Identical to `leader_drain.rs`/`batch_coordinator.rs`'s own sections
//! of the same name. Sharding the *queue* changes nothing about how
//! `seq` is assigned or how durability is proven — `GroupCommitter`
//! remains the sole authority for both, exactly as in every other Phase
//! 2/2B architecture.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub use super::common::{Completion, RequestId};
use super::common::{CompletionGuard, CompletionSlot};
use crate::error::{EngineError, Result};
use crate::wal::group_commit::{estimate_frame_len, GroupCommitStats, ShutdownReport};
use crate::wal::{GroupCommitter, WalOpOwned, WalPosition};

#[derive(Debug, Clone)]
pub struct ShardedIngressConfig {
    pub shard_count: usize,
    /// Per-shard capacity — total queue capacity across all shards is
    /// `shard_count * queue_capacity_per_shard`.
    pub queue_capacity_per_shard: usize,
    pub max_queued_bytes_per_shard: usize,
    pub submission_timeout: Duration,
    pub shutdown_drain_bound: Duration,
    pub await_retry_budget: Duration,
    pub max_drain_per_shard_per_cycle: usize,
}

impl Default for ShardedIngressConfig {
    fn default() -> Self {
        ShardedIngressConfig {
            shard_count: 8,
            queue_capacity_per_shard: 1024,
            max_queued_bytes_per_shard: 8 * 1024 * 1024,
            submission_timeout: Duration::from_secs(2),
            shutdown_drain_bound: Duration::from_secs(30),
            await_retry_budget: Duration::from_secs(5),
            max_drain_per_shard_per_cycle: 8192,
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
pub struct ShardedIngressStats {
    pub state: PoolState,
    pub submitted: u64,
    pub completed_ok: u64,
    pub completed_err: u64,
    pub rejected_backpressure: u64,
    pub queue_depth_total: usize,
    pub queued_bytes_total: usize,
    pub coordinator_alive: bool,
    pub queue_wait_ns_total: u64,
    pub processing_ns_total: u64,
    pub drain_batches: u64,
    pub drain_entries_total: u64,
    pub committer_stats: GroupCommitStats,
}

#[derive(Debug, Clone)]
pub struct ShutdownReportSI {
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

struct ShardState {
    entries: VecDeque<QueueEntry>,
    queued_bytes: usize,
}

/// Coordination state — separate from the `shard_count` independent
/// `Mutex<ShardState>`s (producers never touch this lock at all, only
/// the coordinator does, once per drain cycle) so producer-side
/// submission across different shards never contends on anything beyond
/// its own shard's lock.
struct CoordState {
    pool_state: PoolState,
    coordinator_alive: bool,
}

struct PoolShared {
    shards: Vec<Mutex<ShardState>>,
    /// One `not_full` per shard (a producer only ever waits on its own
    /// shard filling up) and one shared `not_empty`/`coord` state the
    /// coordinator waits on (woken by a submission to *any* shard).
    not_full: Vec<Condvar>,
    not_empty: Condvar,
    coord: Mutex<CoordState>,
    next_shard: AtomicUsize,
    next_request_id: AtomicU64,
    submitted: AtomicU64,
    completed_ok: AtomicU64,
    completed_err: AtomicU64,
    rejected_backpressure: AtomicU64,
    queue_wait_ns_total: AtomicU64,
    processing_ns_total: AtomicU64,
    drain_batches: AtomicU64,
    drain_entries_total: AtomicU64,
}

impl PoolShared {
    fn total_queue_depth_and_bytes(&self) -> (usize, usize) {
        let mut depth = 0;
        let mut bytes = 0;
        for shard in &self.shards {
            let guard = shard.lock().unwrap_or_else(|p| p.into_inner());
            depth += guard.entries.len();
            bytes += guard.queued_bytes;
        }
        (depth, bytes)
    }
}

struct CoordinatorAliveGuard {
    shared: Arc<PoolShared>,
}

impl Drop for CoordinatorAliveGuard {
    fn drop(&mut self) {
        let mut coord = self.shared.coord.lock().unwrap_or_else(|p| p.into_inner());
        coord.coordinator_alive = false;
        let requested_shutdown = matches!(coord.pool_state, PoolState::Draining);
        if requested_shutdown {
            coord.pool_state = PoolState::Stopped;
        } else if coord.pool_state != PoolState::Stopped {
            coord.pool_state = PoolState::Failed;
            for shard in &self.shared.shards {
                let mut guard = shard.lock().unwrap_or_else(|p| p.into_inner());
                while let Some(entry) = guard.entries.pop_front() {
                    guard.queued_bytes = guard.queued_bytes.saturating_sub(entry.approx_bytes);
                    entry.completion.complete(Err(EngineError::WalUnavailable {
                        detail: "ShardedIngressPool: the coordinator thread terminated \
                                 unexpectedly (panicked) before this request could be \
                                 dequeued"
                            .to_string(),
                    }));
                }
            }
        }
        drop(coord);
        self.shared.not_empty.notify_all();
        for nf in &self.shared.not_full {
            nf.notify_all();
        }
    }
}

pub struct ShardedIngressPool {
    shared: Arc<PoolShared>,
    committer: Arc<GroupCommitter>,
    config: ShardedIngressConfig,
    join_handle: Mutex<Option<JoinHandle<()>>>,
}

impl ShardedIngressPool {
    pub fn new(committer: GroupCommitter, config: ShardedIngressConfig) -> Result<Self> {
        let shard_count = config.shard_count.max(1);
        let shards = (0..shard_count)
            .map(|_| {
                Mutex::new(ShardState {
                    entries: VecDeque::new(),
                    queued_bytes: 0,
                })
            })
            .collect();
        let not_full = (0..shard_count).map(|_| Condvar::new()).collect();
        let shared = Arc::new(PoolShared {
            shards,
            not_full,
            not_empty: Condvar::new(),
            coord: Mutex::new(CoordState {
                pool_state: PoolState::Running,
                coordinator_alive: false,
            }),
            next_shard: AtomicUsize::new(0),
            next_request_id: AtomicU64::new(1),
            submitted: AtomicU64::new(0),
            completed_ok: AtomicU64::new(0),
            completed_err: AtomicU64::new(0),
            rejected_backpressure: AtomicU64::new(0),
            queue_wait_ns_total: AtomicU64::new(0),
            processing_ns_total: AtomicU64::new(0),
            drain_batches: AtomicU64::new(0),
            drain_entries_total: AtomicU64::new(0),
        });
        let committer = Arc::new(committer);

        let shared_clone = Arc::clone(&shared);
        let committer_clone = Arc::clone(&committer);
        let await_retry_budget = config.await_retry_budget;
        let max_drain_per_shard = config.max_drain_per_shard_per_cycle;
        let spawn_result = thread::Builder::new()
            .name("rubixdb-sharded-coordinator".to_string())
            .spawn(move || {
                coordinator_loop(
                    shared_clone,
                    committer_clone,
                    await_retry_budget,
                    max_drain_per_shard,
                )
            });
        let join_handle = match spawn_result {
            Ok(handle) => {
                shared
                    .coord
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .coordinator_alive = true;
                Some(handle)
            }
            Err(io_err) => return Err(EngineError::Io(io_err)),
        };

        Ok(ShardedIngressPool {
            shared,
            committer,
            config,
            join_handle: Mutex::new(join_handle),
        })
    }

    pub fn submit(&self, op: WalOpOwned) -> Result<Completion> {
        let approx_bytes = estimate_frame_len(&op.as_wal_op());
        let shard_idx =
            self.shared.next_shard.fetch_add(1, Ordering::Relaxed) % self.shared.shards.len();
        let deadline = Instant::now() + self.config.submission_timeout;

        {
            let coord = self.shared.coord.lock().unwrap_or_else(|p| p.into_inner());
            match coord.pool_state {
                PoolState::Draining | PoolState::Stopped => {
                    self.shared
                        .rejected_backpressure
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(EngineError::Aborted {
                        detail: "ShardedIngressPool is shutting down; new submissions are \
                                 rejected"
                            .to_string(),
                    });
                }
                PoolState::Failed => {
                    self.shared
                        .rejected_backpressure
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(EngineError::WalUnavailable {
                        detail: "ShardedIngressPool has failed (the coordinator thread \
                                 terminated unexpectedly); no new submissions are accepted"
                            .to_string(),
                    });
                }
                PoolState::Running => {}
            }
        }

        let mut guard = self.shared.shards[shard_idx]
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        loop {
            let has_capacity = guard.entries.len() < self.config.queue_capacity_per_shard
                && guard.queued_bytes.saturating_add(approx_bytes)
                    <= self.config.max_queued_bytes_per_shard;
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
                        "submit() timed out after {:?} waiting for capacity on shard {shard_idx} \
                         (depth={}/{}, bytes={}/{})",
                        self.config.submission_timeout,
                        guard.entries.len(),
                        self.config.queue_capacity_per_shard,
                        guard.queued_bytes,
                        self.config.max_queued_bytes_per_shard
                    ),
                });
            }
            let (new_guard, _) = self.shared.not_full[shard_idx]
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

    pub fn shutdown(&self) -> ShutdownReportSI {
        {
            let mut coord = self.shared.coord.lock().unwrap_or_else(|p| p.into_inner());
            match coord.pool_state {
                PoolState::Stopped | PoolState::Failed => {
                    let (depth, _) = self.shared.total_queue_depth_and_bytes();
                    return ShutdownReportSI {
                        fully_drained: !coord.coordinator_alive,
                        pool_state: coord.pool_state,
                        committer_report: None,
                        queue_depth_at_shutdown: depth,
                    };
                }
                PoolState::Running => coord.pool_state = PoolState::Draining,
                PoolState::Draining => {}
            }
        }
        self.shared.not_empty.notify_all();
        for nf in &self.shared.not_full {
            nf.notify_all();
        }

        let (queue_depth_at_shutdown, _) = self.shared.total_queue_depth_and_bytes();
        let deadline = Instant::now() + self.config.shutdown_drain_bound;
        let mut coord = self.shared.coord.lock().unwrap_or_else(|p| p.into_inner());
        while coord.coordinator_alive {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (new_coord, _) = self
                .shared
                .not_empty
                .wait_timeout(coord, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            coord = new_coord;
        }
        let coordinator_alive = coord.coordinator_alive;
        if !coordinator_alive && coord.pool_state == PoolState::Draining {
            coord.pool_state = PoolState::Stopped;
        }
        let pool_state = coord.pool_state;
        drop(coord);

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

        ShutdownReportSI {
            fully_drained: !coordinator_alive,
            pool_state,
            committer_report,
            queue_depth_at_shutdown,
        }
    }

    pub fn stats(&self) -> ShardedIngressStats {
        let (queue_depth_total, queued_bytes_total) = self.shared.total_queue_depth_and_bytes();
        let coord = self.shared.coord.lock().unwrap_or_else(|p| p.into_inner());
        let state = coord.pool_state;
        let coordinator_alive = coord.coordinator_alive;
        drop(coord);
        ShardedIngressStats {
            state,
            submitted: self.shared.submitted.load(Ordering::Relaxed),
            completed_ok: self.shared.completed_ok.load(Ordering::Relaxed),
            completed_err: self.shared.completed_err.load(Ordering::Relaxed),
            rejected_backpressure: self.shared.rejected_backpressure.load(Ordering::Relaxed),
            queue_depth_total,
            queued_bytes_total,
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
            .coord
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pool_state
    }

    pub fn into_inner(self) -> Result<GroupCommitter> {
        self.shutdown();
        let committer = Arc::clone(&self.committer);
        drop(self);
        Arc::try_unwrap(committer).map_err(|_| EngineError::WalUnavailable {
            detail: "ShardedIngressPool::into_inner: the underlying GroupCommitter still had \
                     an outstanding reference after shutdown() drained the coordinator"
                .to_string(),
        })
    }
}

impl Drop for ShardedIngressPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn drain_all_shards(shared: &PoolShared, max_per_shard: usize) -> Vec<QueueEntry> {
    let mut merged = Vec::new();
    for shard in &shared.shards {
        let mut guard = shard.lock().unwrap_or_else(|p| p.into_inner());
        let take = guard.entries.len().min(max_per_shard.max(1));
        let drained: Vec<QueueEntry> = guard.entries.drain(..take).collect();
        let drained_bytes: usize = drained.iter().map(|e| e.approx_bytes).sum();
        guard.queued_bytes = guard.queued_bytes.saturating_sub(drained_bytes);
        drop(guard);
        merged.extend(drained);
    }
    merged
}

fn coordinator_loop(
    shared: Arc<PoolShared>,
    committer: Arc<GroupCommitter>,
    await_retry_budget: Duration,
    max_drain_per_shard: usize,
) {
    let _alive_guard = CoordinatorAliveGuard {
        shared: Arc::clone(&shared),
    };
    loop {
        let batch = drain_all_shards(&shared, max_drain_per_shard);
        if !batch.is_empty() {
            for nf in &shared.not_full {
                nf.notify_all();
            }
            process_batch(batch, &committer, &shared, await_retry_budget);
            continue;
        }
        // Nothing on any shard right now — check shutdown, else wait to
        // be woken by the next submission to any shard.
        let mut coord = shared.coord.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(
            coord.pool_state,
            PoolState::Draining | PoolState::Stopped | PoolState::Failed
        ) {
            // Re-check every shard one more time under the coord lock's
            // own happens-before edge is unnecessary here: shards were
            // just observed empty above, and shutdown only ever
            // transitions forward, never re-admits work — safe to exit.
            return;
        }
        // Bounded, not a pure indefinite wait: `submit()` pushes onto a
        // *shard* lock, independent of `coord`'s own lock, then calls
        // `not_empty.notify_one()` — there is a real, if narrow, window
        // between this loop's `drain_all_shards` observing every shard
        // empty and this `wait` call actually parking, during which a
        // concurrent `submit()` could push and notify *before* anyone is
        // listening (a classic lost-wakeup: `Condvar::notify_one` does
        // not queue up for a future waiter). A single shared lock
        // spanning both the per-shard push and this coordinator's check
        // would close the race, but only by giving every submission
        // back the same global-lock contention sharding this module
        // exists to test away. A generous (50ms — not a tight busy-poll)
        // bounded fallback is the standard resolution: notified
        // immediately in the overwhelmingly common case, and never stuck
        // longer than this bound even in the unlucky race window — still
        // satisfies "no unbounded blocking" without reintroducing a
        // global per-submission lock.
        let (new_coord, _) = shared
            .not_empty
            .wait_timeout(coord, Duration::from_millis(50))
            .unwrap_or_else(|p| p.into_inner());
        coord = new_coord;
        drop(coord);
    }
}

fn respread_error(e: &EngineError) -> EngineError {
    EngineError::WalUnavailable {
        detail: format!("sharded-ingress batch outcome (shared across this batch): {e}"),
    }
}

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
            detail: "sharded-ingress batch: an earlier entry in this same batch failed to \
                     append; this entry was never attempted"
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
        let path = std::env::temp_dir().join(format!("rubixdb_sharded_ut_{tag}_{nanos}"));
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

    fn small_config() -> ShardedIngressConfig {
        ShardedIngressConfig {
            shard_count: 4,
            queue_capacity_per_shard: 4,
            max_queued_bytes_per_shard: 1024 * 1024,
            submission_timeout: Duration::from_millis(500),
            shutdown_drain_bound: Duration::from_secs(5),
            await_retry_budget: Duration::from_secs(2),
            max_drain_per_shard_per_cycle: 4096,
        }
    }

    #[test]
    fn single_submit_completes_durably_and_recovers() {
        let dir = temp_dir("single");
        let pool = ShardedIngressPool::new(test_committer(&dir), small_config()).unwrap();
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

    fn submit_retrying(pool: &ShardedIngressPool, key: &[u8], value: &[u8]) -> Completion {
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
        let pool = Arc::new(ShardedIngressPool::new(test_committer(&dir), small_config()).unwrap());
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

        // Note: unlike a single-queue architecture, per-shard round-robin
        // assignment means sequence order (assigned by GroupCommitter at
        // append time, once the coordinator drains a given shard) is not
        // simply "submission order" even in this single-coordinator
        // design — the coordinator drains shard 0..N each cycle, so
        // entries from a later-assigned shard within the same cycle can
        // still be appended (and thus sequenced) before an
        // earlier-submitted entry sitting in a shard visited later in
        // that same cycle. This is exactly this module's own "# Ordering"
        // section's point: queue arrival order was never a durability
        // guarantee in any Phase 2/2B architecture. What must still hold
        // — checked below — is a gap-free, duplicate-free recovered
        // prefix, not submission-order preservation.
        let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        assert!(replay.corrupted_segments.is_empty());
        assert_eq!(replay.records.len(), THREADS * PER_THREAD);
        for (i, (seq, _)) in replay.records.iter().enumerate() {
            assert_eq!(
                *seq,
                (i as u64) + 1,
                "sequences must be gap-free and ordered, even if not submission-ordered"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn queue_full_rejects_with_timeout_not_silently() {
        let dir = temp_dir("full");
        let config = ShardedIngressConfig {
            shard_count: 1,
            queue_capacity_per_shard: 2,
            submission_timeout: Duration::from_millis(50),
            ..small_config()
        };
        let pool = Arc::new(ShardedIngressPool::new(test_committer(&dir), config).unwrap());
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
        assert!(any_rejected, "a burst must eventually observe backpressure");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_is_idempotent() {
        let dir = temp_dir("idempotent");
        let pool = ShardedIngressPool::new(test_committer(&dir), small_config()).unwrap();
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
        let pool = ShardedIngressPool::new(committer, small_config()).unwrap();
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
    fn coordinator_panicking_fails_safely_and_rejects_further_work() {
        let dir = temp_dir("coordinator_panic");
        let committer = test_committer(&dir);
        committer.install_fsync_fault_hook(|| panic!("injected coordinator panic"));
        let pool = ShardedIngressPool::new(committer, small_config()).unwrap();
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
