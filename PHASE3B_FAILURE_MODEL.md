# RubiXDB Phase 3B — Failure Model

Companion to `PHASE1_FAILURE_MODEL.md`/`PHASE2_FAILURE_MODEL.md`/
`PHASE2B_FAILURE_MODEL.md`/`PHASE3_FAILURE_MODEL.md` — every failure
mode those files document still applies unchanged. This file covers
what Phase 3B adds: the coordinator-level fault matrix, the completion-
safety fix it surfaced, and crash semantics per fault point.

## 1. Two distinct failure layers

Phase 3A hardened **`GroupCommitter`-level** failure (a leader thread
panicking inside `run_as_leader`, specifically around the `fsync` call —
`LeaderFailureGuard`). Phase 3B hardens the **`BatchCoordinatorPool`-level**
failure: the coordinator thread's own batch-processing loop (queue
drain, per-entry append, the shared `await_durable` call, completion
delivery, shutdown) can fail at points `GroupCommitter`'s own fault hook
cannot reach at all — `install_fsync_fault_hook` only ever fires inside
`do_leader_fsync`, deep inside `await_durable`.

These are genuinely different things: a `GroupCommitter`-level panic
means the *durability engine itself* failed mid-`fsync` (affects every
architecture built on it). A coordinator-level panic (at a point other
than inside `await_durable`) means the *scheduling layer* built on top
of a perfectly healthy `GroupCommitter` failed — e.g., in the append
loop, or while delivering completions. Both must fail safely; neither
implies the other's specific state machine.

## 2. Coordinator lifecycle (unchanged from Phase 2B, re-verified)

```text
Running -> Draining -> Stopped     (clean shutdown)
Running -> Failed                  (coordinator thread dies unexpectedly)
Draining -> Failed                 (coordinator thread dies during shutdown's own drain)
```

`PoolState` (`src/execution/batch_coordinator.rs`) already implements
exactly the shape the Phase 3 operating brief's §11/§6 asks for: no new
states were needed. `CoordinatorAliveGuard::drop` is the single place
that decides `Draining -> Stopped` vs. `(anything else) -> Failed` —
unchanged this phase; Phase 3B's new fault points exercise it more
thoroughly, they do not modify it.

**Policy decision (unchanged, re-confirmed under the new fault matrix)**:
per operating brief §11's three options (restart the coordinator;
transition to failed/unavailable; reconstruct while preserving the
WAL), this architecture's existing, deliberate choice is the second —
`PoolState::Failed`, reject further work, require the caller to
construct a fresh `BatchCoordinatorPool` (optionally over the same,
still-healthy `GroupCommitter`/WAL if that one wasn't itself poisoned;
otherwise per `PHASE3_FAILURE_MODEL.md`'s own reopen procedure). No
in-process restart is attempted — matching Approach B's own documented,
measured trade-off (`PHASE2B_ADR.md`): the Dedicated Batch Coordinator
has no standby, by design, in exchange for being the simplest of the
three Phase 2B architectures.

## 3. `CoordinatorFaultPoint` — the 7 injectable points

| Point | Fires | Models |
|---|---|---|
| `BeforeBatchFormation` | Inside the queue lock, right before `drain_available` (only once entries are present) | Coordinator dies right as it's about to start forming a batch |
| `AfterDrain` | `process_batch`'s first line, right after every entry got its `CompletionGuard` | Coordinator dies immediately after taking ownership of a batch |
| `AfterAppend` | After every entry has been appended to the WAL (or failed) | Coordinator dies right after WAL append, before requesting durability |
| `BeforeAwaitDurable` | Immediately before the one shared `await_durable` call | Coordinator dies right as it's about to wait for durability |
| `AfterDurable` | Immediately after `await_durable_retrying` returns `Ok` | Coordinator dies after durability is confirmed, before delivering completions |
| `BeforeCompletion` | Immediately before resolving the first entry's `CompletionGuard` | Coordinator dies while delivering results |
| `DuringShutdown` | The coordinator's own exit path, once it observes nothing left to drain and a terminal/draining state | Coordinator dies while exiting a real shutdown |

Defined unconditionally (mirrors `wal::mod::AbortPoint`'s own pattern —
zero-cost in production builds, no `#[cfg(...)]` needed at call sites);
only the hook storage/dispatch is `test`/`test-util`-gated.

## 4. The completion-safety gap found and fixed

**Found while writing the `AfterDrain` test**, not hypothesized in
advance: the original (Phase 2B) `process_batch` built each entry's
`CompletionGuard` only when the append loop individually reached that
entry. Entries dequeued from the shared queue (`batch: Vec<QueueEntry>`,
local to `coordinator_loop`/`process_batch`, no longer reachable from
`CoordinatorAliveGuard`'s own queue-draining fallback once removed from
the shared `VecDeque`) had **no panic-safety net at all** between
dequeue and that point. A coordinator panic in that narrow window —
exactly what `CoordinatorFaultPoint::AfterDrain` exists to test — would
have dropped every entry's `CompletionSlot` unresolved, hanging every
caller in that batch forever, silently.

**Fix**: `process_batch` now builds a `CompletionGuard` for every entry
in the batch as its very first action, before any other work — see
`src/execution/batch_coordinator.rs`'s `process_batch` doc comment. A
panic at *any* point in the function, including every
`CoordinatorFaultPoint` above, now resolves every entry via some
guard's `Drop` fallback.

**Verified, not merely reasoned about**: all 7 `coordinator_panic_*_
fails_safely` tests (one per fault point) submit 5 requests each, panic
the coordinator at the named point, and assert every completion resolves
(none hang), at least one reports failure (never universal false
success), the pool reaches a terminal state and rejects further work,
and the WAL remains fully recoverable and gap-free afterward.

## 5. Crash semantics per fault point (operating brief §4)

For every `CoordinatorFaultPoint`, `assert_coordinator_panics_safely_at`
(`src/execution/batch_coordinator.rs`) checks the same fixed checklist:

| Property | How verified |
|---|---|
| Whether data was appended | Implicit — recovery below only ever sees what `FileWal::append` actually wrote; no test asserts a specific count *must* be appended at a given fault point, since that legitimately varies by point (e.g. `AfterDrain` fires before any append; `AfterAppend` fires after all of them) |
| Whether it was durable | `replay.records` after reopen — only entries whose `fsync` genuinely completed appear |
| Whether the caller was allowed to observe success | Every completion is asserted to resolve; `any_err` (at least one caller sees a failure) is asserted at every point — no point produces universal false success |
| WAL recoverable | `FileWal::open_for_recovery` succeeds, `corrupted_segments.is_empty()` |
| Durable sequence / recovered sequence | Gap-free, in-order, asserted via the recovered `(seq, _)` pairs |
| Tail valid or truncated | Implicit in `corrupted_segments.is_empty()` — `FileWal`'s own recovery contract (unchanged, Phase 0/1) already classifies a torn tail correctly; Phase 3B does not add a new recovery mechanism, per the operating brief's own explicit instruction |
| Next startup succeeds | `FileWal::open_for_recovery` after the panic, in the same test |
| Fresh `GroupCommitter`/pool constructible | Every test's own final assertions rely on being able to reopen and recover; a dedicated fresh-pool round-trip is `PHASE3_FAILURE_MODEL.md`'s own leader-panic test's job, not duplicated here |

## 6. What Phase 3B does NOT change

- `GroupCommitter`'s own `LeaderFailureGuard`/`PoisonReason` (Phase 3A) —
  unchanged, re-verified green under the new coordinator-level tests
  (which necessarily also exercise `GroupCommitter` underneath).
- The WAL binary format, recovery contract, or rotation semantics.
- The choice of `BatchCoordinatorPool` (Approach B) as the recommended
  production architecture.
