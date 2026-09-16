# Changelog

All notable changes to RubixDB are recorded here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project has not cut a
release yet, so everything so far lives under `[Unreleased]`.

## [Unreleased]

### Phase 3C: final WAL/coordinator release certification (long soak in progress)

Targets Phase 3B's own six named blockers directly.

- **`GroupCommitter`/`BatchCoordinatorPool::purge_before`** (new):
  mirrors the existing `rotate()` wrapper, delegating to `FileWal::
  purge_before` under the `wal` lock. Safe to call concurrently with
  ongoing writes. Enables realistic checkpointing during a genuinely
  long soak.
- **`examples/crash_cycle_child.rs` + `crash_cycle_test.rs`** (new):
  periodic forced-crash-during-soak testing via a real external
  process kill (`Child::kill()`, randomized/seeded/reproducible delay)
  against a real child process running the production
  `BatchCoordinatorPool` — a genuinely new, asynchronous, uncooperative
  fault-injection class. 40/40 cycles recovered cleanly, zero
  corruption, monotonic gap-free sequences.
- **`examples/long_soak_test.rs`** (new): extends Phase 3B's
  `soak_test.rs` with periodic checkpointing and CPU sampling
  alongside RSS. A true 4-hour-per-writer-level run (100w then 1000w)
  launched against the production `BatchCoordinatorPool`.
- **`tests/pathological_recovery_matrix.rs`** (new): 9 consolidated
  fixture tests against the existing, unmodified recovery contract —
  9/9 pass, including two genuinely new corruption classes beyond
  Phase 0/1's own coverage.
- **`examples/recovery_memory_scaling.rs`** (new): quantifies
  `PHASE3B_ADR.md` ADR-P3B-5's finding with real swept data (1M-15M
  records) — RSS scales linearly at ~134 bytes/record, recovery
  throughput stays flat regardless of scale. `PHASE3C_ADR.md`
  ADR-P3C-1 analyzes (does not implement) a future streaming/callback/
  bounded-batch recovery API redesign.
- **`BatchCoordinatorStats::{bytes_total, writes_timed_out}`** (new):
  two more genuine, low-contention observability fields.
- Security/dependency review completed: zero `unsafe` code, zero
  payload logging in any Phase 3C addition; `Cargo.lock` fully
  reviewed (no new production dependency); `cargo-audit`/`cargo-deny`
  not installed (crates.io network access unavailable this session,
  decision documented).

130/130 lib tests pass, clippy and fmt clean. Full design:
`PHASE3C_ARCHITECTURE.md`; failure model: `PHASE3C_FAILURE_MODEL.md`;
decisions: `PHASE3C_ADR.md`; results and current status (authoritative
— the long soak, final benchmark comparison, and final certification
decision were still in progress at this entry's own commit time):
`PHASE3C_TEST_RESULTS.md`.

### Phase 3, Increment 3B: coordinator fault matrix + resource/rotation/shutdown hardening (soak run complete — PHASE 3B INCOMPLETE, blockers remain)

Completes the coordinator-level half of Phase 3's production-hardening
scope, distinct from Increment 3A's `GroupCommitter`-level leader-panic
fix.

- **`CoordinatorFaultPoint`** (new, `src/execution/batch_coordinator.rs`):
  7 deterministically injectable points in the Dedicated Batch
  Coordinator's own batch-processing loop (`BeforeBatchFormation`,
  `AfterDrain`, `AfterAppend`, `BeforeAwaitDurable`, `AfterDurable`,
  `BeforeCompletion`, `DuringShutdown`), plus `install_coordinator_
  fault_hook`/`clear_coordinator_fault_hook` (`test-util`-gated).
- **Fixed a real completion-safety gap**, found while wiring up the
  `AfterDrain` test: `process_batch` previously only protected a
  dequeued entry with a `CompletionGuard` once the append loop
  individually reached it — a coordinator panic between dequeue and
  that point would have dropped every entry in the batch with callers
  hanging forever. Every entry now gets its guard as `process_batch`'s
  first action.
- **`queued_bytes` accounting hardened to saturating arithmetic** across
  `batch_coordinator`/`leader_drain`/`sharded_ingress`/`write_pool` —
  consistency fix, not currently exploitable, matching this project's
  own `wal_test.md` §3.7 precedent.
- **New tests**: large-payload byte accounting (deterministic barrier),
  rapid submit/shutdown cycling, frequent rotation under sustained load
  through the full production path, shutdown racing active submission,
  and 7 coordinator-panic tests (one per fault point).
- **Observability**: `BatchCoordinatorStats::{queue_capacity,
  queued_bytes_capacity}`, `GroupCommitStats::{highest_sequence,
  segment_rotations}` (new fields, zero new contention). Full metric-
  list audit and explicit gap accounting: `PHASE3B_TEST_RESULTS.md` §7.
- **`examples/soak_test.rs`** (new): sustained-workload harness against
  the production `BatchCoordinatorPool` with low-contention per-thread
  latency sampling and periodic aggregation, RSS tracking, and a
  start/mid/end drift comparison. Run for 900s (15 min) at both 100 and
  1,000 writers — write path clean at both levels (zero errors/timeouts,
  flat RSS, no degradation trend).
- **A genuine finding, not a Phase 3B-introduced defect**: the
  1,000-writer soak's own post-run recovery-verification step (not the
  write path) was killed by a real host out-of-memory condition while
  `FileWal::open_for_recovery` materialized ~85M records into one `Vec`
  — this project's existing (Phase 0) recovery API has no streaming
  variant, and its memory demand scales with WAL size. Investigated,
  confirmed correct at reduced scale by a supplementary run, documented
  precisely (`PHASE3B_ADR.md` ADR-P3B-5) rather than hidden; the harness
  now warns before repeating it. Fixing the underlying API is out of
  this phase's scope.
- Final post-hardening benchmark: no measurable regression (100w 16,133
  ops/sec, 1000w 91,208 ops/sec — both within the pre-established
  historical noise band and comfortably above target).

128/128 lib tests pass (117 + 11 new), clippy and fmt clean, zero
regressions. Full design: `PHASE3B_ARCHITECTURE.md`; failure model:
`PHASE3B_FAILURE_MODEL.md`; decisions: `PHASE3B_ADR.md`; performance:
`PHASE3B_PERFORMANCE.md`; results and final verdict: `PHASE3B_TEST_
RESULTS.md` — **PHASE 3B INCOMPLETE — BLOCKERS REMAIN** (six explicit,
named gaps against the operating brief's full scope; see that
document's §11 for the complete list and recommendation).

### Phase 3, Increment 3A: leader-failure P0 fix

Fixes a real availability gap `PHASE2B_FAILURE_MODEL.md` §3 diagnosed
but did not fix: a leader thread panicking mid-batch left
`GroupCommitter`'s `leader_active` flag (`src/wal/group_commit.rs`)
stuck `true` forever, degrading every future caller (on any
architecture — Approach A/B/C, or a direct caller) to a repeated-timeout
failure mode instead of a clean, bounded error.

- **`LeaderFailureGuard`** (new, `src/wal/group_commit.rs`): an RAII
  guard, armed the instant a caller is elected leader, disarmed only
  once `run_as_leader` returns normally. If the leader thread instead
  panics, the guard's `Drop` clears `leader_active` and poisons the
  committer during the unwind itself — mirrors `execution::common::
  CompletionGuard`'s existing pattern, not a new abstraction.
- **`PoisonReason`** (new enum, replaces `BatchState::poisoned`'s
  previous bare `io::ErrorKind`): `FsyncFailed(io::ErrorKind)` (the
  original Phase 1 poisoning path, unchanged) or `LeaderPanicked` (new).
  The `Err` a poisoned `GroupCommitter` returns now says which.
- **`GroupCommitter::is_poisoned() -> bool`** (new, public): observability
  accessor: poisoning was already externally observable via `await_
  durable`'s `Err`; this makes it queryable without a live batch.
- No change to the WAL format, `durable_through`'s semantics, sequence
  allocation, rotation, or `FileWal`'s recovery contract. No new
  dependency, no `unsafe` code. Recovery from a poisoned `GroupCommitter`
  is unchanged from Phase 1's own documented model: discard it, reopen
  the WAL directory (`FileWal::open_for_recovery` re-scans from disk),
  construct a fresh one — verified end-to-end by a new test.
- Two pre-existing `execution::leader_drain` tests, whose doc comments
  and implicit timing assumptions described the old, now-fixed behavior
  (~5s shutdown cost; a second request only failing after its full
  retry budget), were updated in place with new `< 1s` timing
  assertions locking in the fix, rather than left stale next to
  passing-but-now-misleading documentation.

Full design and the leader-failure state machine: `PHASE3_FAILURE_
MODEL.md`; decision record: `PHASE3_ADR.md` ADR-P3-1; results: `PHASE3_
TEST_RESULTS.md`; benchmarks: `PHASE3_PERFORMANCE.md` (both the
100-writer and 1,000-writer Phase 2B throughput targets remain met
after this fix, using the same Approach B/Dedicated Batch Coordinator
architecture, unchanged).

### Phase 1: Group Commit

Adds `wal::group_commit::GroupCommitter`, a leader-follower group commit
layer over the existing `FileWal`: concurrent callers share one `fsync`
per batch instead of paying one per write, with a monotone
`durable_through` watermark, bounded backpressure, explicit shutdown, and
observability counters. No change to the WAL's on-disk format, `Wal`
trait signatures, or `FileWal`'s single-writer internal model — see
`PHASE1_ARCHITECTURE.md`/`PHASE1_GROUP_COMMIT.md`/`PHASE1_ADR.md` for the
design and `PHASE1_TEST_RESULTS.md` for full results, benchmark numbers,
and the production-readiness decision (**not production ready**: the
100-writer/1,000-writer throughput targets are not met on the
development machine's disk, even after a controlled window-size sweep
(`PHASE1_ADR.md` ADR-12) substantially closed the gap by fixing the
leader's batch-window formula (`WINDOW_EMA_DIVISOR` `10 → 1`, `max_wait`
`200µs → 5ms`, plus a demand-adaptive probe protecting single-writer
latency) — 100 writers improved from ~67% to ~79% of target, 1,000
writers from ~46% to ~81%; every other gate is met).

- **`GroupCommitter`** (`src/wal/group_commit.rs`): `append`/
  `await_durable`/`append_durable`/`rotate`/`durable_through`/`stats`/
  `shutdown`, plus `with_max_pending_waiters` for explicit backpressure
  configuration. `SyncMode::GroupCommit` is no longer rejected by
  `FileWal::open_for_recovery` (it previously returned `EngineError::
  Unsupported` — see below).
- **`FsyncLatencyTracker`** (`src/wal/metrics.rs`): an `AtomicU64`-only
  EMA `fsync`-latency tracker (`new = 0.1 * sample + 0.9 * old`), driving
  the leader's batch-window sizing and a follower's timeout.
- **`FileWal::durable_seq`**: a new field, advanced only inside `sync()`/
  `rotate()` after a genuinely successful `fsync` — distinct from
  `next_seq() - 1` ("assigned," not "durable"), closing a real footgun
  where an unsynced raw `append()` before constructing a `GroupCommitter`
  could otherwise be silently treated as durable.
- **`AbortPoint`** (`src/wal/mod.rs`) expanded from 4 to 11 variants: the
  7 new ones (`BeforeLeader`, `AfterLeaderElection`, `DuringBatchWaitPre`/
  `Post`, `AfterWatermarkBeforeWake`, `DuringRotationPre`/`Post`) name
  real, reachable boundaries in `GroupCommitter`'s leader/rotation paths.
  `BeforeSync`/`AfterSync` now additionally fire from `GroupCommitter`'s
  own leader `fsync` call, not only from `FileWal::sync()`, which that
  path never calls.
- **`EngineError::Timeout`** (`src/error.rs`): a new variant for a
  follower's bounded wait expiring — required by the algorithm, distinct
  from `Io` (no I/O necessarily failed) and safe to retry.
- Seven new integration test files under `tests/group_commit/` (one per
  milestone, plus a proptest), a write-only load-test harness (`examples/
  group_commit_load_test.rs`), and a permanent append-path diagnostic
  (`examples/append_only_benchmark.rs`).
- **Window-size sweep and batch-window formula fix** (`PHASE1_ADR.md`
  ADR-12, `PHASE1_TEST_RESULTS.md` §9A/§9B): a temporary, feature-gated
  experiment (`phase1-window-experiment` Cargo feature, off by default;
  `examples/window_sweep.rs`) established empirically that the original
  leader batch-window formula (`min(200µs, EMA/10)`) was substantially
  under-tuned, not solely limited by disk `fsync` latency as first
  believed. `WINDOW_EMA_DIVISOR` changed `10 → 1`; every test/harness
  `max_wait` changed `200µs → 5ms`; a demand-adaptive probe
  (`PROBE_WINDOW = 200µs`) added so a lone, uncontended writer never
  pays for batching benefit that will never materialize — a real
  regression the naive fix introduced and this probe resolves, verified
  by re-running M1.1.

### Five follow-up fixes from external review

- **`purge_before` now attempts its directory fsync on the error path
  too**, not only on success — the "resurrection is tolerated" argument
  (every purged segment's data is already durable elsewhere, WAL Spec
  §10) is a second line of defense, not a substitute for actually trying
  the fsync whenever the directory genuinely changed. A `remove_file`
  failure and a subsequent fsync failure are now folded into one error
  that names both, rather than either one being silently dropped.
  Dropped the unconditional `eprintln!` (a library writing to stderr
  unconditionally is untestable and rude to embedders) — the combined
  error message itself now carries what the log line used to.
- **Documented, not "fixed," why recovery's torn-tail truncation doesn't
  need a directory fsync**: `set_len` + `sync_all` flush exactly the
  file's own inode metadata (its size field); no directory *entry* is
  created, renamed, or unlinked, so a directory fsync there would be a
  no-op on every mainstream filesystem and pure cost on the recovery hot
  path.
- **`write_all_at` now has a real Windows implementation** (`seek_write`
  in a retry loop matching `std::io::Write::write_all`'s own
  `Interrupted`-retry rule) instead of falling back to the portable
  seek-then-write-all default. **In verifying this by actually running
  the test on Windows** (this project's dev machine), found a genuine,
  previously-undocumented platform difference: Windows' `seek_write` on
  an ordinary synchronous handle *does* leave the file's position at the
  end of the just-written region, unlike Unix's `pwrite`, which never
  touches it. The byte content lands correctly at the correct offset on
  both platforms either way (nothing in this crate's production code
  relies on the position side-effect), but the doc comment and test
  previously claimed a cross-platform guarantee that turned out to be
  Unix-only — corrected rather than asserted from memory. See
  `WalFile::write_all_at`'s doc comment.
- Expanded `Fault::PartialThenFail`'s doc comment with its exact
  interaction with `std::io::Write::write_all`'s retry behavior and how
  it differs from `ShortWrite`, at the definition site rather than
  requiring a future test author to read `FaultInjectingIo::write`'s
  body to find out.
- Added a `NOTE` comment directly above `scan_directory`'s main loop
  making explicit that the first-corruption-stops-the-scan behavior
  (Group 3.1) is deliberate, and that a future operator-diagnostics
  function walking past corruption would need to be a *different*
  function with a *different* contract, not a loosened version of this
  loop.

### Cross-process file locking

- **`open_for_recovery` now takes an OS-level exclusive advisory lock**
  on the WAL directory (`std::fs::File::try_lock` — `flock` on Unix,
  `LockFileEx` on Windows, both via the standard library, no new
  dependency and no `unsafe`) for as long as the returned `FileWal`
  lives, closing a real gap where two concurrent `open_for_recovery`
  calls on the same directory — two separate processes, or two
  unsynchronized calls within one — could each independently scan,
  truncate torn tails, and append, silently corrupting each other's view
  of the WAL. A second attempt while the lock is held fails immediately
  (`EngineError::WalUnavailable`) — it never blocks, and it is never
  silently allowed to proceed.
- **`inspect` takes a compatible shared lock**: any number of `inspect`
  calls may run concurrently with each other, but not while a writer
  holds the exclusive lock (closing a narrower race — `FileWal::append`
  writes directly with no atomic-rename step, so `inspect` could
  otherwise observe a segment file mid-write). Takes no lock at all
  against a directory no writer has ever opened — `inspect` must never
  create anything.
- Verified with both an in-process regression test and a genuine
  cross-process test (`tests/wal_tests.rs`'s
  `cross_process_lock_prevents_concurrent_writers`, using the same
  spawn-this-test-binary-as-a-child-process technique as
  `tests/crash_consistency.rs`) — a real second OS process is rejected
  while the first is open and succeeds once it's dropped.

### WAL hardening pass (production-readiness review)

A targeted review of the WAL implementation (`src/wal/`) against
production-readiness criteria, fixing 20 issues across crash-safety,
format validation, recovery semantics, thread-safety documentation, and
test coverage, plus the cross-process locking gap above. The on-disk
format (WAL Spec §2) is unchanged — verified byte-for-byte against
`encode_segment_header(42)` and a sample `PUT` frame before and after
this pass.

#### Crash-safety / durability

- **`SegmentIo::append` now rolls back a failed write.** A partial write
  (some bytes physically land, then the write call fails) used to leave
  garbage bytes on disk past the tracked segment length, silently
  corrupting the *next* append's target region. `append` now truncates
  the file back to its pre-append length and fsyncs that truncation on
  any write failure. If the rollback itself fails, the `SegmentIo` is
  marked poisoned (`SegmentIo::is_poisoned`, `FileWal::is_poisoned`) and
  refuses all further I/O rather than write on top of an unknown-length
  file.
- **Directory fsync after segment create/remove.** `create_new_segment_file`
  and `FileWal::purge_before` now fsync the containing directory (Unix;
  documented no-op on Windows — see `file_io::fsync_dir`'s doc comment)
  so a file's *creation or removal*, not just its contents, survives a
  crash.
- **`create_new_segment_file` is now atomic w.r.t. partial failure.** A
  failure at any step after `create_new(true)` — header write, header
  fsync, or the new directory fsync — removes the partially-initialized
  file (best-effort) before returning, so a segment never exists on disk
  without a valid header.
- **`FileWal::rotate` is now atomic.** The new segment file is created
  *before* anything about the current `FileWal` state is touched; if
  sealing the old segment (`sync`) then fails, the just-created file is
  deleted and every field is left exactly as it was.
- **`FileWal::purge_before` removes segments one at a time**, updating its
  internal bookkeeping only after each individual removal succeeds, so a
  mid-list failure leaves accurate state rather than a mismatch between
  disk and memory. The directory is fsynced once after the whole batch.

#### Format validation

- `decode_segment_header` now rejects an unrecognized `format_version` and
  non-zero reserved `flags`, instead of accepting and silently
  misinterpreting a foreign/future format.
- `read_u32_le`/`read_u64_le` now return `Result` instead of relying on a
  `debug_assert!`-only precondition that disappears in release builds.
- WAL-frame encoding is now fully fallible end-to-end
  (`format::encode_frame`'s op-body closure, `write_len_prefixed`): an
  oversized field is rejected at the exact point of violation, not by
  emitting a sentinel value for a separate, later check to catch.
- `decode_wal_body`'s existing trailing-byte/wrong-length strictness for
  `PUT`/`DELETE`/`CHECKPOINT_MARKER` now has explicit regression tests.

#### Recovery semantics (behavior change — see below)

- **Recovery now stops at the first corrupted segment** and trusts
  nothing at or after it, including that segment's own records that
  preceded the corruption point within it. **This amends WAL Spec
  §6.2's original text**, which allowed scanning to continue past a
  corrupted non-last segment. See `wal::mod`'s "# Durability" section and
  `scan_directory`'s doc comment for the full rationale (fail-closed:
  once one segment's integrity is in question, a partial picture
  assembled from what comes after it is not more trustworthy for looking
  more complete). One pre-existing test asserted the old behavior by
  name and by assertion; it has been updated (not deleted) to assert the
  new contract, and a complementary test was added covering the
  "corruption is not in the first segment" case.
- `walk_segment`'s frame-extent overflow case now returns a `Corruption`
  error instead of `.expect()`-panicking on a value that is only
  provably non-overflowing for realistic input.
- Segment-ID arithmetic (`next_segment_id`, used by both `rotate` and
  fresh-segment creation) is now checked, returning `CapacityExceeded`
  instead of wrapping on overflow.

#### `inspect()` is now genuinely read-only

- `canonicalize_existing_dir` (used only by `inspect`) never creates the
  WAL directory — `canonicalize_data_dir` (used by `open_for_recovery`)
  still does.
- `scan_segment` takes a `mutate` flag; when false, every segment file is
  opened read-only, so `inspect` can run against a directory the caller
  can't write to.

#### Concurrency & API surface

- `FileWal` is documented as `Send` but deliberately not `Sync`
  (single-writer type), enforced at compile time via a
  `static_assertions::assert_not_impl_any!` check.
- `SyncMode::GroupCommit` is now rejected by `open_for_recovery`
  (`EngineError::Unsupported`) rather than silently running in
  `Immediate` mode — a caller that asks for batching is told there is
  none yet, instead of quietly getting different behavior than it
  configured.
- `wal::testing` (the `FaultInjectingIo` harness) is now gated behind
  `#[cfg(any(test, feature = "test-util"))]` instead of being
  unconditionally `pub`.

#### Performance

- `SegmentIo::append` uses a new `WalFile::write_all_at` method — a
  single `pwrite`-based syscall on Unix (via `std::fs::File`'s override,
  `std::os::unix::fs::FileExt`), falling back to the portable
  seek-then-write-all on other platforms. The doc comment claiming "one
  syscall per append" was accurate on neither platform before this
  change (it always did a separate `seek` first); it now says exactly
  what happens on each platform.
- Added `benches/append.rs` (behind the new `bench` Cargo feature),
  isolating pure `append` latency from `append_sync`'s `fsync` cost.

#### Tests & fuzz coverage

- Three new proptest cases in `wal::fuzz_tests` (≥1,000 runs each):
  random single-byte corruption anywhere outside the header, a
  partial-write-with-garbage-header-bytes scenario, and a fixed-seed,
  10,000-iteration arbitrary-byte-string panic check.
- `Fault::PartialThenFail` added to the `FaultInjectingIo` harness (bytes
  physically land, then the call fails), with regression tests for both
  the successful-rollback and poison-on-rollback-failure paths.
- New `tests/crash_consistency.rs` (behind the `test-util` feature):
  spawns this same test binary as a child process, which opens a real
  `FileWal`, appends records, and calls `std::process::abort()` at one of
  four configurable points (`FileWal::set_abort_hook`); the parent
  reopens and asserts a corruption-free, gap-free recovered prefix. See
  that file's doc comment for what this does and does not prove
  (`process::abort()` is not a power-loss simulation).
- A read-only-directory test (`#[cfg(unix)]`, `chmod 0o555`) asserting
  `inspect` succeeds where `open_for_recovery` fails with
  `PermissionDenied`.

#### Documentation

- Checked the entire `src/`/`tests/`/`benches/` tree for mojibake — found
  none (all files are valid UTF-8, `§`/`—`/`'` are correctly encoded
  throughout already). Added `scripts/check-encoding.sh`, a corrected
  version of the originally-specified check (the literal
  `grep -rP '[\x80-\xff]'` pattern matches *all* non-ASCII UTF-8 bytes,
  which would flag this codebase's own correct typography as an error).
- Added "# Safety" and "# Durability" sections to `wal::mod`'s
  module-level doc comment.

#### Fixed

- `create_new_segment_file` (rotation and initial-segment creation) now
  writes and fsyncs a new segment's header to a temporary file name and
  `fs::rename`s it into place, instead of writing the header directly at
  the segment's final name. A crash between file creation and header
  write previously left a zero-byte file at a real segment name, which
  recovery correctly (but undesirably) reported as a corrupted segment.
  Found by a deterministic `crash_consistency_across_abort_points`
  failure; see `PHASE1_TEST_RESULTS.md` §9F.2 and `PHASE1_ADR.md` ADR-15.
- Reverted the `filling_active`/`fsyncing_active` batch-pipelining split
  back to the single-phase `leader_active` design: measured to regress
  throughput on this project's development environment (Windows/NTFS)
  rather than improve it, with the change left uncommitted in the tree
  as pipelining's own regression check rather than reverted. See
  `PHASE1_TEST_RESULTS.md` §9E/§9F.1 and `PHASE1_ADR.md` ADR-14.

### Phase 2: Write Worker Pool (implemented, measured, rejected)

Adds `execution::WriteWorkerPool` (`src/execution/write_pool.rs`): a
bounded queue plus a configurable number of worker threads in front of
`GroupCommitter`, meant to separate logical client concurrency from
physical storage execution concurrency. `std`-only (`Mutex`+`Condvar`,
no new dependency), no change to `GroupCommitter`'s durability logic, no
WAL format change. Public API: `submit`/`Completion::wait`/
`wait_timeout`, `shutdown` (three-step: reject new work, drain the
queue, then finalize the underlying `GroupCommitter`), `stats`,
`into_inner`. Bounded everywhere: `queue_capacity`, `max_queued_bytes`,
`submission_timeout` (blocks then `EngineError::Timeout`, never drops a
write or blocks unboundedly), `shutdown_drain_bound`. A worker panic
resolves only its own in-flight request with an error (via an RAII
completion guard) and never loses another queued request; if every
worker terminates unexpectedly, the pool fails cleanly and drains the
remaining queue with an explicit error rather than leaving any caller
blocked forever.

**Measured and rejected as a production default**: a worker-count sweep
(1/2/4/8/16/32/64, plus a parity point at `worker_count = writer_count`)
at 100 and 1,000 logical writers found `GroupCommitter`'s batch size
architecturally capped at the worker count, not the logical writer
count — throughput regressed by one to two orders of magnitude at every
worker count meaningfully smaller than the writer count, and even at
parity (`worker_count = writer_count`) 1,000-writer throughput was 28%
below Phase 1's existing direct-thread architecture. Kept in the tree as
a documented, tested, but not-recommended artifact — see `PHASE2_TEST_
RESULTS.md`/`PHASE2_ADR.md` (ADR-P2-5) for the full evidence and
decision.

#### Fixed

- The worker pool's request-processing path originally retried nothing:
  a single-attempt `append` + `await_durable` call could surface a
  spurious `Timeout` to a caller under real concurrent load even though
  the underlying write was never lost. Fixed by retrying only `await_
  durable` (never `append` — appending exactly once means retrying the
  wait can never duplicate a record), mirroring the retry pattern Phase
  1's own test harness already established as correct
  (`tests/group_commit/support.rs::await_durable_retrying_on_timeout`).
  See `PHASE2_TEST_RESULTS.md` §13 and `PHASE2_ADR.md` ADR-P2-4.

### Phase 2B: three architectures evaluated — target achieved

Adds three further execution-layer architectures, evaluated against
Phase 2's rejected `WriteWorkerPool` and against each other:

- **`execution::leader_drain`** (Approach A, "Leader Queue Drain"): a
  worker drains the entire currently-queued backlog at once (not one
  request per loop iteration, unlike the rejected worker pool), appends
  every entry, then issues one `await_durable` for the whole batch.
  `worker_count=1` reached 16,806/95,686 ops/sec (100w/1,000w medians),
  exceeding both Phase 1 targets on the first attempt. A single-active-
  drain-leader coordination flag (`draining_active`/`DrainLeaderGuard`)
  lets `worker_count>1` provide hot-standby redundancy without
  fragmenting batches (the naive multi-worker failure mode this fixes),
  at a small cost to 100-writer margin.
- **`execution::batch_coordinator`** (Approach B, "Dedicated Batch
  Coordinator", **adopted as the recommended default**): exactly one
  coordinator thread, no worker-election machinery — structurally
  simpler than A. Reached 17,512/93,594 ops/sec (100w/1,000w medians,
  5 independent repetitions each) — the best 100-writer result of any
  architecture measured, with the least code.
- **`execution::sharded_ingress`** (Approach C, "Sharded/Per-Core
  Ingress"): `shard_count` independent ingress queues merged by one
  coordinator, evaluated once (the operating brief's own conditional
  framing — evaluate only if A and B fail, which they did not).
  15,234/96,033 ops/sec — no material improvement over B, confirming
  the single shared queue was never the bottleneck.

All three preserve the WAL format, `GroupCommitter`'s durability
contract, and crash-consistency guarantees unchanged. Zero Phase 1/
Phase 2 regressions across the full cycle. Full account: `PHASE2B_
FINAL_TEST_RESULTS.md`; design: `PHASE2B_ARCHITECTURE_A/B/C.md`;
decisions: `PHASE2B_ADR.md`.

**Target achieved**: Approach B reached a median 17,512 durable
ops/sec at 100 writers (target ≥15,000) and 93,594 at 1,000 writers
(target ≥80,000) — the first phase in this project's history to meet
the original Phase 1 throughput targets.

#### Fixed

- Every Phase 2B architecture's batch-processing path originally (in
  Approach A's first implementation) constructed each request's panic-
  safety guard (`CompletionGuard`) *after* the one shared `await_
  durable` call for a batch, rather than before — leaving every entry
  in a batch unprotected during the call most likely to observe a fault.
  A panic there hung the corresponding fault-injection test past a
  60-second timeout. Fixed by constructing every guard before the
  shared call and keeping them alive across it; Approaches B and C were
  written after this fix and used the correct ordering from the start.
  See `PHASE2B_FAILURE_MODEL.md` §2 and `PHASE2B_ADR.md` ADR-P2B-3.

#### Discovered (pre-existing Phase 1 behavior, not a regression)

- A leader/coordinator thread that panics specifically while inside the
  leader `fsync` call leaves `GroupCommitter`'s own `leader_active` flag
  (`src/wal/group_commit.rs`) permanently stuck — every architecture's
  worker/standby redundancy is powerless against this specific failure,
  since the underlying committer itself becomes globally wedged, not
  just the one thread that died. Verified the system still fails safely
  (bounded, no hang, no false acknowledgment) under this condition.
  See `PHASE2B_FAILURE_MODEL.md` §3.
