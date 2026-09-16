# RubiXDB Phase 3 — Architecture

## Scope and increment structure

Phase 3's operating brief (§1–§39) is large: Stage A (production
hardening of `GroupCommitter`/`BatchCoordinatorPool`/WAL/observability/
soak/crash testing) and Stage B (MemTable integration). Per this
project's own git discipline ("focused commits... every commit must
remain buildable") and its "do not invent behavior silently, do not
declare production-ready until evidence supports it" rules, Phase 3 is
being executed as a sequence of independently-verifiable increments
rather than one large, unreviewable change. This document is updated
each increment; it does not claim completion of sections not yet done
(see `PHASE3_FAILURE_MODEL.md` §5 for the current explicit scope
boundary).

**Increment 3A (this document's current state): the P0 leader-failure
fix.** The operating brief singles this out explicitly (§5: "This is
now a P0/P1 reliability issue for Phase 3... Design and implement a
proper recovery state machine") ahead of the rest of Stage A, so it was
built and verified first, on its own, before any other hardening work.

## Preserved architecture (unchanged, per the brief's own Non-Negotiable rules)

```text
Logical Writers
      |
      v
Dedicated Batch Coordinator   (execution::batch_coordinator — Phase 2B's winner, unchanged)
      |
      v
Group Commit                  (wal::group_commit::GroupCommitter — durability authority, unchanged
      |                        contract; this increment adds a panic-safety fix, not a redesign)
      v
Durable WAL                   (wal::FileWal — on-disk format, recovery contract: unchanged)
```

No change to:

- the WAL binary format (WAL Spec §2),
- `durable_through`'s semantics as the durability watermark,
- sequence allocation semantics (`FileWal::append` remains the sole
  assigner of `seq`),
- rotation semantics (`FileWal::rotate`),
- `FileWal`'s own recovery/torn-tail/corruption rules,
- the `BatchCoordinatorPool`/`GroupCommitter` split of responsibility
  (request scheduling vs. WAL leader execution) that Phase 2B
  established.

## What Increment 3A adds

One new type in `src/wal/group_commit.rs`, `LeaderFailureGuard`, plus a
new `PoisonReason` enum replacing the bare `io::ErrorKind` `BatchState`
used to store — both purely internal to `GroupCommitter`; no public API
signature changed except two additions:

- `GroupCommitter::is_poisoned(&self) -> bool` — new, observability-only
  accessor (poisoning was already externally observable via `await_
  durable`'s `Err`; this just makes it queryable without a live batch).
- The text of the `Err` a poisoned `GroupCommitter` returns now
  distinguishes an `fsync` failure from a leader panic (`PoisonReason`'s
  two variants), which it did not before (previously both would have
  been indistinguishable had the panic path been handled at all).

Full design rationale, the state machine, and why poisoning
(unconditionally, on any leader panic) is the correct fail-closed
choice: `PHASE3_FAILURE_MODEL.md` §2–§3.

**No new dependency.** **No `unsafe` code.** The fix is a single RAII
guard, following the exact pattern `execution::common::CompletionGuard`
already established in this codebase (Phase 2B) — not a new
abstraction.

## Why this is not a "redesign" of the batch coordinator

The operating brief explicitly warns against replacing the winning
Dedicated Batch Coordinator architecture "unless new evidence proves a
correctness or scalability problem." The leader-panic gap is exactly
such evidence — but the fix required is local to `GroupCommitter`'s own
already-existing poisoning mechanism (a panic is just another way a
batch can fail to confirm its outcome, handled the same way an `fsync`
`Err` already was), not a change to which architecture assigns
sequences, forms batches, or calls `fsync`. `BatchCoordinatorPool`
itself is untouched by this increment.

## Baseline discipline

Per operating brief §3, the Phase 2B winning configuration was
re-measured at the frozen starting commit (`7c808eb`, clean tree)
before any Phase 3 code was written — see `PHASE3_PERFORMANCE.md` §1
for the exact numbers. Phase 3 is compared against this freshly
re-measured baseline, not against `PHASE2B_FINAL_TEST_RESULTS.md`'s own
historical numbers directly (though they agree closely, confirming this
machine's own documented run-to-run variance is the only source of
difference).
