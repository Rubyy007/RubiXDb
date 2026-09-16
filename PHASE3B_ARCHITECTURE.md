# RubiXDB Phase 3B — Architecture

## Scope

Phase 3A closed the P0 leader-panic availability gap
(`LeaderFailureGuard`). Phase 3B's brief calls for completing Phase 3's
remaining production-hardening scope before MemTable integration begins:
the full `GroupCommit`/coordinator fault matrix, crash semantics per
fault point, coordinator-failure testing distinct from leader-failure
testing, shutdown hardening, resource-exhaustion testing, a large-payload
accounting/overflow audit, long-duration soak testing, periodic
forced-crash soak, recovery stress, rotation stress, an observability
layer, production logging, a security review, and final performance
re-verification — all before Stage B (MemTable) begins.

This is a large brief. Per this project's own established practice
(Phases 1/2/2B/3A were each executed as measured, independently-verified
increments, never as one unreviewable pass) and per this document's own
Section 28/29 instructions ("do not convert unfinished work into PASS"),
Phase 3B was executed as a sequence of concrete, testable increments
within a single session, each committed and regression-gated
separately. **Not every item in the 31-section brief was completed to
its fullest literal scope** (most notably: the soak duration is bounded,
not multi-hour; the observability layer is an audit plus targeted
additions, not a from-scratch metrics subsystem) — see `PHASE3B_TEST_
RESULTS.md` §8 for the explicit, itemized completion/gap list and §9 for
the final engineering decision.

## What Phase 3B adds, by area

### Coordinator fault-injection matrix (`src/execution/batch_coordinator.rs`)

`CoordinatorFaultPoint` — 7 deterministically injectable points in the
Dedicated Batch Coordinator's own batch-processing loop (before batch
formation, after drain, after append, before/after awaiting durability,
before completion, during shutdown), distinct from `GroupCommitter`'s
existing leader-`fsync`-only fault hook (Phase 1/3A), since none of
these 7 points are reachable through it. See `PHASE3B_FAILURE_MODEL.md`
§3.

**A real correctness gap found and fixed while wiring this up**:
`process_batch` previously only gave a dequeued entry its `CompletionGuard`
once individually reached by the append loop — a coordinator panic
between dequeue and that point would have dropped every entry in the
batch with callers hanging forever. Fixed by building every entry's
guard as the function's first action. See `PHASE3B_FAILURE_MODEL.md` §4.

### Overflow-safety hardening (`src/execution/{batch_coordinator,leader_drain,sharded_ingress,write_pool}.rs`)

`queued_bytes` accounting changed from raw `+=`/`.sum()` to
`saturating_add`/a saturating fold, across all four `execution::*`
architectures, matching the admission check beside it (already
saturating) and this project's own established precedent (`wal_test.md`
§3.7) for consistency on any corruption-adjacent arithmetic. Not
currently reachable (the admission check already bounds accepted totals
far below `usize::MAX`) but closed for defense in depth.

### Resource/rotation/shutdown test coverage

New tests (`src/execution/batch_coordinator.rs`, the production
architecture): large-payload byte accounting (deterministic-barrier-
based, not sleep-based), rapid submit/shutdown cycling, frequent
rotation under sustained concurrent load through the full production
path (extends the existing `tests/group_commit/rotation_mid_batch.rs`
M1.5 coverage, which only exercises `GroupCommitter` directly), and
shutdown called while the queue is actively populated.

### Observability

A grounded audit against the brief's full metric list (`PHASE3B_TEST_
RESULTS.md` §7), plus two small, safe, zero-new-contention additions:
`BatchCoordinatorStats::{queue_capacity, queued_bytes_capacity}` and
`GroupCommitStats::{highest_sequence, segment_rotations}`. The
remaining requested metrics (a distinct `writes_timed_out`,
`bytes_per_batch`, production-embedded p50/p95/p99 commit latency, a
unified `failure_count`, `recovery_count`) are explicitly **not**
added this increment — see the gap list.

`examples/soak_test.rs` doubles as a proof-of-concept for the
low-contention per-thread-local-counters-with-periodic-aggregation
design the brief's observability section asks for: each writer thread
owns one uncontended latency slot, drained periodically by a dedicated
sampler thread — the same shape Phase 1's own `batch_timing` module
already proved necessary at this project's scale (a shared atomic
touched by every writer measurably collapsed throughput the one time
this codebase tried that).

### Soak testing

`examples/soak_test.rs`, run against the production `BatchCoordinatorPool`
for a bounded (not multi-hour — flagged, not hidden) duration at 100 and
1,000 writers, sampling throughput/latency/queue/sync/RSS periodically
and reporting a start/mid/end drift comparison plus a final recovery
check. See `PHASE3B_TEST_RESULTS.md` for the actual run and its results.

## Preserved architecture (unchanged)

```text
Logical Writers -> Dedicated Batch Coordinator -> Group Commit -> Durable WAL
```

No change to the WAL binary format, `durable_through`'s semantics,
sequence allocation, rotation semantics, or `FileWal`'s recovery
contract. `BatchCoordinatorPool` remains the recommended production
architecture; Phase 3B's fixes and tests harden it, they do not replace
it — consistent with the operating brief's own explicit instruction not
to redesign the winning architecture absent a measured correctness or
reliability problem (the one problem actually found — the completion-
guard gap — was fixed locally, not by redesigning the coordinator).
