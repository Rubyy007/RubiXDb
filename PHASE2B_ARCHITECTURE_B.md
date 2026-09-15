# Phase 2B — Approach B: Dedicated Batch Coordinator (WINNER)

Design only. Implementation: `src/execution/batch_coordinator.rs`, whose
own module doc comment is authoritative if this summary ever disagrees
with it. See `PHASE2B_FINAL_TEST_RESULTS.md` for the numbers that made
this the winning architecture and `PHASE2B_ADR.md` for the full decision
record.

## 1. Hypothesis

Approach A tests "let whichever worker is active drain everything," with
`N` *interchangeable* workers coordinated at runtime (a `draining_active`
flag negotiated on every drain cycle). Approach B tests a structurally
different question: **is that runtime negotiation even necessary, or is
a design with exactly one coordinator thread — fixed at construction
time — simpler and at least as fast?** This directly matches the
operating brief's own framing of Approach B: separate *request
scheduling* (many producer threads, a queue) from *WAL leader execution*
(one coordinator) as two structurally distinct roles, not one role a
variable number of workers can all play.

## 2. Architecture

```text
Logical Writers (10s/100s/1,000+)
      |  submit(op) -> Completion
      v
Bounded ingress queue (Mutex<VecDeque> + 2 Condvars)
      |  drained ONLY by the one coordinator thread
      v
The one Batch Coordinator thread
      |  drain everything queued, append each (fast), ONE
      |  await_durable() for the whole batch, complete all
      v
Arc<GroupCommitter> --------> WAL
```

Producers/callers **never** call into `GroupCommitter` themselves and
never compete to become a leader — there is exactly one thread in the
whole system that ever does either. `QueueState`/`CoordinatorAliveGuard`
are correspondingly simpler than Approach A's equivalents: "the
coordinator is gone" and "every worker is gone" are the same event (no
multi-worker "was this the last one?" branch is needed).

## 3. Ordering, durability, ownership

Identical to Approach A's own sections of the same name — `GroupCommitter`
remains the sole authority for `seq` assignment and durability; queue
order is not sequence order; one copy per request at `submit()`.

## 4. The trade-off this design makes explicitly

**No redundancy, by construction.** There is no standby thread — if the
one coordinator panics, the pool transitions to `PoolState::Failed`
immediately and stays there; a fresh `BatchCoordinatorPool` (and, per
`PHASE2B_FAILURE_MODEL.md`'s own finding, likely a fresh `GroupCommitter`
too, given the `leader_active`-stuck-forever limitation both Approaches
A and B inherit unchanged from Phase 1) is the only recovery path. This
is the same single-point-of-failure profile as Approach A's own
`worker_count=1` configuration — Approach B just reaches it with less
code, since there was never a multi-worker case to coordinate away.
`coordinator_panicking_fails_safely_and_rejects_further_work` locks in
that this failure mode is still *safe* (bounded, clean, predictable
rejection of further work) even though it is not self-healing.

## 5. Why this won (summary — full reasoning in `PHASE2B_FINAL_TEST_RESULTS.md` §11)

Tied with every other Phase 2B architecture on correctness, durability,
and safety-under-failure. Won outright on 100-writer throughput (best
median of any architecture measured, including Approach A). Statistically
tied with Approach A's best (redundancy-free) configuration at 1,000
writers. Simplest code of any architecture evaluated — no worker-election
machinery, no per-shard bookkeeping. The operating brief's own priority
order ("complexity sixth" — the last tiebreaker, but a real one) favors
this design over anything structurally more complex once the higher-
priority criteria are already tied.

## 6. When *not* to choose this architecture

If hot-standby redundancy against a coordinator-thread panic is a hard
deployment requirement, Approach A at `worker_count=2` is the
recommended alternative instead — at the honestly-recorded cost of a
small 100-writer throughput margin (`PHASE2B_FINAL_TEST_RESULTS.md`
§6/§11). This is a call for the deploying team to make with the evidence
in hand, not one this document makes unilaterally.
