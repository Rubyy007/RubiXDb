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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::error::{EngineError, Result};
use crate::wal::metrics::FsyncLatencyTracker;
use crate::wal::{FileWal, SyncMode, Wal, WalOp, WalPosition};

/// Coordination state guarded by `GroupCommitter::batch`. Deliberately
/// minimal (`PROCESS.md` §1.10): no per-waiter registry is needed, since
/// every waiter independently re-derives its own outcome from
/// `durable_through`/`poisoned` after every wake.
#[derive(Debug, Default)]
struct BatchState {
    /// `true` while some thread is between "elected leader" and "finished
    /// this batch" (success or failure). Only ever set by a thread that
    /// just transitioned `false -> true` under this lock; only ever
    /// cleared by that same thread once its batch concludes.
    leader_active: bool,
    /// Set once, permanently, the first time a leader's `fsync` fails.
    /// Never cleared — per the algorithm, a poisoned `GroupCommitter` stays
    /// poisoned until it is dropped and a fresh one is constructed. Stores
    /// only `io::ErrorKind` (not the original `io::Error`, which is not
    /// `Clone`) so every subsequent caller can synthesize an equivalent,
    /// same-class error (`GroupCommitter::poisoned_error`).
    poisoned: Option<io::ErrorKind>,
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
    pub fn new(wal: FileWal) -> Result<Self> {
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
        let committer = GroupCommitter {
            wal: Mutex::new(wal),
            durable_through: AtomicU64::new(initial_durable_through),
            batch: Mutex::new(BatchState::default()),
            condvar: Condvar::new(),
            latency: FsyncLatencyTracker::new(),
            max_wait_cap: max_wait,
            max_batch_bytes,
            batch_bytes: AtomicUsize::new(0),
            #[cfg(any(test, feature = "test-util"))]
            fsync_fault_hook: Mutex::new(None),
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

        let mut guard = self.lock_batch();
        loop {
            if self.durable_through.load(Ordering::Acquire) >= seq {
                return Ok(());
            }
            if let Some(kind) = guard.poisoned {
                return Err(Self::poisoned_error(kind));
            }

            if !guard.leader_active {
                guard.leader_active = true;
                self.batch_bytes.store(0, Ordering::Relaxed);
                drop(guard);
                return self.run_as_leader();
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
                if let Some(kind) = guard.poisoned {
                    return Err(Self::poisoned_error(kind));
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

    /// Consumes this `GroupCommitter` and returns the wrapped `FileWal`.
    /// Never panics: a poisoned `std::sync::Mutex` (only possible if a
    /// prior critical section panicked, which this module's own code never
    /// does) is recovered rather than propagated, matching `lock_wal`'s
    /// policy — see its doc comment.
    pub fn into_inner(self) -> FileWal {
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
        self.spin_wait_for_batch_window();

        let (cloned_file, batch_max_seq) = match self.snapshot_sync_target() {
            Ok(t) => t,
            Err(e) => {
                self.finish_batch_with_error(Self::io_kind_of(&e));
                return Err(e);
            }
        };

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
        let started = Instant::now();
        let fsync_result = self.do_leader_fsync(&cloned_file);
        let elapsed_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);

        match fsync_result {
            Ok(()) => {
                super::fire_abort_hook(super::AbortPoint::AfterSync);
                self.latency.record(elapsed_ns);
                self.durable_through
                    .fetch_max(batch_max_seq, Ordering::Release);
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

    /// The leader's wait window: `min(max_wait_cap, EMA / 10)`, but
    /// returns early the moment `batch_bytes` reaches `max_batch_bytes`
    /// (WAL Spec's group-commit extension point's own "whichever comes
    /// first" rule). Implemented as a tight poll/`spin_loop` rather than
    /// `thread::sleep` — see `PROCESS.md` §1.6: at this sub-millisecond
    /// scale, `thread::sleep`'s OS timer-resolution overshoot (particularly
    /// on Windows) would cost more than the window itself is worth
    /// amortizing `fsync` latency against. Only one thread is ever the
    /// leader at a time (enforced by `batch.leader_active`), so this spin
    /// never contends with itself.
    fn spin_wait_for_batch_window(&self) {
        let ema_ns = self.latency.current_ns();
        let ema_based = Duration::from_nanos(ema_ns / 10);
        let window = self.max_wait_cap.min(ema_based);
        if window.is_zero() {
            return;
        }
        let deadline = Instant::now() + window;
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
        const YIELD_EVERY: u32 = 10_000;
        let mut iterations: u32 = 0;
        loop {
            if self.batch_bytes.load(Ordering::Relaxed) >= self.max_batch_bytes {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            iterations += 1;
            if iterations.is_multiple_of(YIELD_EVERY) {
                std::thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
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
            guard.poisoned = Some(kind);
        }
        self.condvar.notify_all();
    }

    fn poisoned_error(kind: io::ErrorKind) -> EngineError {
        EngineError::Io(io::Error::new(
            kind,
            "group-commit leader's fsync failed; this GroupCommitter is \
             poisoned and must be discarded (drop it and open a fresh one) \
             — see PROCESS.md §1",
        ))
    }

    fn io_kind_of(err: &EngineError) -> io::ErrorKind {
        match err {
            EngineError::Io(e) => e.kind(),
            _ => io::ErrorKind::Other,
        }
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

/// An exact accounting of one `WalOp`'s encoded on-disk frame length,
/// mirroring `format`/`ops`'s layout (`length:u32, crc32c:u32, seq:u64,
/// op:u8, op_body`) without depending on their private bounds-checking —
/// used only to decide when a batch has accumulated "enough" bytes to stop
/// waiting early (WAL Spec's group-commit "or until batch payload reaches
/// 256 KB" rule). Not required to be exact for correctness (durability is
/// always derived from `FileWal::next_seq()`, never from this counter) —
/// it happens to be exact here because the layout is simple and stable,
/// not because exactness is load-bearing.
fn estimate_frame_len(op: &WalOp<'_>) -> usize {
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
    use crate::wal::WalConfig;
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
                max_wait: Duration::from_micros(200),
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
