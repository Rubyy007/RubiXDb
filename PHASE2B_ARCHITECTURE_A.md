# Phase 2B — Approach A: Leader Queue Drain

Design only — no benchmark numbers here beyond what's needed for
context (see `PHASE2B_FINAL_TEST_RESULTS.md` for full data,
`PHASE2B_ADR.md` for the decision). Implementation:
`src/execution/leader_drain.rs`, whose own module doc comment is the
more detailed, line-referenced version of this document — read that
first if the two ever appear to disagree; this file is a summary, not
an independent source.

## 1. Hypothesis

`PHASE2_ADR.md` ADR-P2-5 (the rejected `WriteWorkerPool`'s own
retrospective) named a specific alternative: let the current batch
leader actively drain many already-queued requests into its own batch
before syncing, instead of requiring one dedicated worker per in-flight
request. This module tests that alternative directly.

## 2. Architecture

```text
Logical Writers (10s/100s/1,000+)
      |  submit(op) -> Completion
      v
Bounded queue (Mutex<VecDeque> + 2 Condvars)
      |  a worker drains the ENTIRE currently-available queue at once
      v
N worker threads, each capable of becoming a drain-leader
      |  append() every drained entry (microseconds each), then ONE
      |  await_durable() for the whole batch
      v
Arc<GroupCommitter> --------> WAL
```

The one structural change from the rejected `WriteWorkerPool`: a worker
pops **every** entry currently queued (`VecDeque::drain`, one lock-held
operation) instead of exactly one per loop iteration.

## 3. Ordering

`GroupCommitter::append` remains the sole assigner of `seq`. Queue
insertion order is **not** sequence order — this was already true of
Phase 1's own direct-thread model (many threads calling `append`
concurrently, in whatever order the scheduler runs them) and is
unchanged by adding a queue in front. No sequence-allocation logic
exists in this module.

## 4. Memory ownership

Identical shape to the rejected `write_pool`'s own table (`PHASE2_
WORKER_POOL_ARCHITECTURE.md` §3): one copy at `submit()` (caller bytes →
owned `WalOpOwned`), zero-copy re-borrow at processing time
(`WalOpOwned::as_wal_op`), moved into the WAL's own frame buffer inside
`GroupCommitter::append`.

## 5. Attempt A2: single-active-drain-leader coordination

Attempt A1 (`worker_count` sweep) found `worker_count > 1` *fragments*
throughput — every worker capable of concurrently draining splits one
potentially-large batch into several smaller ones, since batch size at
the moment of drain reflects only what's arrived since the *last* drain
by *any* worker. Attempt A2 added `QueueState::draining_active` (a
`bool`) plus `DrainLeaderGuard` (RAII, mirrors `CompletionGuard`'s own
panic-safety pattern): a worker must claim `draining_active` before
draining; if already claimed, it goes back to waiting rather than
draining a fragment. Extra workers become **hot standbys** — capable of
taking over draining if the active one exits (panics or is otherwise
gone) — rather than concurrent, throughput-costing drainers.

## 6. State machine

Identical shape to `write_pool`'s own (`RUNNING → DRAINING → STOPPED`,
or `→ FAILED` if every worker dies without a requested shutdown) — see
that module's documentation for the full transition diagram; unchanged
here beyond `workers_alive: usize` (any configured count, not just 0/1).

## 7. Failure semantics summary

See `PHASE2B_FAILURE_MODEL.md` for the full table. Headline: a single
worker panicking fails only its own in-flight batch and is picked up by
a surviving standby for the *next* batch — but (a genuine, documented
limitation, not a bug) cannot rescue a request that lands *after* the
panic if the panic happened inside `fsync`, because `GroupCommitter`'s
own `leader_active` flag is left permanently stuck by that specific
failure mode, independent of how many pool-level workers remain alive.

## 8. Trade-off summary (see `PHASE2B_FINAL_TEST_RESULTS.md` §6 for numbers)

- `worker_count=1`: best measured throughput at both required levels,
  clean margin above target — but a true single point of failure (no
  standby exists at all).
- `worker_count=2` (Attempt A2 coordination): near-identical 1,000-writer
  throughput, hot-standby redundancy against ordinary worker death — at
  a small, honestly-recorded cost to 100-writer margin (median lands
  just under target, within this machine's noise band).
