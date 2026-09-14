# RubixDB Phase 1 — Architecture Decision Records

Decisions and rationale, no results/numbers (see `PHASE1_TEST_RESULTS.md`).
Full reasoning for each is in `PROCESS.md` §1 (written before
implementation) and §2 (milestone log, written as each landed); this file
is the condensed index.

## ADR-1: Shape B — decouple `fsync` from the append lock via `File::try_clone`

**Decision**: the leader `fsync`s a `std::fs::File::try_clone`'d handle
to the active segment, outside the `Mutex<FileWal>` that guards `append`.

**Rationale**: `fsync` is a property of the underlying file, not the
handle. Cloning is safe, dependency-free `std` API. Wrapping the whole
`FileWal` in one lock (Shape A) would block every follower's `append()`
for the duration of whichever `fsync` is currently in flight, undermining
the throughput group commit exists to provide.

**Alternatives rejected**: Shape A (single lock for everything) — simpler
but measurably worse under load; a generic `Wal`-trait-level group-commit
wrapper — would force Shape A for any non-`File` backend, so `GroupCommitter`
is concretely typed over `FileWal` instead (ADR-6).

## ADR-2: No per-waiter registry

**Decision**: every waiter re-derives "am I durable yet" from a single
`AtomicU64 durable_through`, never from a per-waiter data structure keyed
by `seq`.

**Rationale**: `durable_through` is a total order over "durable so far" —
any waiter can check its own `seq` against it in O(1) with no registry to
maintain, insert into, or clean up. This is also what makes rotation need
no special-case logic (ADR-3) and what makes backpressure (ADR-7) a
simple counter rather than a bounded queue.

## ADR-3: Rotation needs no group-commit-specific logic

**Decision**: `GroupCommitter::rotate()` is a thin delegation to `FileWal::
rotate()` under the same lock `append()`/the leader's snapshot use.

**Rationale**: `seq` is WAL-wide, not segment-scoped; a leader's sync
target is snapshotted atomically with respect to `rotate()` because both
require the same `wal` lock. `FileWal::rotate()` already `fsync`s the
segment being sealed (pre-existing behavior) before creating the next
one. The linearization argument is in `PROCESS.md` §1.5.

## ADR-4: Leader wait window spins-then-yields, never `thread::sleep`

**Decision**: `spin_wait_for_batch_window` polls `Instant::now()` in a
tight loop (`std::hint::spin_loop()`), calling `std::thread::yield_now()`
every 10,000 iterations.

**Rationale**: at the algorithm's sub-millisecond scale, `thread::sleep`'s
OS timer-resolution overshoot (especially on Windows, commonly 1–15ms)
would cost more than the window is worth. The periodic yield bounds
worst-case CPU burn if a caller configures a much larger `max_wait`.

**Tried and reverted**: applying the same spin-first idea to the `wal`/
`batch` *locks themselves* (spin on `try_lock` before a blocking `lock`).
Measured worse under this development machine's real load (100+ threads
on 8 cores) — spinning starves the actual lock holder under CPU
oversubscription. Reverted; documented as a negative result in
`GroupCommitter::lock_wal`'s doc comment and `PROCESS.md`'s M1.2 entry
rather than silently dropped.

## ADR-5: `FileWal::durable_seq`, not `next_seq() - 1`, seeds `durable_through`

**Decision**: `FileWal` gained a `durable_seq: u64` field, advanced only
inside `sync()`/`rotate()` after a real, successful `fsync`.
`GroupCommitter::new` reads this, not `next_seq() - 1`.

**Rationale**: `next_seq() - 1` means "assigned," not "durable." A caller
that appends to a raw `FileWal` without syncing before handing it to
`GroupCommitter::new` would, under the old logic, have those records
silently treated as already durable. `durable_seq()` cannot make that
mistake — it only moves on a genuine `fsync`. Found and fixed during a
review pass (`PROCESS.md`'s M0.1 entry), not present in the initial cut.

## ADR-6: `GroupCommitter` is concretely typed over `FileWal`, not generic over `Wal`

**Decision**: no `GroupCommitter<W: Wal>` — one concrete type.

**Rationale**: ADR-1's decoupling depends on `std::fs::File::try_clone`,
which has no equivalent in the abstract `Wal` trait. Genericizing would
force a fallback to Shape A for any hypothetical non-`File` backend,
silently reintroducing the exact problem ADR-1 solves for the one backend
that actually exists in this crate.

## ADR-7: Backpressure is a bounded permit count, not a bounded queue

**Decision**: `pending_waiters: AtomicUsize`, capped at `max_pending_
waiters`; a caller past the cap fails immediately with `CapacityExceeded`
rather than blocking for room.

**Rationale**: per ADR-2, there is no queue to bound in the first place —
the only thing that scales with concurrent callers is how many OS threads
are blocked in `condvar.wait_timeout` at once, which is exactly what this
counter tracks. Failing fast (rather than queuing for a slot) avoids
introducing the unbounded-queue failure mode this control exists to
prevent, one layer up.

## ADR-8: `shutdown()` is a flag plus a bounded drain, not a consuming call

**Decision**: `shutdown(&self) -> ShutdownReport`, not `shutdown(self)`.

**Rationale**: matches every other `GroupCommitter` method's `&self`
signature and lets a caller holding it via `Arc` call `shutdown()` from
one thread while others still hold references. The bounded drain
(`SHUTDOWN_DRAIN_BOUND`) gives an in-flight leader a real chance to finish
without ever making `shutdown()` itself capable of hanging.

## ADR-9: Reuse `EngineError` variants; add only `Timeout`

**Decision**: backpressure reuses `CapacityExceeded`, shutdown reuses
`Aborted`, construction misuse reuses `Unsupported` — all pre-existing.
The one addition to the shared error enum is `EngineError::Timeout`,
required verbatim by the algorithm's follower-timeout path.

**Rationale**: minimizes new public surface on a type (`EngineError`)
shared by every module in the crate; each reused variant's existing
semantics already fit the Phase 1 case exactly (see `PHASE1_FAILURE_
MODEL.md` §1's table).

## ADR-10: Per-instance fault-injection hook, not a process-wide global

**Decision**: `install_fsync_fault_hook`/`clear_fsync_fault_hook` store
the injected closure in a field on the `GroupCommitter` instance being
tested, not a module-level `static`.

**Rationale**: an earlier, `static`-based version caused one test's
injected failure to leak into a different, concurrently-running test's
unrelated `GroupCommitter` — `cargo test` runs test functions
concurrently within one process by default. Found and fixed during
development (`PROCESS.md`'s M0 entry); the general lesson (a test seam
for a *type* belongs on that type when more than one instance can exist
across concurrently-running tests) is recorded there in full.

## ADR-11: Load-test harness is write-only; CPU/RSS/flame-graph profiling is skipped

**Decision, made with the user**: the §16 load harness measures writes
only (no 80/20 read/write split); CPU utilization, RSS, and a literal
flame graph are marked NOT VERIFIED ON THIS PLATFORM rather than pursued.

**Rationale**: this repository has no read path at all (Memtable/SSTable/
the LSM facade were never built) — measuring reads would require either
fabricating numbers for unimplemented functionality (explicitly
disallowed by the brief itself) or building a read path, which is out of
Phase 1's scope by the brief's own rules. CPU/RSS/flame-graph profiling
would require either a new dependency (e.g. `sysinfo`) or platform-
specific tooling this Windows development environment doesn't have set
up (no perf/dtrace-based flame-graph story) — both are Tier-3-style
additions this codebase has always stopped to ask about first, per
`ARCHITECTURE.md`'s own dependency-pinning discipline. The dominant-cost
analysis §20 asks for is still produced (see `PHASE1_TEST_RESULTS.md`'s
profiling section) from the diagnostic measurements already taken, in
place of a literal flame graph.

## ADR-12: window-size sweep resolves the batch-window formula, via a real experiment

**Context**: this report's original §15 attributed the M1.2/M1.3
throughput miss entirely to this machine's `fsync` latency, supported by
an experiment (`max_wait = 2ms`) a follow-up review correctly identified
as insufficient — it could only ever move the effective window ~40%
(`200µs → 280µs`, since `EMA/10 ≈ 280µs` already exceeded any larger
`max_wait`), so it never actually tested "is the window too small,"
only "does raising `max_wait` past the `EMA/10` cap help."

**Decision**: built a temporary, feature-gated experiment override
(`phase1-window-experiment` Cargo feature; `PHASE1_EXPERIMENT_MAX_WAIT_US`
/ `PHASE1_EXPERIMENT_EMA_DIVISOR` env vars, read by a small module in
`src/wal/group_commit.rs` that reduces to the exact production formula
when both are unset) and swept five window configurations — baseline
(~200µs), `max_wait=2ms`/`÷10` (~280µs), and three "uncapped" windows
(1ms, 3ms, 10ms via a `÷0` sentinel meaning "no EMA cap") — three
repetitions each, against both the M1.2 (100-writer) and M1.3
(1,000-writer) workload shapes. Full data: `PHASE1_TEST_RESULTS.md` §9A.

**Finding**: throughput scales substantially with window size (~1.7–2.2x
from baseline to the best-performing tested window, in both shapes),
plateauing — and, for 100 writers specifically, *reversing* — once the
window exceeds what the available writer count can supply. This
corrected the original report's conclusion: the `EMA/10` formula was a
real, fixable under-tuning, not solely a hardware floor. `fsync` latency
remains a genuine, unremovable floor (§9A.4, §15) — the fix narrows the
gap to target, it does not close it.

**A second finding the sweep itself could not surface**: naively adopting
the sweep's best-looking window unconditionally regressed the
*single-writer* case (M1.1: 2.905ms → 5.761ms median) — a real, measured
trade-off invisible to a sweep that only ever tested 100/1,000 concurrent
writers. Resolved with a demand-adaptive two-stage wait: probe for
`PROBE_WINDOW` (200µs, the *original* default) for any evidence of a
follower (`batch_bytes > 0`, which is reset to `0` at leader election and
can therefore only be nonzero due to another caller), and only extend
toward the full sweep-informed window if one has joined. Verified this
fully restores single-writer latency (§9B) without giving back the
throughput gain (§9, §16).

**Decision, resulting formula**: `WINDOW_EMA_DIVISOR` changed `10 → 1`
(`src/wal/group_commit.rs`), and every test/harness `max_wait` in this
repository changed `200µs → 5ms` (`tests/group_commit/support.rs` and
sibling configs, `examples/group_commit_load_test.rs`) — chosen because,
on this machine's measured EMA (~2.8ms), `min(5ms, EMA/1)` lands close to
the sweep's empirically strongest configuration (~3ms) while remaining
proportional to `fsync` latency on other hardware (a disk with 50µs
`fsync` latency yields a ~50µs window here, not a fixed multi-millisecond
one paid regardless). `PROBE_WINDOW = 200µs` was chosen to match the
*original* default exactly, since that value was already empirically
shown (the sweep's own baseline configuration) to be enough time for a
follower to join under genuine contention.

**Verification**: M1.1/M1.4/M1.5/M1.6/`watermark_monotonicity` all
re-verified passing after the fix; M1.2/M1.3 re-run three times each,
improved substantially (100 writers: ~67% of target → ~79%; 1,000
writers: ~46% → ~81%) but still below target; the full §16 load harness
and §19 regression commands re-run. Full numbers: `PHASE1_TEST_RESULTS.md`
§9, §9A, §9B, §12, §16.

**Two real, secondary bugs found and fixed while implementing this**:
(1) the experiment module's "env var unset" fallback initially hardcoded
the old `/10` divisor instead of reading the new `WINDOW_EMA_DIVISOR`
constant — caught by `cargo clippy --all-features` flagging the constant
as unused, fixed by having the fallback reference it directly so the two
paths cannot drift apart again; (2) the experiment module's own unit
tests initially set/cleared the override env vars from two separate
`#[test]` functions, which Rust's parallel test runner let race (env vars
are process-global) — the same class of bug as ADR-10's fault-hook leak —
fixed by merging both scenarios into one sequentially-run test.

**Alternatives considered**: keeping the original formula and accepting
the hardware-bound conclusion (rejected — the sweep proved that
conclusion incomplete, and the brief explicitly required running the
corrected experiment rather than accepting the original, insufficiently-
supported claim); a fully dynamic/self-tuning window with no fixed
formula at all (rejected as out of scope for this fix — a `min(max_wait,
EMA/divisor)` shape plus a demand probe was sufficient to capture most of
the sweep's gain without a larger redesign).

**Scaffolding disposition**: the `phase1-window-experiment` feature and
its one gated module remain in the tree (feature-gated, off by default,
zero effect on any build that doesn't enable it) rather than being
removed in this same change, since the brief's own instructions describe
it as "removable in a follow-up commit," not required to be removed
immediately. Removing it is a pure code-deletion, no-behavior-change
follow-up whenever desired.
