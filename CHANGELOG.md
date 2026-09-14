# Changelog

All notable changes to RubixDB are recorded here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project has not cut a
release yet, so everything so far lives under `[Unreleased]`.

## [Unreleased]

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
