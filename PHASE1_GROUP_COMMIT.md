# RubixDB Phase 1 — Group Commit State Machine and Durability Model

Design only. No test results or numbers — see `PHASE1_TEST_RESULTS.md`.

## 1. State machine

`GroupCommitter` does not materialize these names as an enum — the states
below are the *conceptual* states its actual fields (`BatchState.filling_
active`, `BatchState.fsyncing_active`, `BatchState.poisoned`, `shutting_
down`, `durable_through`) compose into. This section names them
explicitly because the states and transitions are the contract,
independent of how they happen to be encoded.

**Revision note (pipelining):** a batch's lifecycle used to be a single
`LEADER_ACTIVE` span held by one thread from election through a
completed `fsync`. It is now two independently-gated sub-states,
`FILLING` and `FSYNCING`, so that batch `N+1`'s `FILLING` phase can run
concurrently with batch `N`'s `FSYNCING` phase — see `PHASE1_TEST_
RESULTS.md` §9D.4's model and §9E for what this was measured to actually
do on this environment (implemented and correct; **not currently
adopted** — see §9E's recommendation before assuming this is the shipped
behavior's performance profile).

### States

- **IDLE** — `filling_active = false`, `fsyncing_active = false`,
  `poisoned = None`, `shutting_down = false`. No batch is active in
  either phase. The steady state between one batch's `FILLING` phase
  ending (with no successor yet elected) and its successor's election.
- **FILLING** — `filling_active = true`. Exactly one caller owns the
  current batch's window-and-snapshot work, from the moment it wins
  election (`BeforeLeader` → `AfterLeaderElection`) until the
  filling→fsyncing handoff (`GroupCommitter::begin_fsyncing_phase`)
  releases it. **Not mutually exclusive with another batch's
  `FSYNCING`** — this is the whole point: while batch `N` is `FSYNCING`,
  batch `N+1` may already be `FILLING`, concurrently, so long as at most
  one batch is `FILLING` and at most one is `FSYNCING` at any instant.
- **FOLLOWERS_WAITING** — a state of the *system*, not a field: any number
  of other callers are blocked in `condvar.wait_timeout` while some batch
  is `FILLING` or `FSYNCING`. Not mutually exclusive with either — it
  describes everyone *except* the two active leaders during that
  interval.
- **BATCH_WINDOW** — a sub-state of `FILLING`: the leader is inside
  `spin_wait_for_batch_window` (`DuringBatchWaitPre` → `DuringBatchWaitPost`),
  waiting for `min(max_wait, EMA/WINDOW_EMA_DIVISOR)` or `max_batch_bytes`,
  whichever comes first — after first probing for `PROBE_WINDOW` (200µs)
  to confirm a follower has actually joined before committing to the
  full window (`PHASE1_ADR.md` ADR-12; `WINDOW_EMA_DIVISOR` was
  originally `10`, revised to `1` by that ADR's window-size sweep).
- **SNAPSHOT** — a sub-state of `FILLING`, after the window closes: the
  leader takes `(cloned file, batch_max_seq)` under a brief `wal` lock
  (`snapshot_sync_target`). `FILLING` ends immediately after this, at the
  filling→fsyncing handoff — not after `fsync` completes.
- **FSYNCING** — `fsyncing_active = true`, entered via `begin_fsyncing_
  phase`'s handoff (which, in the same critical section, clears the
  outgoing batch's `filling_active`). Executes the `fsync` (`BeforeSync`
  → the syscall → `AfterSync`) outside any lock. Strictly one batch at a
  time system-wide — a second batch reaching the handoff while this one
  is `FSYNCING` blocks (on `condvar`, unbounded — see `begin_fsyncing_
  phase`'s doc comment for why an internal leader-to-leader handoff does
  not need `await_durable`'s bounded-`Timeout` contract) until this one
  finishes, preserving FIFO `fsync` order across batches without a
  separate queue.
- **SYNC_SUCCESS** — the `fsync` returned `Ok`. `durable_through` is
  published via `fetch_max` (`AfterWatermarkBeforeWake`), then every
  waiter is woken (`condvar.notify_all()`) and `fsyncing_active` returns
  to `false`. Transitions to **IDLE** (unless a successor is already
  `FILLING`, in which case the system moves directly to that successor's
  eventual **FSYNCING**, with no observable **IDLE** interval).
- **SYNC_FAILURE** — the `fsync` returned `Err`, or the pre-`fsync`
  snapshot itself failed (e.g. a poisoned `SegmentIo`). `poisoned` is set
  to `Some(kind)`, every waiter is woken, the failing phase's own active
  flag (`filling_active` if the snapshot failed, `fsyncing_active` if the
  `fsync` itself failed) returns to `false`. Transitions to **POISONED**,
  not back to **IDLE** — this is the one state this machine never
  recovers from on its own.
- **POISONED** — `poisoned = Some(_)`. Permanent for the lifetime of this
  `GroupCommitter`. Every `await_durable` call for a not-yet-durable `seq`
  fails immediately with the stored error class; no new **FILLING** phase
  is ever elected again. A batch that was already **FILLING** when a
  *different*, already-**FSYNCING** batch fails discovers the poison at
  its own filling→fsyncing handoff (`begin_fsyncing_phase` returns
  `Err(kind)`) rather than at election time — it never attempts its own
  `fsync`, and reports the same poisoned error every other caller would.
  Recovery is out-of-band: drop this `GroupCommitter` and construct a
  fresh one (which re-derives its durability baseline from `FileWal::
  durable_seq()`, itself derived from real, on-disk, `fsync`-proven
  state — see §3).
- **SHUTTING_DOWN** — `shutting_down = true`, checked orthogonally to the
  states above (it can be set from **IDLE**, **FILLING**, **FSYNCING**,
  or **POISONED**). No new **FILLING** transition is permitted once set;
  a batch already in **FILLING** or **FSYNCING** is allowed to finish
  naturally (an in-flight `fsync` cannot be interrupted regardless).
  Terminal in practice — this crate exposes no "resume" operation.

`UNAVAILABLE` (the brief's name for "the underlying WAL entered an
unrecoverable state") is `POISONED` in this implementation — there is no
separate WAL-level unavailability distinct from a failed `fsync`, since
`FileWal`'s own poisoning (`SegmentIo::is_poisoned`) surfaces through the
same `fsync`/snapshot failure path and the same `POISONED` state.

### Legal transitions

```
IDLE --[caller finds filling_active=false]--> FILLING
FILLING --[window/byte threshold]--> BATCH_WINDOW closes --> SNAPSHOT
SNAPSHOT --[snapshot Err]--> SYNC_FAILURE --> POISONED
SNAPSHOT --[snapshot Ok]--> filling->fsyncing handoff:
    clears this batch's filling_active (successor may now enter FILLING)
    --[fsyncing_active already held by another batch]--> blocks until free
    --[poisoned observed at handoff]--> SYNC_FAILURE (no fsync attempted)
    --[fsyncing_active claimed]--> FSYNCING
FSYNCING --[fsync Ok]--> SYNC_SUCCESS --> IDLE (or successor's FILLING, already in progress)
FSYNCING --[fsync Err]--> SYNC_FAILURE --> POISONED
ANY STATE --[shutdown() called]--> SHUTTING_DOWN flag set (orthogonal)
POISONED --[nothing]--> POISONED (only exit: drop and reconstruct)
```

There is no `FILLING -> FILLING` self-transition and no way for two
callers to be `FILLING` simultaneously (same reasoning for `FSYNCING`):
each `false -> true` flip happens under `batch`'s lock, read-modify-write,
so exactly one caller performs each flip per batch cycle. A batch's
`FILLING` and the *previous* batch's `FSYNCING` are the only two phases
ever concurrent with each other.

## 2. Mandatory durability invariants, and where each is enforced

1. **A `seq` is never reported durable before its bytes have completed
   `fsync`.** `durable_through` only ever advances via `fetch_max`
   *after* `do_leader_fsync` has returned `Ok` (`run_as_leader`); no other
   code path writes to it.
2. **`durable_through` is monotonic.** The only writer is `fetch_max`
   with `Ordering::Release`; `fetch_max` cannot decrease a value.
3. **A failed sync never advances the watermark.** The `Err` branch of
   `run_as_leader`'s `match` never touches `durable_through` — it only
   sets `poisoned`.
4. **A waiter only completes successfully when its `seq <=` the
   successfully-synced watermark.** `await_durable`'s only `Ok(())` path
   is the `durable_through.load() >= seq` check, evaluated fresh on
   entry and on every loop iteration.
5. **A waiter is never lost.** No per-waiter registry exists to lose an
   entry from (§1.10 of `PROCESS.md`) — every waiter independently
   re-polls shared state. `finish_batch_ok`/`finish_batch_with_error`
   both call `condvar.notify_all()` unconditionally, so no outcome (batch
   success, batch failure, or shutdown) can be reached without waking
   every follower.
6. **A waiter is never completed twice.** `await_durable` returns exactly
   once per call — there is no callback/completion object that could be
   invoked more than once; this is ordinary Rust call/return, not an
   async completion model with a separate "complete" step.
7. **A leader failure does not leave followers permanently blocked.**
   Every follower's `condvar.wait_timeout` carries a bound (`10 * EMA`,
   floored at `max_wait_cap`); worst case, a follower observes a stale
   state until its own timeout and returns `EngineError::Timeout` —
   never an unbounded wait.
8. **Sync failure propagation is deterministic.** Every waiter's next
   loop iteration (whether reached via `notify_all` or via its own
   timeout) re-checks `poisoned` before anything else and returns the
   same error class every time (`poisoned_error(kind)`, reconstructed
   from the stored `io::ErrorKind` since `io::Error` is not `Clone`).
9. **A new batch never inherits a completed batch's error state.**
   `poisoned` is a one-way latch — once set, `await_durable` never
   attempts leader election again (checked before the `leader_active`
   branch), so there is no "new batch" after poisoning to inherit
   anything into.
10. **Rotation never lets the watermark skip unsynchronized records.**
    A leader's `batch_max_seq` is captured from `FileWal::next_seq() - 1`
    at snapshot time, under the same lock `rotate()` requires — the two
    operations linearize; `rotate()` cannot silently move `durable_
    through` past bytes that were never `fsync`ed (see `PHASE1_
    ARCHITECTURE.md` §4).
11. **An acknowledged `seq` remains recoverable after process failure.**
    Enforced by the underlying, frozen `FileWal` recovery contract, not
    reinvented here — Phase 1 never bypasses `FileWal`'s own framing/CRC/
    torn-tail logic; it only decides *when* to call `fsync` and *when* to
    tell a caller the result covers their `seq`.
12. **No duplicate `seq`.** `seq` assignment happens exactly once, inside
    `FileWal::append` (pre-existing, unchanged, single-writer-serialized
    by the `wal` lock) — `GroupCommitter` never assigns a `seq` itself.
13. **No reordering.** `append()`'s critical section is the only place a
    `seq` is assigned and the only place bytes are written for it;
    `GroupCommitter` never buffers or reorders ops relative to that
    assignment order.
14. **No caller observes durability before the physical sync succeeds.**
    Same enforcement point as invariant 1 — there is exactly one place
    `durable_through` is written, and it is strictly after a successful
    `fsync` returns.

## 3. Recovery after `POISONED`

A poisoned `GroupCommitter` is discarded, not repaired in place — the
documented path is `drop` it and call `FileWal::open_for_recovery` again,
then construct a fresh `GroupCommitter::new` over the result.
`durable_seq()` (a `FileWal` field advanced only inside `sync()`/
`rotate()`, after a real `fsync`) seeds the new committer's `durable_
through` baseline, so nothing the old committer never proved durable can
be silently treated as durable by the new one, and nothing it *did* prove
durable needs to be re-earned.
