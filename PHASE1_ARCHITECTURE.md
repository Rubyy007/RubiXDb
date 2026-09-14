# RubixDB Phase 1 — Architecture

Design only. No test results, benchmark numbers, or pass/fail status live
in this file — see `PHASE1_TEST_RESULTS.md` for all of that. This
document describes intended behavior in the present tense.

For the byte-level design log (written before implementation, with the
full reasoning behind every choice below) see `PROCESS.md` §1. This file
is the condensed, results-free architecture summary; `PROCESS.md` is the
detailed record.

## 1. Scope

Phase 1 adds `wal::group_commit::GroupCommitter`, a wrapper around the
existing, frozen `FileWal` that lets multiple concurrent callers share a
single `fsync` per batch instead of paying one `fsync` per write. Nothing
about `FileWal`'s on-disk format, its `Wal` trait signatures, or its
single-writer internal model changes. `GroupCommitter` is additive: a
`FileWal` used directly, without a `GroupCommitter`, behaves exactly as
before.

## 2. The core tension and the chosen shape

`FileWal::append` and `FileWal::sync` both require `&mut self`. Any
concurrent use needs mutual exclusion. Two shapes were considered:

- **Shape A** — wrap the whole `FileWal` in one `Mutex`, route both
  `append()` and `sync()` through it. Simple, but the mutex is held for
  the full duration of the `fsync` syscall, so every follower's `append()`
  is blocked behind whichever batch's `fsync` happens to be in flight —
  reintroducing serialized-per-write behavior with fewer `fsync` calls,
  not the throughput characteristics group commit is meant to provide.
- **Shape B (chosen)** — a `Mutex<FileWal>` guards only memory-speed
  operations (`append`, `rotate`, and a `(cloned file handle, batch high-
  water seq)` snapshot). The leader's `fsync` runs on a
  `std::fs::File::try_clone`'d handle, entirely outside that lock. `fsync`
  is a property of the underlying file, not of the handle used to invoke
  it, so a clone's `sync_all()` durably covers every byte written through
  the original handle up to that moment — this is standard OS behavior
  (POSIX `fsync`, Windows `FlushFileBuffers`), not something this crate
  invents. No `unsafe`, no new dependency: `File::try_clone` is safe,
  dependency-free `std` API.

Lock acquisition sequence on the hot path (single caller, no contention):

1. `append()`: lock `wal` → `FileWal::append(op)` → unlock. Memory-speed;
   no I/O wait.
2. `await_durable(seq)`: lock-free `AtomicU64` load of `durable_through`;
   if not yet satisfied, lock `batch` (a tiny `Mutex<BatchState>` guarding
   only `leader_active`/`poisoned`) to decide leader-or-follower.
3. **Leader**: unlock `batch` → spin-wait the batch window → lock `wal` →
   snapshot `(cloned file, batch_max_seq)` → unlock `wal` →
   `cloned_file.sync_all()` (no lock held) → on success: publish
   `durable_through`, lock `batch` only to clear `leader_active`, unlock,
   `condvar.notify_all()`.
4. **Follower**: `condvar.wait_timeout` on `batch`, re-checking
   `durable_through`/`poisoned`/shutdown on every wake.

No thread ever holds `wal` and `batch` simultaneously, and no thread holds
either lock across the `fsync` syscall.

## 3. Batching policy

`SyncMode::GroupCommit { max_wait, max_batch_bytes }` (a pre-existing
`WalConfig` field, not introduced by Phase 1) supplies the leader's
decision parameters. Defaults used throughout this phase's own tests and
harness: `max_wait = 200 µs`, `max_batch_bytes = 256 KiB` — configuration
values, not hardcoded constants; a caller may construct a different
`WalConfig` and get different batching behavior from the same code.

The leader waits `min(max_wait, EMA_fsync_latency / 10)`, or until
`max_batch_bytes` of appended payload has accumulated, whichever comes
first. The wait is a tight, periodically-yielding poll
(`std::hint::spin_loop()` with a `std::thread::yield_now()` every 10,000
iterations), not `std::thread::sleep` — at this sub-millisecond scale, OS
timer-resolution overshoot (particularly on Windows) would cost more than
the window itself is worth amortizing `fsync` latency against.

`FsyncLatencyTracker` (`wal::metrics`) maintains the EMA feeding that
formula: `new = 0.1 * sample + 0.9 * old`, in a single `AtomicU64`,
updated via a `compare_exchange` retry loop so it is safe to call
concurrently on its own terms. `GroupCommitter::new` performs one real,
synchronous `fsync` against the active segment before returning, seeding
this tracker with a genuine measurement rather than leaving it at `0` —
construction therefore costs roughly one `fsync`'s worth of latency,
paid once, not per call.

## 4. Rotation

`GroupCommitter::rotate()` delegates to `FileWal::rotate()` under the same
`wal` lock `append()` and the leader's snapshot use. No group-commit-
specific rotation logic exists, by design: `seq` is WAL-wide, not
segment-scoped, and a leader's `(cloned file, batch_max_seq)` snapshot is
atomic with respect to `rotate()` because both require the same lock. A
leader's in-flight clone always refers to a file `rotate()` can seal
around but never invalidates — `FileWal::rotate()` already `fsync`s the
segment being sealed before creating the next one, exactly as it did
before Phase 1.

## 5. Backpressure and shutdown

`GroupCommitter` bounds its own resource usage: `pending_waiters`
(callers currently inside `await_durable`, leader or follower) is capped
at `max_pending_waiters` (`GroupCommitter::with_max_pending_waiters`;
`new()` uses `DEFAULT_MAX_PENDING_WAITERS`). A caller arriving once the
cap is reached fails immediately with `EngineError::CapacityExceeded`
rather than blocking for a slot or queuing unboundedly — there is no
internal queue at all (see §6 of `PROCESS.md` for why none is needed:
every waiter re-derives its own outcome from `durable_through`, so no
per-waiter registry exists to bound in the first place; the waiter *count*
is the only thing that can grow with concurrency, and it is what this cap
bounds).

`GroupCommitter::shutdown()` sets a flag checked by every entry to
`append()` and every loop iteration of `await_durable()`; new batches
never start after it is set. A caller already blocked is woken via
`notify_all()` and observes the flag on its next iteration rather than
waiting out its full timeout. `shutdown()` gives an already-in-flight
leader a bounded chance to finish (its `fsync` cannot be interrupted
mid-syscall regardless) before returning a `ShutdownReport` stating
`durable_through` and the highest assigned `seq`, so a caller can tell
whether anything remains un-synced rather than that state being silently
dropped.

## 6. Observability

`GroupCommitter::stats()` returns a `GroupCommitStats` snapshot: `sync_
attempts`, `sync_successes` (and the derived `sync_failures`), `records_
total`/`max_batch_records` (and the derived `avg_batch_records`),
`window_wait_ns_total`/`window_wait_samples` (and the derived `avg_
window_wait_ns`), `durable_through`, `pending_waiters`. All backed by
`Relaxed` atomics updated on the leader's own path; none of them
participate in any correctness decision.

## 7. Crash-consistency instrumentation

`AbortPoint` (`wal::mod`) — a test-only hook mechanism predating Phase 1
— gains seven new variants naming real, reachable boundaries in
`GroupCommitter`'s leader and rotation paths: `BeforeLeader`, `After
LeaderElection`, `DuringBatchWaitPre`, `DuringBatchWaitPost`, `After
WatermarkBeforeWake` (all in `wal::group_commit`), and `DuringRotationPre`/
`DuringRotationPost` (in `FileWal::rotate`, reached identically whether
rotation is triggered by `GroupCommitter::rotate()` or directly). The four
pre-Phase-1 points (`AfterHeader`, `MidAppend`, `BeforeSync`, `AfterSync`)
are unchanged; `BeforeSync`/`AfterSync` now additionally fire from
`GroupCommitter`'s own leader `fsync` call (not only from `FileWal::
sync()`, which that path never calls), since both describe the same
conceptual moment relocated by Shape B.

## 8. What Phase 1 does not touch

No change to the WAL's on-disk format, `Wal` trait signatures, `FileWal`'s
`&mut self` single-writer model, `MANIFEST_FORMAT`/`SSTABLE_FORMAT`
(neither exists yet in this repository), or any read path (none exists —
Memtable/SSTable/the LSM facade are unimplemented; see `PROGRESS.md`).
`io_uring`, `O_DIRECT`, thread-per-core, and specialized allocators are
out of scope per this phase's own decision gate — no profiling evidence
names any of them as the dominant cost (see `PHASE1_TEST_RESULTS.md`'s
profiling section).
