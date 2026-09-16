# RubiXDB Phase 3B — Architecture Decision Records

## ADR-P3B-1: Fix the completion-guard gap by building every guard up front, not by adding a second recovery mechanism

**Status**: Accepted, implemented.

**Context**: Wiring up `CoordinatorFaultPoint::AfterDrain` surfaced a
real gap (`PHASE3B_FAILURE_MODEL.md` §4): entries dequeued from the
shared queue had no panic-safety net until the append loop individually
reached them.

**Decision**: Build every entry's `CompletionGuard` as `process_batch`'s
first action, before any other work, iterating `batch` by reference
(not by value) so the guards can borrow from it for the whole function.

**Alternatives considered**:
1. **Wrap the whole `process_batch` call in `catch_unwind`, deliver a
   generic failure to every entry in `batch` from the caller
   (`coordinator_loop`) on `Err`.** Rejected: `catch_unwind` requires
   `UnwindSafe`, `QueueEntry`'s `Arc<CompletionSlot>` fields make this
   awkward without `AssertUnwindSafe` (which quietly signs off on
   potentially-inconsistent state after a caught panic — a worse
   correctness posture than just not catching it), and it would still
   let the coordinator thread itself survive a bug that should probably
   kill it (matching this architecture's own deliberate "no standby,
   fail visibly" trade-off, `PHASE2B_ARCHITECTURE_B.md`) rather than
   propagate as this project's established pattern already does
   everywhere else.
2. **Give `QueueEntry` its own `Drop` impl that completes its slot with
   a fallback error if not already resolved.** Rejected: this duplicates
   `CompletionGuard`'s exact existing responsibility in a second place,
   and would need its own "was this already resolved" flag — more
   surface for the same guarantee `CompletionGuard` already provides
   for every other completion path in this codebase (`write_pool.rs`,
   `leader_drain.rs`, `sharded_ingress.rs` all use it identically).
3. **Build guards up front** (chosen): reuses the exact mechanism
   already proven correct everywhere else in `execution::*`, requires
   restructuring `process_batch` to iterate by reference instead of
   consuming `batch` by value (a small, local, well-understood change),
   and closes the gap for *every* fault point in the function
   simultaneously, not just the one that happened to find it.

**Consequences**: `process_batch`'s control flow changed from
`Vec<(QueueEntry, WalPosition)>` (owning both the entry and its
position) to `Vec<(usize, WalPosition)>` (index into the still-owned
`batch`) plus a parallel `Vec<Option<CompletionGuard>>` — slightly more
bookkeeping, in exchange for a genuinely stronger safety property. No
behavioral change to the happy path (verified: all pre-existing tests
pass unchanged) — only the panic-path guarantee strengthened.

## ADR-P3B-2: `queued_bytes` accounting hardened to saturating arithmetic across all four `execution::*` architectures, not just the production one

**Status**: Accepted, implemented.

**Context**: Operating brief §9 requires proven-safe size accounting;
auditing `batch_coordinator.rs`'s `guard.queued_bytes += approx_bytes`
found it used raw addition while the admission check right next to it
(`guard.queued_bytes.saturating_add(approx_bytes) <= max_queued_bytes`)
already used saturating arithmetic — an inconsistency, not currently
exploitable (the admission check already bounds accepted totals far
below `usize::MAX` before the raw add ever runs), but exactly the class
of thing this project's own prior security audit (`wal_test.md` §3.7)
already flagged and fixed once, for the same reason: consistency on
every corruption-adjacent value, not just the ones currently reachable.

**Decision**: Fix it, and fix the identical pattern in `leader_drain.rs`,
`sharded_ingress.rs`, and `write_pool.rs` (all four `execution::*`
modules share this exact bookkeeping shape) in the same pass, rather
than fixing only the production architecture and leaving the other
three (still in the tree as tested, documented negative results per
`PHASE2B_ADR.md`) inconsistent with the project's own stated arithmetic
discipline.

**Alternatives considered**: fix only `batch_coordinator.rs` (the
recommended production architecture) and leave the others as-is, since
they are not the recommended default. Rejected: the other three are
still shipped, tested, and could be selected by a caller reading this
project's own documentation of the trade-offs (`PHASE2B_ADR.md`'s
"Approach A at `worker_count=2`" recommendation for redundancy-
requiring deployments) — leaving them with a known-inconsistent pattern
serves no one, and the fix is small and mechanical.

**Consequences**: No behavior change (both old and new code compute the
same result whenever no overflow would have occurred, which is every
reachable case given the existing admission check) — purely a defense-
in-depth hardening, verified by the full regression suite showing zero
behavioral difference.

## ADR-P3B-3: Observability — audit and targeted additions, not a from-scratch metrics subsystem

**Status**: Accepted (partial completion, explicitly not claimed as
complete).

**Context**: Operating brief §15–§19 asks for a full production metrics
layer (20+ named counters, explicit low-contention design, a metrics-
on/off performance comparison) and production logging audit.

**Decision**: Rather than build an entire new metrics subsystem under
this session's time constraints — risking exactly the "declare
production-ready without evidence" outcome the brief explicitly forbids
— audit the existing `GroupCommitStats`/`BatchCoordinatorStats` against
the brief's full list, add the handful of genuinely easy, safe,
zero-new-contention gaps (`queue_capacity`, `queued_bytes_capacity`,
`highest_sequence`, `segment_rotations`), and record the rest as an
explicit, itemized gap in `PHASE3B_TEST_RESULTS.md` rather than
fabricating partial coverage as complete.

**Why not attempt the rest anyway, at lower quality?** The brief's own
§16 requires measuring whether the observability layer changes
performance materially — a claim that itself requires the metrics
layer to exist, be wired into the hot path, and be benchmarked on/off,
which is a multi-hour exercise of its own (Phase 1's `batch_timing`
module's own revision history in `src/wal/group_commit.rs` is a direct,
in-repo demonstration of how easy it is to get this wrong on the first
attempt — a naive per-write shared-atomic design collapsed M1.3
throughput by ~85% the first time this codebase tried it). Shipping an
unmeasured, possibly-contended metrics layer to satisfy a checklist
item would risk violating the durability-adjacent "no unbounded/
speculative complexity" and "every optimization must be measurement-
driven" rules more than honestly deferring it does.

**Consequences**: `examples/soak_test.rs`'s own latency-sampling design
(per-thread local slots, periodic aggregation) stands as a validated
reference implementation for the next session's actual metrics-layer
work — not thrown away, reusable.

## ADR-P3B-4: Soak testing run at a bounded duration, not multi-hour, with the shortfall recorded rather than hidden

**Status**: Accepted (explicit, documented gap).

**Context**: Operating brief §10 asks for a "meaningful multi-hour
duration."

**Decision**: Ran `examples/soak_test.rs` for a bounded duration (see
`PHASE3B_TEST_RESULTS.md` for the exact figure) instead — long enough to
show a real trend (RSS, throughput, latency over many sample windows),
short enough to fit this session's interactive constraints.

**Why not simulate or extrapolate a multi-hour result?** Doing so would
be exactly the "fabricate test results" / "convert unfinished work into
PASS" outcome operating brief §28/§29 explicitly forbids. A shorter,
honestly-labeled run that shows no adverse trend is real evidence of
"no problem observed in this window," not evidence of "no problem
exists over multiple hours" — the two are recorded as distinct claims
in `PHASE3B_TEST_RESULTS.md`, not conflated.

**Consequences**: The final Phase 3B engineering decision (`PHASE3B_
TEST_RESULTS.md` §9) accounts for this explicitly as an open item, not
a silently-accepted risk.
