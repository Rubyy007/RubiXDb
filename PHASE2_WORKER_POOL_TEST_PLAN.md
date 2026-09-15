# RubiXDB Phase 2 — Write Worker Pool Test Plan

What was planned, what was actually executed, and what was deliberately
not executed once the critical experiment (operating brief §25)
produced a decisive result — see `PHASE2_TEST_RESULTS.md` for outcomes
and evidence, this file for the plan and its own accounting of what
changed and why.

## 1. Planned test layers

1. **Unit/correctness tests** (`src/execution/write_pool.rs`'s own
   `#[cfg(test)] mod tests`): basic submit/wait, concurrent submitters
   with exact-count/gap-free/recovery assertions, bounded-queue
   backpressure, shutdown semantics (idempotency, drain-before-reject),
   `into_inner`, `Drop` safety net, fault injection (fsync failure,
   worker panic).
2. **Regression gate**: full Phase 1 `cargo test`/`--release`/
   `--features test-util`/clippy/fmt, run before and after implementation
   and again before any acceptance decision.
3. **Phase 1 crash-consistency suite**: re-run unmodified against the
   Phase-2-extended tree to confirm zero interaction with `GroupCommitter`/
   `FileWal`'s own crash safety.
4. **Baseline measurement**: Phase 1 direct-thread throughput, same
   session, same commit, before any worker-pool benchmark — the
   comparison basis (operating brief §20).
5. **Critical experiment**: worker-count sweep (1/2/4/8/16/32/64, plus a
   parity point at `worker_count = writer_count`) at both 100 and 1,000
   logical writers, measuring throughput, `avg_batch_records`, queue
   wait, and processing time.
6. **Comparison and decision**: Phase 1 vs. Phase 2 at parity and at the
   full sweep, against the unchanged Phase 1 throughput targets.
7. *(Planned, contingent on the critical experiment's outcome)*:
   long-duration stability, resource-exhaustion stress, property-based
   randomized testing, a dedicated worker-pool `AbortPoint` set for
   queue submission/dequeue/completion boundaries.

## 2. What was actually executed

Layers 1–6 in full — see `PHASE2_TEST_RESULTS.md` §4–§10 for the
complete, evidenced results. Layer 7 was **not** executed, and this plan
records that as a deliberate scope decision, not an omission:

Layer 5's critical experiment produced an unambiguous, mechanistically-
explained result (`PHASE2_TEST_RESULTS.md` §7: `avg_batch_records`
tracks `worker_count` almost exactly at every point tested, both writer-
count levels, both the coarse and the parity-point sweep) leading
directly to a **REJECT** decision (§15 of that file) before Layer 7's
work would have begun. The operating brief's own governing principle
(§40: "Measure → Design → Implement → Verify → Benchmark → Compare →
Keep or Revert") does not mandate exhaustive long-duration/property/
resource-exhaustion investment into an architecture that has already
been measured, decisively, not to achieve its stated goal at any tested
or extrapolatable configuration — spending further effort characterizing
*how* a rejected design degrades under sustained load, rare races, or
resource pressure would not have changed the KEEP/REJECT decision, and
was judged not the highest-value use of further measurement time. The
correctness-layer tests already executed (Layer 1, including two
targeted fault-injection tests) establish that the code is *safe* to
leave in the tree as a documented, available-but-not-recommended
artifact — which is a materially lower bar than "safe to run in
production," and is the bar this component actually needs to clear
given its Reject status.

If a future cycle revisits this architecture (e.g., a redesigned
sharded/per-core variant motivated by a *different* hypothesis than the
one tested here — see `PHASE2_ADR.md`'s own note on this), Layer 7's
full plan remains valid and should be executed against that new design
before any adoption decision, not skipped a second time.

## 3. Reproduction commands

All commands, exact configurations, and evidence file locations are
recorded in `PHASE2_TEST_RESULTS.md` — this file does not duplicate them
a second time, to avoid the two documents silently drifting apart on a
number.
