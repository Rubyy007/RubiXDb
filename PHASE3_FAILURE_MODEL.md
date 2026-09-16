# RubiXDB Phase 3 — Failure Model

Companion to `PHASE1_FAILURE_MODEL.md`/`PHASE2_FAILURE_MODEL.md`/
`PHASE2B_FAILURE_MODEL.md` — every failure mode those files document
still applies unchanged. This file covers what Phase 3 adds: the
leader-failure state machine and its fix (Increment 3A, the P0 item),
plus the scope of what Phase 3 has **not yet** reached (see §5).

## 1. The P0 issue this increment fixes

`PHASE2B_FAILURE_MODEL.md` §3 diagnosed, but did not fix, a real
availability gap in `GroupCommitter` (`src/wal/group_commit.rs`,
unchanged since Phase 1): a leader thread that panics **after** being
elected (`BatchState::leader_active = true`) but **before** reaching
`finish_batch_ok`/`finish_batch_with_error` leaves `leader_active`
stuck `true` forever. Consequence, pre-fix:

- No new leader can ever be elected on that `GroupCommitter` again.
- Every subsequent `await_durable` call (any thread, any architecture —
  Approach A/B/C or a direct Phase 1 caller) becomes a follower forever
  and only ever discovers the problem after riding out its own
  `follower_wait_timeout()` (`10 * EMA`, floored at `max_wait_cap`) —
  bounded, but repeated in full on **every single call**, forever, until
  the process reconstructs a fresh `GroupCommitter`.
- `GroupCommitter::shutdown()` pays its own fixed `SHUTDOWN_DRAIN_BOUND`
  (5s) waiting for `leader_active` to clear before giving up anyway.

This is exactly the scenario Phase 3's operating brief names as P0/P1:
"a leader panic occurring during a critical Group Commit operation can
leave `leader_active = true` and prevent future progress."

## 2. State machine

`GroupCommitter`'s durability-relevant state, in the vocabulary the
brief asks for (implementation names differ slightly — noted below —
because they were already in place and Phase 3 minimizes invented
surface rather than renaming a working design):

```text
                 no leader_active, not poisoned
                            |
                            v
                  ,---> IDLE/BATCHING <---------------------,
                  |          |                               |
                  |   a caller wins the leader race           |
                  |   (leader_active: false -> true,          |
                  |    under `batch`)                         |
                  |          |                                |
                  |          v                                |
                  |   LEADER_ACTIVE                            |
                  |   (spin_wait_for_batch_window,             |
                  |    snapshot_sync_target)                   |
                  |          |                                |
                  |          v                                |
                  |     SYNCING (do_leader_fsync)              |
                  |      /        \                            |
                  |     v          v                           |
                  | SYNC_SUCCEEDED  SYNC_FAILED                |
                  |     |               |                      |
                  |     v               v                      |
                  | durable_through  POISONED (FsyncFailed) ---'
                  | advances,       (permanent, terminal)
                  | leader_active=false,
                  | notify_all ------------------------------> IDLE
                  |
                  |  [ANY POINT ABOVE]: thread panics
                  |  (unwinds through LeaderFailureGuard)
                  '--------------------------------------------> POISONED
                                                                  (LeaderPanicked)
```

Mapping to the brief's requested vocabulary:

| Brief's name | This implementation |
|---|---|
| `IDLE` | `leader_active == false && poisoned.is_none()` |
| `LEADER_ACTIVE` | `leader_active == true` (spans batch-window wait + snapshot) |
| `BATCHING` | the batch-window wait inside `LEADER_ACTIVE` (`spin_wait_for_batch_window`) |
| `SYNCING` | `do_leader_fsync` in flight |
| `SYNC_SUCCEEDED` | the moment `do_leader_fsync` returns `Ok` (transient — immediately followed by watermark publication) |
| `SYNC_FAILED` | the moment `do_leader_fsync` returns `Err` (transient — immediately followed by `finish_batch_with_error`) |
| `RECOVERING` | not a `GroupCommitter`-internal state — recovery happens *outside* this type, via `FileWal::open_for_recovery` on a freshly reopened directory (§4) |
| `POISONED` | `BatchState::poisoned.is_some()` — permanent, terminal (`PoisonReason::FsyncFailed` or `PoisonReason::LeaderPanicked`) |
| `SHUTTING_DOWN` | `GroupCommitter::shutting_down` (`AtomicBool`, orthogonal to the above — see §6) |

**Why no separate `RECOVERING` state inside `GroupCommitter`**: per this
project's standing rule ("do not duplicate durability logic in the
MemTable layer" generalizes to "do not duplicate `FileWal`'s recovery
logic in `GroupCommitter`" — `PHASE2B_FAILURE_MODEL.md` §3's own closing
line already commits to this), a poisoned `GroupCommitter` is not
repaired in place. The only recovery path is: drop it, call `FileWal::
open_for_recovery` again (which re-scans every segment from disk and
only ever trusts bytes a completed `fsync` actually wrote), and
construct a fresh `GroupCommitter` over the result. `RECOVERING` is
therefore a caller-level procedure, not a `GroupCommitter`-internal
state — see §4.

## 3. Who owns leader state, how it is released, what happens on panic

- **Ownership**: `BatchState` (`leader_active: bool`, `poisoned: Option<
  PoisonReason>`), guarded by `GroupCommitter::batch` (`Mutex`). Exactly
  one thread may hold `leader_active == true` at a time, enforced by the
  same lock used to test-and-set it in `await_durable`.
- **Normal release**: the leader thread itself, via `finish_batch_ok`
  (success) or `finish_batch_with_error` (a real `fsync` `Err`) — both
  already existed pre-Phase-3 and are unchanged.
- **Release on panic (the fix)**: `LeaderFailureGuard`, constructed in
  `await_durable` in the same statement that sets `leader_active = true`
  (before `run_as_leader()` is called), disarmed only after `run_as_
  leader()` returns normally. If the leader thread instead unwinds
  (panics) anywhere inside `run_as_leader` — before, during, or after
  the `fsync` syscall itself, indistinguishably from this guard's point
  of view — `Drop::drop` runs during the unwind and:
  1. locks `batch`,
  2. sets `leader_active = false`,
  3. sets `poisoned = Some(PoisonReason::LeaderPanicked)` if not already
     poisoned,
  4. releases the lock and calls `condvar.notify_all()`.

  This mirrors `execution::common::CompletionGuard`'s existing
  armed-unless-disarmed pattern exactly — not a new abstraction
  introduced for this fix.
- **Followers**: every follower re-derives its own outcome from
  `durable_through`/`poisoned` on every wake (no per-waiter registry,
  unchanged design principle from Phase 1) — see `await_durable`'s wait
  loop. Once the guard's `Drop` sets `poisoned` and calls `notify_all`,
  every follower currently parked in `condvar.wait_timeout` wakes,
  re-checks, and returns `Err(poisoned_error(LeaderPanicked))`
  immediately — it does not wait out its own timeout.
- **Queued requests** (at the `execution::*` pool layer, above
  `GroupCommitter`): unaffected by this fix directly — each pool's own
  `process_batch` already builds every entry's `CompletionGuard` before
  the shared `await_durable` call (`PHASE2B_FAILURE_MODEL.md` §2), so a
  panic propagating up from `await_durable` (which no longer panics
  itself post-fix — it returns a poisoned `Err` instead of blocking, but
  the *pool's own coordinator/worker thread* can still die from other
  causes) is already handled at that layer. What Phase 3 changes is that
  `GroupCommitter` itself never again requires a caller to discover the
  problem via timeout.
- **May a new leader take over?** No — poisoning is unconditional and
  permanent (§3.1 below explains why). The *next* caller to observe
  `poisoned.is_some()` gets a clear, bounded error; it never becomes a
  new leader for the same `GroupCommitter`.
- **Is the WAL still usable?** Yes, unconditionally — `LeaderFailureGuard`
  never touches `FileWal` at all, only `GroupCommitter`'s own in-memory
  `BatchState`/`durable_through`. §4's reopen test proves the on-disk
  WAL is untouched and fully recoverable.

### 3.1. Why poison unconditionally, rather than clearing `leader_active` and letting a new leader be elected

A panic inside `run_as_leader` can occur at any point relative to the
`fsync` syscall: before it, during it (OS-level fault, injected test
panic), or after it returned `Ok` but before `durable_through` was
published. `LeaderFailureGuard` cannot distinguish these cases from the
information available to it (there is no partial-progress marker to
consult — adding one would be new state to keep consistent, for a
benefit this design does not need: see below). Three options were
considered:

1. **Clear `leader_active`, do not poison** — let the very next caller
   become a new leader. Rejected: if the panic happened *after* a
   successful `fsync` but before `durable_through` advanced, this is
   actually safe (the next leader's own `fsync` would flush the same
   already-durable bytes again, and `durable_through` would catch up
   correctly) — but if the panic happened *during* an `fsync` whose
   outcome on disk is unknown (some OS/filesystem combinations do not
   guarantee an interrupted `fsync` is a no-op), silently proceeding
   risks building forward state on an unconfirmed foundation. This
   project's stated Non-Negotiable rules ("do not advance durability
   incorrectly," "do not silently swallow worker/coordinator failures")
   argue against optimistic self-healing here.
2. **Poison unconditionally** (the chosen design) — every batch outcome
   this `GroupCommitter` was ever uncertain about becomes a hard stop.
   Matches the *existing*, already-accepted behavior for a returned
   `fsync` `Err` (`finish_batch_with_error`) exactly — a panic is simply
   another way a batch fails to confirm its own outcome, not a
   different category of failure requiring different handling.
3. **Attempt in-process repair** (re-verify `FileWal`'s on-disk state,
   reconcile, resume) — rejected outright per the brief's own explicit
   instruction ("recovery must rely on the existing WAL recovery
   contract... do not create a new recovery mechanism inside Group
   Commit").

Option 2 was chosen: it is the minimal change that closes the P0 gap,
introduces no new recovery machinery, and keeps `GroupCommitter`'s
existing poison-and-reconstruct recovery story (already true for
`fsync` failures since Phase 1) as the single, uniform answer for every
way a batch can fail to complete. The cost — a `GroupCommitter` that
saw a leader panic must be discarded even in the case where the `fsync`
had actually already succeeded — is a strictly better outcome than the
pre-fix behavior (permanently degraded, not discarded) in every
respect: it fails faster, it fails more legibly (`PoisonReason::
LeaderPanicked` vs. a generic timeout), and it still never loses data
(§4 proves the reopen recovers everything the `fsync`, if it ran to
completion, actually wrote).

## 4. Recovery contract (relies on the existing WAL recovery path — nothing new)

1. Caller observes `GroupCommitter::is_poisoned() == true` (or any
   `await_durable`/`append_durable` call returning the `LeaderPanicked`
   `Err`).
2. Caller drops the poisoned `GroupCommitter` (and, if using one of the
   `execution::*` pools, the pool wrapping it — those already fail
   cleanly to `PoolState::Failed`, per `PHASE2B_FAILURE_MODEL.md` §1).
3. Caller calls `FileWal::open_for_recovery` again on the same
   directory. This re-scans every segment from disk (WAL Spec §6),
   applying the existing torn-tail/corruption rules unchanged — it does
   not consult or trust any of the poisoned `GroupCommitter`'s in-memory
   state.
4. Caller constructs a fresh `GroupCommitter::new(wal)` over the result.
   `durable_through` is correctly re-seeded from `wal.durable_seq()`
   (unchanged Phase 1 behavior, `GroupCommitter::new`'s own doc comment)
   — i.e., from what recovery actually proved durable, not from
   whatever the old committer's watermark happened to be.

**Verified, not merely asserted**: `wal::group_commit::tests::
leader_panic_clears_leader_active_poisons_and_recovers_cleanly_on_
reopen` performs exactly this sequence end-to-end — durable-before-panic
record survives; the reopened WAL is uncorrupted; sequences are
gap-free; a fresh `GroupCommitter` over the reopened `FileWal` accepts
and durably commits new writes normally, with `is_poisoned() == false`.

## 5. What Phase 3 has NOT yet reached (explicitly, not silently)

Per this project's standing "explicitly not done yet" discipline
(`PROGRESS.md`), Increment 3A (this document's own scope) covers only
the leader-failure P0 item (operating brief §5–§10 for the specific
case of a panicking leader thread). The following sections of the
Phase 3 operating brief are **not yet implemented**:

- §7's full audit of "may the failed leader leave a partially written
  WAL frame" — not separately investigated this increment; `FileWal`'s
  existing `SegmentIo::append` rollback-on-partial-write-failure (Phase
  1 hardening pass, `ARCHITECTURE.md`'s "WAL hardening pass" section)
  already covers a *write*-time partial failure; a panic specifically
  *inside `fsync`* (this increment's scenario) never touches the
  in-progress write path at all, so no new torn-frame risk was found,
  but this was not exhaustively fuzzed as its own dedicated exercise.
- §8's full fault-injection matrix (before/after leader election, during
  batch formation, before/after snapshot, before/during/after sync,
  after watermark publication, before/after completion, coordinator
  panic distinct from leader panic, shutdown-during-batching/syncing/
  completion) — only the "leader panics during `fsync`" point (already
  reachable via `install_fsync_fault_hook`) has a dedicated deterministic
  test this increment. The existing `AbortPoint` enum (11 variants,
  `wal::mod`) already covers most of the *process-abort* half of this
  matrix (via `tests/crash_consistency.rs`) for the underlying `FileWal`
  layer; extending it with the remaining GroupCommit-specific points is
  future work.
- §11 (Dedicated Batch Coordinator lifecycle/failure policy formalized
  as its own state machine) — `BatchCoordinatorPool`'s existing
  `PoolState` (`Running`/`Draining`/`Stopped`/`Failed`) already
  implements the brief's requested shape (§11's "or transitions the
  storage engine to failed/unavailable" branch — chosen and already
  verified, `coordinator_panicking_fails_safely_and_rejects_further_
  work`), but was not re-examined or extended this increment.
- §13–§17 (long-duration soak tests, repeated crash testing at
  controlled intervals, resource-exhaustion testing) — not run this
  increment.
- §18–§19 (production-grade metrics/logging layer) — not built this
  increment; `GroupCommitStats`/`BatchCoordinatorStats` (Phase 1/2B)
  already cover a meaningful subset (§18's `sync_count`, `sync_failures`
  via `sync_failures()`, `records_per_batch` via `avg_batch_records()`,
  `queue_depth`, etc.) but the brief's full metric list and its
  contention-safety requirement were not audited end-to-end.
- Stage B (MemTable) in its entirety — §20–§30 — not started.

None of the above is silently dropped — each is a concrete, named item
for the next Phase 3 increment.

## 6. Failure table addendum (extends `PHASE2B_FAILURE_MODEL.md` §1)

| Failure | Detection | Response (post-3A) | Caller-visible outcome |
|---|---|---|---|
| Leader thread panics inside `run_as_leader` (any point) | `LeaderFailureGuard::drop` runs during unwind | `leader_active -> false`, `poisoned -> Some(LeaderPanicked)`, `notify_all` — all inside the unwind, before the panic propagates further | The panicking caller's own call unwinds (propagates the panic, as before — this fix does not catch/suppress it); every *other* waiter (follower or a later caller) gets `Err(EngineError::Io)` **immediately**, not after a timeout |
| A later `await_durable`/`append_durable` call on the same (now-poisoned) `GroupCommitter` | `poisoned.is_some()` checked on every loop iteration, same as an `fsync`-failure poison | Immediate `Err`, no batch attempted, no new `fsync` | Bounded, fast, clearly labeled (`PoisonReason::LeaderPanicked` in the error text) |
| Caller wants to recover after a leader panic | N/A (caller-driven) | Discard the poisoned `GroupCommitter`; `FileWal::open_for_recovery` on the same directory; construct a fresh `GroupCommitter` | Works exactly as a fresh startup would — see §4 |
