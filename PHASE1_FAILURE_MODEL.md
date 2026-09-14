# RubixDB Phase 1 — Failure Model

What fails, how, and what the caller sees. Design only — no test results
or numbers; see `PHASE1_TEST_RESULTS.md`.

## 1. Error taxonomy

`GroupCommitter` returns the crate's one shared `EngineError` (`src/
error.rs`) — no Phase-1-specific error type. Every variant it can
surface, and what it means from this component specifically:

| Variant | When `GroupCommitter` returns it | Caller-visible meaning |
|---|---|---|
| `EngineError::Io(e)` | A leader's `fsync` failed (`run_as_leader`'s `Err` branch); or a poisoned committer's stored `io::ErrorKind`, resynthesized (`poisoned_error`) | The batch that would have covered this `seq` failed physically. Once seen, this `GroupCommitter` is permanently poisoned — see §2. |
| `EngineError::Timeout { detail }` | A follower's bounded `condvar.wait_timeout` expired without the awaited `seq` becoming durable | *Not* a failure of the write itself — see §3. Bounded, recoverable, safe to retry the wait. |
| `EngineError::CapacityExceeded { requested, max }` | `acquire_waiter_permit` finds `pending_waiters >= max_pending_waiters` | Backpressure (§11 of the Phase 1 brief). No slot was consumed; safe to retry after some in-flight waiters complete. |
| `EngineError::Aborted { detail }` | `append()` or `await_durable()` called after `shutdown()` | This `GroupCommitter` is shutting down; no new batch will ever start. Not recoverable on this instance. |
| `EngineError::Unsupported { operation }` | `GroupCommitter::new`/`with_max_pending_waiters` called with a `FileWal` opened in `SyncMode::Immediate` | Construction-time misuse — this `FileWal` was never configured for batching. |
| Any `EngineError` `FileWal::append` itself can return (`CapacityExceeded` for an oversized record, `Io` for a write failure, etc.) | `GroupCommitter::append` propagates it unchanged | Same meaning as it already has for direct `FileWal` use — Phase 1 does not reinterpret it. |

## 2. Leader `fsync` failure — full propagation path

1. `run_as_leader`'s `do_leader_fsync` call returns `Err(io_err)`.
2. `finish_batch_with_error(io_err.kind())` runs: under `batch`'s lock,
   `leader_active = false` and `poisoned = Some(kind)` are set together
   (never one without the other — no window where a new leader could be
   elected while the failure is still being recorded), then `condvar.
   notify_all()` wakes every waiter unconditionally.
3. The failing caller's own `await_durable` call (the one that became
   leader) receives `Err(EngineError::Io(io_err))` directly — the
   original error, not a resynthesized one.
4. Every other waiter, on its next loop iteration (woken by step 2, or
   by its own timeout — see §1's `Timeout` row for why the two can race
   and neither is wrong), observes `poisoned == Some(kind)` and returns
   `Err(poisoned_error(kind))` — same `ErrorKind`, a fresh `io::Error`
   (the original is not `Clone`, so an equivalent one is synthesized;
   message text differs, `.kind()` does not).
5. **Permanently**: `poisoned` is never cleared. Every subsequent
   `await_durable` call for a not-yet-durable `seq` checks `poisoned`
   before attempting leader election and returns immediately. No further
   real `fsync` is ever attempted by this `GroupCommitter` instance.
6. **What remains true despite the failure**: `durable_through` is
   untouched — every `seq` that was already durable before this batch
   remains durable and continues to resolve `Ok` from `await_durable`,
   indefinitely, even after poisoning. Only `seq`s that depended on *this*
   batch (or any batch after it, since none will ever run) are affected.

## 3. `Timeout` is not data loss

A follower's `condvar.wait_timeout` expiring means exactly one thing: no
`notify_all()` landed inside that specific wait window before the bound
was reached. It does not mean:

- the write was lost (the `append()` call that produced this `seq`
  already completed and its bytes are sitting in the OS page cache,
  eligible to be covered by *any* future successful batch, including one
  this same caller starts if it retries);
- the committer is poisoned (a genuinely poisoned committer returns
  `Io`, not `Timeout` — though see the note below on why a caller might
  see `Timeout` in the last moments before poisoning is observed);
- the write will never become durable (absent a genuine leader failure,
  it eventually will, via some future batch).

The correct caller response to `Timeout` is to call `await_durable` again
for the *same* `seq` — never to re-`append`, which would assign a *new*
`seq` and leave the original one's fate unresolved. This crate's own test
suite follows this pattern throughout (`await_durable_retrying_on_
timeout`, bounded at a fixed retry count to turn "still not durable after
an unreasonable number of retries" into a fast, diagnosable test failure
rather than a hang — see `PROCESS.md`'s M0.1 entry for the real bug this
pattern was extracted from).

**Race note**: a follower's timeout and a leader's `poisoned`/`durable_
through` update can land at almost the same instant. `await_durable`
re-checks both *after* a timeout is observed (using the fresh lock guard
`wait_timeout` returns), before committing to `Err(Timeout)`, so a
follower never reports a spurious timeout when the real, definitive
outcome was available by the time it looked.

## 4. Backpressure failure

`CapacityExceeded` from `acquire_waiter_permit` is returned *before* any
lock is acquired, any leader-election attempt is made, or any `condvar`
wait begins — the rejected caller consumes no resource and leaves no
trace in `GroupCommitter`'s state. The caller that already holds a permit
is entirely unaffected; this is a purely local, immediate rejection of
the *new* arrival.

## 5. Shutdown failure semantics

Once `shutdown()` has been called:

- `append()` returns `Aborted` immediately for every subsequent call.
- `await_durable()` returns `Aborted` immediately for every subsequent
  call whose `seq` is not already durable (an already-durable `seq`
  still correctly resolves `Ok` — shutdown does not retroactively
  un-durable anything).
- A caller already blocked in `await_durable` when `shutdown()` runs is
  woken and observes `Aborted` on its next loop iteration rather than
  its full timeout.
- `shutdown()` itself cannot hang: it waits at most `SHUTDOWN_DRAIN_
  BOUND` for an in-flight leader to finish, then returns a `ShutdownReport`
  regardless of whether that batch actually finished in time.

## 6. Rotation failure

`GroupCommitter::rotate()` returns whatever `FileWal::rotate()` returns,
unmodified — Phase 1 adds no new rotation-failure semantics, because none
are needed (see `PHASE1_ARCHITECTURE.md` §4: mid-batch rotation is
linearized against the leader's snapshot by construction, not by
additional error-handling). A `rotate()` failure leaves `FileWal`'s
existing, pre-Phase-1 atomicity guarantee intact — the new segment file
either doesn't exist or is fully valid; `self` is left exactly as it was
before the call, per `FileWal::rotate`'s own pre-existing contract.

## 7. Unimplementable failure scenarios, named explicitly

Two scenarios this failure model does *not* cover, because the mechanism
to reach them does not exist in this codebase:

- **Torn writes reaching `GroupCommitter`'s own code.** `GroupCommitter`
  never parses WAL bytes; every torn-write/corruption classification is
  `FileWal`'s pre-existing, frozen recovery logic. Nothing Phase 1 adds
  changes what counts as a torn write or how it's detected.
- **A read-path failure.** No read path exists in this repository (see
  `PHASE1_ARCHITECTURE.md` §8) — there is no failure mode to document for
  something that isn't implemented.
