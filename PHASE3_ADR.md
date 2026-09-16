# RubiXDB Phase 3 — Architecture Decision Records

## ADR-P3-1: Fix the leader-panic gap with an RAII guard inside `GroupCommitter`, not a new coordinator-level mechanism

**Status**: Accepted, implemented (Increment 3A).

**Context**: `PHASE2B_FAILURE_MODEL.md` §3 found (but did not fix) that
a leader thread panicking inside `run_as_leader` leaves `BatchState::
leader_active` stuck `true` forever, degrading every future caller to a
repeated-timeout failure mode rather than a clean error. The Phase 3
operating brief marks this P0 and requires "a proper recovery state
machine," explicitly forbidding a *new* recovery mechanism duplicating
`FileWal`'s own recovery contract.

**Decision**: Add `LeaderFailureGuard`, an RAII guard armed the instant
a thread is elected leader and disarmed only after `run_as_leader`
returns normally. Its `Drop` — reached only via unwind — clears
`leader_active` and poisons the committer (`PoisonReason::
LeaderPanicked`), exactly mirroring what a returned `fsync` `Err`
already does (`finish_batch_with_error`). No new state machine class
was introduced beyond what `BatchState` already had (`leader_active`,
`poisoned`) — `poisoned` grew a reason enum instead of a bare `io::
ErrorKind`, which is additive, not structural.

**Why not a coordinator-level fix (`BatchCoordinatorPool` catching the
panic and resetting shared state)?** The bug is not specific to
Approach B — Approach A (`leader_drain`) and Approach C
(`sharded_ingress`) share the exact same `GroupCommitter` and are
equally exposed (`PHASE2B_FAILURE_MODEL.md` §3 already noted this).
Fixing it once, at the shared root (`GroupCommitter`), fixes it for
every current and future caller, including a direct Phase 1-style
caller with no pool at all. A coordinator-level fix would need to be
re-implemented three times (once per architecture) and would still
leave a bare `GroupCommitter` user exposed.

**Why poison unconditionally rather than attempt self-healing?** See
`PHASE3_FAILURE_MODEL.md` §3.1 for the full three-option analysis.
Summary: the panic's exact timing relative to the `fsync` syscall is
unknowable from inside the guard; poisoning is the only choice
consistent with this project's existing fail-closed precedent (an
`fsync` `Err` already poisons unconditionally) and with the brief's
explicit "do not advance durability incorrectly" rule.

**Alternatives considered and rejected**:
1. Catch the panic with `std::panic::catch_unwind` around the `fsync`
   call itself and convert it to a normal `Err`. Rejected: `catch_unwind`
   requires the closure to be `UnwindSafe`, and — more importantly —
   silently converting a panic (which, in production, usually indicates
   a real bug or a genuinely exceptional OS-level condition) into an
   ordinary `Err` return path hides the fact that something already
   crossed the panic boundary. The chosen design still lets the panic
   propagate to the leader's own caller (unchanged from pre-fix
   behavior) while *additionally* fixing the shared-state corruption —
   it does not suppress the panic.
2. A background "watchdog" thread that periodically checks whether
   `leader_active` has been `true` unreasonably long and force-clears
   it. Rejected: adds a new moving part (a thread, a timer, a race
   between the watchdog and a legitimately slow but still-alive leader)
   for a problem an RAII guard solves deterministically and for free —
   violates "do not make speculative performance/complexity additions
   without a measured need."
3. Make `leader_active` itself an RAII-guarded value from the start
   (return a guard object from the leader-election check, hold it for
   the batch's duration). Considered structurally cleaner, but would
   have required a larger refactor of `await_durable`'s control flow for
   no behavioral difference from the smaller, additive `LeaderFailureGuard`
   — rejected per "do not add abstractions beyond what the task
   requires."

**Consequences**:
- `GroupCommitter`'s public surface grows by one method
  (`is_poisoned`); no existing signature changed.
- A `GroupCommitter` that witnessed a leader panic must still be
  discarded and reconstructed (self-healing is not provided) — this is
  an accepted, documented limitation, not a regression: pre-fix, the
  same committer was *also* unusable (permanently degraded), just more
  slowly and less legibly.
- Two existing Phase 2B tests (`execution::leader_drain::tests::
  one_worker_panicking_...`/`::a_second_request_after_the_leader_
  panics_...`) had doc comments and implicit timing assumptions
  describing the *old*, now-fixed behavior. Updated in place (not
  deleted or silently left stale) with assertions locking in the new,
  faster behavior — see `PHASE3_TEST_RESULTS.md` §3.

## ADR-P3-2: Scope Increment 3A to the leader-panic fix alone, not the full Phase 3 brief in one pass

**Status**: Accepted.

**Context**: The Phase 3 operating brief spans production hardening
(§1–§19), soak/crash/resource-exhaustion testing (§13–§17), and a full
MemTable implementation (§20–§30) — independently large bodies of work.
This project's own git discipline calls for "focused commits" and "do
not mix unrelated changes"; its correctness culture calls for "do not
declare production-ready until the evidence supports it."

**Decision**: Execute Phase 3 as a sequence of increments, each fully
measured, tested, and documented before the next begins, starting with
the item the brief itself flags as P0 (§5: leader failure). Increment
3A is scoped to exactly that fix plus its required test coverage and
documentation — not a partial, unverified pass across every section of
the brief.

**Why not attempt everything in one pass?** A single change touching
`GroupCommitter`'s failure handling, a new metrics layer, soak-test
harnesses, and a from-scratch MemTable simultaneously would make it
impossible to attribute a regression (should one appear) to a specific
cause, and would risk exactly the "hide benchmark failures" / "declare
production-ready without evidence" outcomes the brief explicitly
forbids. Matches this project's own established practice across Phases
1, 2, and 2B, each of which was itself broken into measured,
independently-committed attempts.

**Consequences**: `PHASE3_FAILURE_MODEL.md` §5 and this document both
explicitly enumerate what remains — soak testing, the full fault-
injection matrix, resource-exhaustion testing, the metrics/logging
layer, and all of Stage B (MemTable) are not yet started. This is
recorded as a scope boundary, not silently omitted.
