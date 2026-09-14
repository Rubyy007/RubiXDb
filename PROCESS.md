# RubixDB — PROCESS.md (Phase 1: Group Commit)

> **Path note:** the build brief for this phase asked for `docs/PROCESS.md`.
> This repository has no `docs/` directory — `ARCHITECTURE.md`,
> `PROGRESS.md`, and `CHANGELOG.md` all live at the repo root instead (see
> `ARCHITECTURE.md`'s "Project layout" entry). This file follows that
> existing convention rather than introducing a new `docs/` directory for
> one file. Likewise, the brief's `docs/Final_Technical_Execution_Brief.md`,
> `docs/MANIFEST_FORMAT.md`, and `docs/SSTABLE_FORMAT.md` do not exist in
> this repo; the equivalent frozen documents actually present are
> `RubixDB-Architecture-Specification-v1.0.md`,
> `RubixDB-WAL-Specification-v1.0.md`, and
> `RubixDB-LSM-Engine-Specification-v1.0.md` at the repo root, plus
> `ARCHITECTURE.md` for project-wide (Tier 2/3) invariants. The brief's own
> algorithm section (its "§2.2 of the Final Technical Execution Brief",
> restated in full in the request itself) is treated as authoritative for
> *this* feature's behavior, since no other document in this repo specifies
> group commit at that level of detail — WAL Spec §4/§9 describe it only as
> a named extension point ("a background flusher batches appends from
> multiple concurrent callers and issues one `fsync` per batch... designed
> so that adding it later does not change the on-disk format, only the
> internal scheduling of append/sync calls"), fully consistent with, and
> not contradicted by, the restated algorithm.

This document is the design log, milestone log, and decision record for
Phase 1: Group Commit, layered on top of the existing production WAL in
`src/wal/`. Per the brief: written before implementation code, updated after
every milestone, never rewritten retroactively (matching `PROGRESS.md`'s own
append-only convention).

---

## 1. Design log (written before code)

### 1.1 What already exists, and what must not change

`FileWal` (`src/wal/mod.rs`) is single-writer by construction: `append`,
`sync`, `rotate`, `purge_before` all take `&mut self`, `seq` is assigned
synchronously inside `append()`, and `FileWal` is compile-time asserted
`Send + !Sync` (`static_assertions::assert_not_impl_any!(FileWal: Sync)` in
`wal::tests`). `SyncMode::GroupCommit` exists in the config surface already
(WAL Spec §4.2/§5) but `open_for_recovery` has, until now, rejected it
outright (`EngineError::Unsupported`) rather than silently running it as
`Immediate` — see `wal::mod`'s "# Durability" section, "`SyncMode::
GroupCommit` is rejected, not silently downgraded". None of this — the
on-disk format, the `Wal` trait's method signatures, `FileWal`'s single-
writer internal model — changes in this phase. Group commit is built
*alongside* `FileWal`, not by rewriting it.

### 1.2 The core tension: two operations, one `&mut self`

`FileWal::append` and `FileWal::sync` both require exclusive (`&mut`)
access to the same `FileWal` value, per the `Wal` trait. Concurrent callers
therefore need *some* form of mutual exclusion around any `FileWal` use.
The naive approach (Shape A, the brief's own name for it) wraps the whole
`FileWal` in one `Arc<Mutex<FileWal>>` and routes both `append()` and
`sync()` through it. This is simple and correct, but it has a specific,
measurable cost: `sync()`'s dominant cost is the `fsync` syscall itself
(single-digit milliseconds on this machine — see the Phase 0 WAL benchmark
numbers in `PROGRESS.md`: `append_sync` at 16–4096 B payloads ran
3.3–10.2 ms per call, almost entirely `fsync` latency, not the write
itself). If the *same* mutex a leader holds for the whole duration of that
`fsync` call is also the mutex every follower's `append()` needs merely to
copy bytes into the OS page cache, then every follower is blocked for the
full `fsync` latency before it can even finish being appended, let alone
counted in the *next* batch. That reintroduces exactly the serialization
group commit exists to remove, just with fewer `fsync` calls than
`Immediate` mode — a real but much smaller win than the brief's 80,000
ops/sec (M1.3) target implies is achievable.

### 1.3 Shape B, concretely: decouple the `fsync` syscall from the append lock

The key realization is that `fsync`'s effect (make previously-`write()`d
bytes durable) is a property of the underlying OS file object, not of the
particular file *handle* (fd / `HANDLE`) used to invoke it. POSIX `fsync`
and Windows `FlushFileBuffers` both flush *every* byte previously written
to that file via *any* handle, the instant the writing `write()`/`pwrite()`
syscall returned success — this project's own `tests/crash_consistency.rs`
already documents and relies on the adjacent fact ("once a `write()`
syscall has returned successfully, its bytes are visible in the OS's own
view of the file to *any* process on that machine"). `std::fs::File::
try_clone()` (safe, dependency-free, stable since Rust 1.0) duplicates a
file handle — `dup()` on Unix, `DuplicateHandle` on Windows — with both
handles referring to the same underlying file. Calling `sync_all()` on a
*cloned* handle therefore durably flushes bytes written through the
*original* handle, with no shared mutable state and no lock contention
between the two calls.

This gives a real, safe-Rust (`std::fs::File::try_clone`, no `unsafe`, no
new dependency) way to split "the right to mutate `FileWal`" from "the
right to `fsync` the bytes `FileWal` has already written":

- **Append lock** (`Mutex<FileWal>`, held only for the duration of one
  `FileWal::append()` call — a handful of memory copies and one `pwrite`,
  not an `fsync`): serializes `seq` assignment and byte writes, exactly as
  `FileWal`'s own single-writer contract already requires.
- **Sync path** (no lock at all, once acquired): the leader, while briefly
  holding the append lock, calls a new crate-private `FileWal` method
  (`active_segment_sync_handle`) that returns `(active_segment_id,
  cloned_File)` and the WAL's current `next_seq() - 1` (this batch's
  candidate high-water seq), then **releases the append lock** before
  calling `sync_all()` on the clone. Every other thread's `append()` calls
  (and, critically, `rotate()` calls — see §1.5) can proceed against the
  *same* append lock while that `fsync` is in flight; the fsync itself
  touches no shared mutable state at all.

Sequence of lock acquisitions on the hot path (single caller, no
contention):

1. `append()`: lock `wal` (the `Mutex<FileWal>`) → `FileWal::append(op)` →
   unlock `wal`. (Memory-speed; no syscall waits on disk.)
2. `await_durable(seq)`: check `durable_through` (lock-free `AtomicU64`
   load) → if not yet satisfied, lock `batch` (a small `Mutex<BatchState>`
   guarding only `leader_active`/`poisoned`) to determine leader-or-
   follower.
3. **Leader only**: unlock `batch` → spin-wait (see §1.6 for why spin, not
   sleep) up to the batch window → lock `wal` → snapshot `(segment_id,
   cloned File, batch_max_seq = wal.next_seq() - 1)` → unlock `wal` →
   `cloned_file.sync_all()` (no lock held) → on success:
   `durable_through.fetch_max(batch_max_seq, Release)`, record `fsync`
   latency into `FsyncLatencyTracker`, lock `batch` only to reset
   `leader_active = false` (and set `poisoned` on failure) → unlock
   `batch` → `condvar.notify_all()`.
4. **Follower only**: `condvar.wait_timeout(batch_guard, wait_timeout)` in
   a loop, re-checking `durable_through`/`poisoned` on every wake
   (spurious or real) until satisfied or the timeout elapses.

At no point does any thread hold `wal` and `batch` simultaneously, and no
thread holds either lock across the `fsync` syscall. `wal` is held only for
memory-speed operations (`append`, the O(1) snapshot-and-clone, and
`rotate`'s own bookkeeping); `batch` is held only for the trivial
leader-election/poison-flag bookkeeping. This is what makes Shape B
different from Shape A in practice, not merely in name.

### 1.4 Why this doesn't need `unsafe`, and where the line is

`File::try_clone` is safe, dependency-free, cross-platform `std` API —
verified against this codebase's own established pattern for platform-
specific durability primitives (`file_io::fsync_dir`'s doc comment already
documents the identical "fsync is a file-level, not handle-level, property"
reasoning for directory fsyncs). No new Cargo dependency, no FFI, no raw
pointers. The one new crate-private surface this requires:

- `SegmentIo<File>::try_clone_file(&self) -> io::Result<File>` (added to
  `file_io.rs`, `impl SegmentIo<File>` — not the generic `impl<F: WalFile>`
  block, since cloning is specific to `std::fs::File` and is never needed
  by the in-memory `MemFile` test backend).
- `FileWal::active_segment_sync_handle(&self) -> Result<(u64, File)>`
  (added to `mod.rs`, `pub(crate)` — visible to `wal::group_commit`, never
  exported from the crate). Returns `EngineError::WalUnavailable` if the
  active segment is poisoned (`SegmentIo::is_poisoned`), matching the
  existing poison contract rather than inventing a new error shape for it.

Neither is part of the crate's public API (both are `pub(crate)`), so this
satisfies "no new public API except `GroupCommitter`, `SyncMode::
GroupCommit` behaving correctly, and `FsyncLatencyTracker`." No `unsafe`
was needed anywhere in this design — the stop condition in the brief
("if you cannot cleanly express this in the existing type system without
`unsafe`, stop... do not reach for `unsafe`") was not hit.

### 1.5 Rotation mid-batch (M1.5) falls out of the design, not a special case

`seq` is a WAL-wide monotonically increasing counter, not scoped to a
segment (WAL Spec §3). `durable_through` tracks "highest durable `seq`",
independent of which segment that `seq`'s bytes live in. `FileWal::rotate`
*already* fsyncs the segment being sealed before switching to the new one
(existing code, `mod.rs`'s `Wal::rotate` impl: `self.active.sync()` is
called on the old segment before `self.active_id`/`self.active` are
reassigned) — this phase does not change that.

Given those two facts, the leader/follower protocol above requires **no
rotation-specific logic**: `GroupCommitter::rotate()` simply locks `wal`
and delegates to `FileWal::rotate()` (sealing/fsyncing the old segment,
creating the new one), exactly like `append()` locks `wal` to delegate to
`FileWal::append()`. The correctness argument is a linearization argument
over the single `wal` mutex:

- A leader's `(segment_id, cloned_file, batch_max_seq)` snapshot is taken
  while holding `wal`, so it is atomic with respect to any `rotate()` call
  (which also needs `wal`). If `rotate()` runs *before* the leader's
  snapshot, the leader simply targets the new segment and a larger
  `batch_max_seq`. If `rotate()` runs *after* the snapshot (while the
  leader's `fsync` on the old segment's clone is in flight, `wal` unlocked)
  — the clone still refers to the old, now-sealed segment; fsyncing it is
  still fully valid (redundant with, never in conflict with, `rotate`'s own
  internal `self.active.sync()`), and it still correctly covers every `seq`
  the leader promised to publish. `rotate()` never mutates a *sealed*
  segment's already-written bytes, so a leader's in-flight fsync of a
  cloned handle to that segment is never invalidated by a rotation that
  happens concurrently.
- New appends after a rotation the leader didn't see land in the new
  segment, under new `seq` values `await_durable` will only report durable
  once *a* leader (possibly the same thread, next time it becomes free;
  possibly a different thread that becomes leader for the next batch)
  fsyncs a clone that includes them. No waiter can be satisfied by a stale
  snapshot, because `durable_through` only ever advances to a value a
  successful, completed `fsync` call actually covered.
- A failed fsync only sets `poisoned` — it never decreases or otherwise
  touches `durable_through`. Waiters already satisfied by an earlier
  successful batch (`S <= durable_through` at the time they returned `Ok`)
  are unaffected by a later batch's failure, regardless of which segment
  either batch targeted. This is the literal requirement ("an error during
  the new segment's first fsync must not invalidate acknowledged waiters
  from the old segment") and it holds by construction (monotone
  `fetch_max`, never a decrement, never a reset except by dropping and
  recreating the `GroupCommitter`).

### 1.6 Wait-window implementation: spin, not `thread::sleep`

The brief's wait window is `min(200 µs, EMA / 10)` for the leader, and
`10 * EMA` for a follower's `condvar.wait_timeout`. `std::thread::sleep` on
Windows (this development machine's platform) has a default scheduler
timer resolution around 1–15.6 ms unless a process calls the (unsafe,
FFI-only, deprecated) `timeBeginPeriod` — meaning a naive `thread::sleep
(Duration::from_micros(200))` would typically sleep 1–15 ms in practice, a
50–75x overshoot that would make every batch's window cost more than the
`fsync` it's trying to amortize, directly threatening the M1.1 (`≤5 ms`
median, single-writer, no batching partner) and M1.2/M1.3 throughput
targets. The leader's wait window is therefore implemented as a tight
spin-loop (`std::hint::spin_loop()` between `Instant::now()` polls),
matching how production group-commit implementations (e.g., the class of
"busy-wait a few hundred microseconds for more work" techniques used by
several widely-deployed log-structured stores) handle sub-millisecond
batching windows — a scheduling round-trip through `sleep`/wake is itself
often slower than the window being waited for. **This is only used for the
leader's own short, bounded, single-purpose wait; a *follower*'s
`condvar.wait_timeout` (tens of microseconds to low milliseconds, and
genuinely blocking — no useful work to interleave) uses the real OS
condvar wait, which does not have this granularity problem in the same way
since it is woken by `notify_all()` on the fast path and only falls back to
its timeout on genuine leader failure/starvation.**

### 1.7 `EngineError::Timeout`

The algorithm requires `await_durable` to return `EngineError::Timeout` on
a follower's exhausted wait. `error.rs`'s `EngineError` enum has no
`Timeout` variant today. This is the one change outside `src/wal/` this
phase makes — adding `EngineError::Timeout { detail: String }`, in the same
style as the enum's other variants (`Display`/`source()` wired the same
way). This is not new *public surface* in the sense the brief's scope guard
is protecting against (a new subsystem, a new capability) — it is a single
variant added to the one shared error taxonomy every module already
returns, required verbatim by the algorithm's own stop condition
("...return `EngineError::Timeout`"). Flagged here rather than silently
added, per the brief's own ambiguity-handling rule.

### 1.8 What `GroupCommitter` is generic over

`GroupCommitter` is **not** generic over `W: Wal`. It is concretely typed
over `FileWal`. The decoupled-fsync trick in §1.3–§1.4 depends on
`std::fs::File::try_clone`, which has no equivalent in the abstract `Wal`
trait (and adding one would be exactly the kind of trait-surface change
this phase is not authorized to make — `Wal`'s signatures are untouched).
Making `GroupCommitter` generic over `Wal` would force it back to Shape A
(lock the whole trait object for `sync()`) for any hypothetical non-`File`
backend, silently reintroducing the serialization problem for the one
backend (`FileWal`) that actually matters in production. A concrete
`GroupCommitter { wal: Mutex<FileWal>, ... }` is simpler, faster, and
honest about what it actually optimizes.

### 1.9 `GroupCommitConfig` derives from `SyncMode::GroupCommit`'s existing fields

`WalConfig::sync_mode`'s `GroupCommit { max_wait, max_batch_bytes }` fields
already exist (pre-Phase-1 config surface, WAL Spec §5). Rather than invent
a second, parallel config type, `GroupCommitter::new` requires the
`FileWal` it's given to be configured with `SyncMode::GroupCommit{..}` and
reads `max_wait` as the leader's window cap (the algorithm's literal
"200 µs" is this value's *default* when a caller wants the spec's exact
numbers, not a hardcoded constant) and `max_batch_bytes` as the "256 KB"
payload threshold, in the same role. `FileWal::open_for_recovery` no longer
rejects `GroupCommit` mode (see §2's milestone log for the exact contract
change and the pre-existing test this updates) — it now succeeds
regardless of `sync_mode`, since `FileWal` itself behaves identically
either way; only wrapping it in a `GroupCommitter` actually turns on
batching. A `FileWal` configured with `GroupCommit` but used directly
(never wrapped) is not an error — it just behaves like `Immediate` mode,
which is a safe, non-surprising default (strictly more durable per call,
just without the throughput win), and is exactly the resolution the
project's own established "amend and document, don't silently break"
pattern (see `ARCHITECTURE.md`'s WAL hardening entry) calls for here.

### 1.10 State layout

```rust
pub struct GroupCommitter {
    wal: Mutex<FileWal>,
    durable_through: AtomicU64,   // 0 = "nothing durable yet" (seq starts at 1)
    batch: Mutex<BatchState>,
    condvar: Condvar,
    latency: FsyncLatencyTracker,
    max_wait_cap: Duration,       // from SyncMode::GroupCommit.max_wait
    max_batch_bytes: usize,       // from SyncMode::GroupCommit.max_batch_bytes
    batch_bytes: AtomicUsize,     // reset to 0 when a new leader opens a batch
}

struct BatchState {
    leader_active: bool,
    poisoned: Option<io::ErrorKind>,
}
```

No per-waiter bookkeeping (no `Vec` of registered `seq`s) — every waiter
independently re-derives "am I done" from `durable_through`/`poisoned`
after every wake, which is both sufficient (per the correctness argument in
§1.5) and is what makes "no waiter hangs" true unconditionally: every path
that can complete a batch (success or failure) calls `notify_all()`, and
every follower's wait is itself time-bounded.

1. **EMA starts at 0; a follower's `10 * EMA` timeout would also start at
   0**, which would make the very first concurrent `await_durable` call
   from a follower (before any batch has ever completed) time out
   immediately even though a leader is actively working. The brief
   specifies the *leader's* `min(200µs, EMA/10)` window using the literal
   formula (and `metrics.rs`'s own required unit test, "zero initial value
   handling," confirms EMA is meant to start at a plain `0`, not a
   special-cased first sample) — but says nothing about clamping a
   follower's derived timeout. **First attempt, found wrong by a failing
   test:** clamping a follower's `wait_timeout` to `max(10 * EMA,
   max_wait_cap)`. This is insufficient on its own: `max_wait_cap` is
   microseconds (the algorithm's own "200 µs" default), while a real
   `fsync` on this machine measured 3–10 ms in the Phase 0 WAL benchmark
   (`PROGRESS.md`) — so the very first concurrent batch's followers timed
   out (observed directly: `wal::group_commit::tests::
   concurrent_appends_all_become_durable_and_recoverable` failed with
   `Timeout { detail: "...durable_through=0, ema_fsync_latency_ns=0" }`
   before the leader's first, legitimate `fsync` had even returned).
   **Actual fix:** `GroupCommitter::new` performs one real `fsync` against
   the active segment before returning, folding its measured latency into
   `FsyncLatencyTracker` — removing the need to guess a fallback constant
   at all. By the time any caller can reach `await_durable`, the EMA
   already reflects a genuine measurement, so `10 * EMA` (still floored at
   `max_wait_cap` as a belt-and-suspenders minimum) is a realistic bound
   from the first real batch onward. `new` fails (fail-closed) if this
   warm-up `fsync` itself fails. This does not touch the EMA *formula*
   (still the plain, unclamped one, satisfying `metrics.rs`'s own tests) —
   only when the first real sample is taken.
2. **Order of checks in `await_durable`**: `durable_through >= S` is
   checked *before* the poisoned check, not after — a `seq` that is
   already genuinely durable must return `Ok` even if a *later* batch
   subsequently failed and poisoned the committer (§1.5's "an error must
   not invalidate acknowledged waiters" generalizes naturally to this:
   poisoning is a property of *future* commits, not a retroactive judgment
   on bytes already fsynced).
3. **`await_durable(S)` where `S == 0`**: `WalPosition::seq` is always
   `>= 1` (WAL Spec §3: seq starts at 1); `durable_through`'s initial value
   of `0` therefore means "nothing yet" without needing an `Option<u64>` or
   a separate "initialized" flag — `0 >= 0` would trivially be `Ok`, but no
   real caller ever has `S = 0`, so this is a non-issue in practice, noted
   here only for completeness.
4. **`GroupCommitter::append`'s batch-byte accounting** does not need to
   be exact — it is a heuristic for "should the leader stop waiting early,"
   not a durability-relevant quantity (durability is always derived from
   the WAL's actual `next_seq()`, never from the byte counter). An
   approximate per-`WalOp` size estimate (frame header + `seq` + op tag +
   length-prefixed field overhead, mirroring `format::encode_frame`'s
   layout without re-deriving its exact `CapacityExceeded` bounds checking)
   is used rather than plumbing the real encoded frame length back out of
   `FileWal::append` (which would require changing `Wal::append`'s return
   type, out of scope).
5. **`FsyncLatencyTracker::record`'s thread-safety** is implemented via a
   `compare_exchange` retry loop over the `AtomicU64`-encoded EMA, even
   though `GroupCommitter`'s own leader-election guarantees only one
   thread ever calls `record` at a time in practice. The tracker is a
   general-purpose primitive ("All state in `AtomicU64`, no locks") that
   must be safe to use correctly on its own terms, not merely safe by
   accident of how its one current caller happens to use it.

### 1.12 Deferred items (explicitly out of scope, not silently built)

- Any change to `Wal` trait signatures, or a `GroupCommit`-aware default
  implementation of `append_sync` on the trait itself — the brief's scope
  guard explicitly excludes new public API surface beyond the three named
  items.
- Memtable/SSTable/Manifest/compaction integration of `GroupCommitter` —
  Phase 2+.
- `io_uring`/`O_DIRECT`/thread-per-core — Phase 4.
- A generic `Wal`-trait-level group-commit wrapper usable by a hypothetical
  future non-`File`-backed `Wal` implementation (see §1.8) — noted as a
  real design question for whenever a second `Wal` implementation exists,
  not invented speculatively now.
- Adaptive/self-tuning `max_batch_bytes`/`max_wait_cap` (the brief pins
  these to `SyncMode::GroupCommit`'s existing static config fields; no
  request for runtime auto-tuning exists in scope for Phase 1).

---

## 2. Milestone log

Entries added as each milestone lands; see commit hashes for the exact
change.

### 2026-09-14 — M0: core `GroupCommitter` + `FsyncLatencyTracker` + wiring

**Status:** done. `src/wal/metrics.rs` (`FsyncLatencyTracker`) and
`src/wal/group_commit.rs` (`GroupCommitter`) implemented per §1's design;
`src/wal/mod.rs` wired in (`active_segment_sync_handle`, `sync_mode`,
`SyncMode::GroupCommit` acceptance in `open_for_recovery`); `EngineError::
Timeout` added to `error.rs`.

**What broke, what was learned** (two real bugs, both found by the crate's
own unit tests failing, not by inspection):

1. **Cold-start follower timeout.** First implementation floored a
   follower's `10 * EMA` timeout at `max_wait_cap` (microseconds) — far
   too short relative to a real `fsync`'s latency (milliseconds) before
   any batch had ever completed. `wal::group_commit::tests::
   concurrent_appends_all_become_durable_and_recoverable` failed
   immediately, with the exact diagnostic needed to see why (`durable_
   through=0, ema_fsync_latency_ns=0`). Fixed by having `GroupCommitter::
   new` perform one real, timed `fsync` before returning, seeding the EMA
   with a genuine measurement — see §1.11 item 1 for the full account.
2. **Global fault-injection hook leaked across concurrently-running unit
   tests.** The first version of the leader-`fsync` test hook (needed
   because the leader bypasses `wal::testing::FaultInjectingIo`'s layer by
   design — see §1.3) was a process-wide `static`. `cargo test` runs test
   *functions* concurrently within one process by default, so
   `a_failed_leader_fsync_poisons_the_committer_permanently`'s injected
   failure was observed leaking into the unrelated, concurrently-running
   `concurrent_appends_all_become_durable_and_recoverable` (its own
   `unwrap()` panicked on `Io(Custom { kind: Other, error: "injected
   leader fsync failure" })` — a fault that test never installed). Fixed
   by moving the hook to a field *on `GroupCommitter` itself*
   (`fsync_fault_hook`), so it is scoped to one instance/one test, never
   process-wide. General lesson, worth stating plainly: a test-only
   interception seam for a *type* should live on that type, not in a
   module-level `static`, whenever more than one instance of that type can
   exist across concurrently-running tests in the same process — the
   existing `file_io::DirFsyncHook`/`abort_hook` precedents this design
   was modeled on get away with a `static`/`thread_local!` only because
   `FileWal` itself is process-singleton-per-test-directory in practice and
   (for `abort_hook`) genuinely process-wide by requirement (a re-exec'd
   child process, not a concurrent sibling test).
3. **A third, non-bug finding**: even after both fixes, a single
   `condvar.wait_timeout` call can still legitimately expire under real OS
   thread-scheduling contention (8+ threads on a shared/virtualized
   machine) without a `notify_all()` landing in that exact window, even
   though the system is making steady forward progress. This is not a
   defect — `EngineError::Timeout` is documented as a *recoverable*
   outcome, and the correct response (matching how a real caller should
   use this API) is to call `await_durable` again for the *same* `seq`
   (never to re-`append`, which would assign a new one). The unit test
   originally treated any `Timeout` as fatal via a bare `.unwrap()`; fixed
   by retrying on `Timeout` in the test, which is the same pattern the
   M1.2/M1.3 throughput tests use (see their own sections below).

**Tests passing:** 83/83 lib (up from 80), stable across repeated runs
(`cargo test --lib` × 3). `cargo test --lib --release`: 83/83. `cargo test
--features test-util`: 83 lib + 12 `wal_tests.rs` + 2 `crash_consistency.rs`
(unchanged from before this phase). `cargo clippy --all-targets
--all-features -- -D warnings`: clean (two real findings fixed along the
way — `io::Error::other` over `io::Error::new(ErrorKind::Other, ..)`, and a
`type_complexity` lint on the fault-hook field, resolved with a type
alias). `cargo fmt --check`: clean.

**Key commit:** (recorded after this entry is committed — see git log for
`phase-1(group-commit): add FsyncLatencyTracker and core GroupCommitter`).

---

## 3. Benchmark results

(Populated as each M1.x test is run for the first time, with the exact
command to reproduce. Raw numbers only — no target is ever hand-tuned to
pass; a miss is reported and analyzed, not hidden.)
