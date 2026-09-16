# RubiXDB Phase 2B — Failure Model

Companion to `PHASE1_FAILURE_MODEL.md`/`PHASE2_FAILURE_MODEL.md` —
every WAL/`GroupCommitter` failure mode those files document still
applies unchanged underneath every Phase 2B architecture, since none of
Approaches A/B/C touch `GroupCommitter`'s own durability logic. This
file covers only what's new or newly-*discovered* (not newly-introduced)
in this cycle.

## 1. Failure table (common to A, B, C unless noted)

| Failure | Detection | Response | Caller-visible outcome |
|---|---|---|---|
| A drain-leader/coordinator panics mid-batch | The panic unwinds past every appended entry's `CompletionGuard` (constructed *before* the shared `await_durable` call — see §2 for why this ordering matters) | Every entry in that batch resolves to `Err(EngineError::Aborted)` | Callers' `wait()`/`wait_timeout()` return `Err` within microseconds, never hang |
| Every worker/the coordinator dies without a requested shutdown | `workers_alive`/`coordinator_alive` reaches 0 outside `PoolState::Draining` | Pool → `PoolState::Failed`; anything still queued is drained and failed with `EngineError::WalUnavailable` | `submit()` rejects all further work immediately and cleanly |
| `GroupCommitter::append` fails partway through a batch | Returned directly, no retry | That entry fails immediately; every entry *after* it in the same batch (never attempted) also fails immediately, with no wait; entries *before* it (already appended, real `seq`) are awaited normally | No entry is ever falsely marked durable; nothing is retried in a way that could duplicate a record |
| `await_durable` times out for a batch | `EngineError::Timeout` | Retried — **only the wait, never the append** — up to `await_retry_budget`; the whole batch shares one outcome | Same reasoning and precedent as `PHASE2_ADR.md` ADR-P2-4: retrying a pure wait cannot duplicate a record, since every entry's `append` already completed before the retry loop begins |
| Queue/shard full | Bounded wait in `submit()` | `EngineError::Timeout` after `submission_timeout` | Never silently drops a write, never blocks unboundedly |
| `shutdown()` called with requests queued | `PoolState::Draining` | New submissions rejected; already-queued requests still processed normally against the still-live `GroupCommitter` (finalized only once every worker/the coordinator has fully exited) | No queued request is lost |

## 2. Why every architecture's `process_batch` builds every `CompletionGuard` before the shared `await_durable` call

**Found as a real bug during Approach A's development** (`PHASE2B_
FINAL_TEST_RESULTS.md` §13, finding 1): the first version of `leader_
drain::process_batch` built each entry's `CompletionGuard` *after* the
one shared `await_durable` call, inside the loop that delivers results.
A panic during that call (exercised by the worker-panic fault-injection
test) therefore happened while **no guard existed yet** for any entry in
the batch — nothing was armed to fire a fallback completion, and every
caller in that batch hung. Diagnosed by the test itself hanging past a
60-second timeout; fixed by constructing every entry's guard *before*
the `await_durable` call and keeping the whole `Vec<CompletionGuard>`
alive across it. Verified fixed: the same scenario now resolves in
under 1ms. Approaches B and C were written *after* this fix was found
and use the corrected ordering from the start.

## 3. A pre-existing Phase 1 limitation, newly and precisely diagnosed this cycle

**Status: FIXED in Phase 3** (`PHASE3_FAILURE_MODEL.md` §1–§3,
`LeaderFailureGuard` in `src/wal/group_commit.rs`, ADR-P3-1). The
description below is kept verbatim as the historical record of what was
found and diagnosed in Phase 2B — it no longer describes current
behavior. Post-Phase-3, a leader panic clears `leader_active` and
poisons the committer immediately (fail-fast), instead of leaving it
permanently `true`.

**Not a bug in any Phase 2B architecture — a characteristic of
`GroupCommitter` itself** (`src/wal/group_commit.rs`), inherited
unchanged by all three, at the time this document was written:

A leader/coordinator thread that panics **specifically while inside the
leader `fsync` call** (`GroupCommitter::do_leader_fsync`, reached via
`run_as_leader`) never reaches `finish_batch_ok`/`finish_batch_with_
error` — the code that resets `BatchState::leader_active` to `false` or
sets `poisoned`. The panic unwinds straight past both. `leader_active`
is left **permanently `true`**, with the `batch` mutex itself healthy
(not poisoned) — just its value is now wrong forever. Consequence:

- Every subsequent `await_durable` call, from **any** thread (including
  a surviving standby worker in Approach A, or a freshly-submitted
  request in Approach B/C), becomes a **follower forever** — no new
  leader can ever be elected — and eventually times out.
- Approach A's `worker_count > 1` redundancy **cannot rescue a request
  submitted after this specific failure**, because the thing that's
  broken (`GroupCommitter`'s own global `leader_active` flag) is shared
  process-wide state no pool-level worker has access to reset. This is
  the one scenario in which Approach A's redundancy provides no benefit
  over Approach B's lack of it — both architectures degrade to "every
  request now fails, safely and boundedly, until the `GroupCommitter`
  is reconstructed."
- `GroupCommitter::shutdown()` still behaves exactly as documented under
  this condition: it waits its own fixed `SHUTDOWN_DRAIN_BOUND` (5
  seconds) for `leader_active` to clear, then gives up and returns
  anyway. This — not a hang, not a new Phase 2B bug — is the source of
  the ~5-second teardown cost observed in the worker-panic tests for
  every architecture in this cycle.

**Verified safe despite being unrecoverable-without-reconstruction**:
a dedicated test (`leader_drain::tests::a_second_request_after_the_
leader_panics_still_fails_safely_not_permanently_blocked`) confirms a
request submitted *after* this failure still resolves with a clean
error within its own bounded retry budget — never a hang, never a false
acknowledgment, never data loss. The only thing this failure mode
removes is *self-healing*; every one of this project's Non-Negotiable
durability/boundedness invariants still holds.

**Recovery path (unchanged from Phase 1's own documented model,
`PHASE1_GROUP_COMMIT.md`)**: drop the affected pool and its underlying
`GroupCommitter`, construct fresh ones. No Phase 2B architecture invents
a different recovery story, and none was asked to — modifying
`GroupCommitter` itself was out of scope for this cycle absent a
documented architectural reason to do so, and none was found.

## 4. Approach-specific notes

- **Approach A**: with `worker_count > 1` (Attempt A2's coordination),
  an *ordinary* worker-thread death (not the §3 scenario above — any
  other panic, or in principle a supervised restart) is recoverable: a
  surviving standby simply becomes the new drain-leader on its next wake.
- **Approach B**: no standby exists by design — any coordinator death,
  ordinary or the §3 scenario, is unrecoverable without reconstructing
  the pool. Verified to fail *safely* either way
  (`coordinator_panicking_fails_safely_and_rejects_further_work`).
- **Approach C**: same single-coordinator profile as Approach B (sharding
  the *ingress* side does not add coordinator redundancy) — plus the
  §4-of-`PHASE2B_ARCHITECTURE_C.md` lost-wakeup race, closed with a
  documented bounded fallback rather than left unbounded.
