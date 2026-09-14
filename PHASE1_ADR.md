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
