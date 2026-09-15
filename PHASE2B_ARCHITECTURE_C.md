# Phase 2B — Approach C: Sharded / Per-Core Ingress With Batch Merge (Evaluated, No Improvement)

Design only. Implementation: `src/execution/sharded_ingress.rs`, whose
own module doc comment is authoritative if this summary ever disagrees
with it. See `PHASE2B_FINAL_TEST_RESULTS.md` §8 for the numbers and
`PHASE2B_ADR.md` for the decision record.

## 1. Scope of evaluation — read this before the design section

The operating brief's own Approach C section is explicitly conditional:
*"If A and B fail to reach the required performance, evaluate a third
architecture..."* Both Approach A and Approach B **met** both throughput
targets comfortably (`PHASE2B_FINAL_TEST_RESULTS.md` §6/§7) using a
single shared `Mutex<VecDeque>` ingress queue, with no evidence in
either's own measurements that this single queue was a contention
point. This module was therefore built and measured **once** (Attempt
C1 only — not the full three-attempt cycle A and B each received)
purely to test the sharding hypothesis directly and complete the
required three-way comparison, not because prior evidence suggested it
was needed.

## 2. Hypothesis

A single global ingress queue could, in principle, become a contention
point under very high concurrent submission rates (many producer
threads all competing for the same lock on every `submit()` call).
Sharding the ingress side into `shard_count` independent queues, with
producers assigned round-robin, removes that specific contention point
while a single coordinator still drains across all shards into one
merged batch — preserving the one durability ordering boundary the
operating brief requires ("do not create independent WAL durability
domains merely to make the benchmark faster").

## 3. Architecture

```text
Writer Group A ---> Shard Queue 0 ---\
Writer Group B ---> Shard Queue 1 ----\
Writer Group C ---> Shard Queue 2 -----+--> Coordinator --> WAL --> Sync
Writer Group D ---> Shard Queue 3 ----/
```

`shard_count` independent `Mutex<ShardState>`s (producers only ever lock
their own assigned shard); one coordinator thread drains every shard in
round-robin order each cycle into one merged `Vec`, then appends and
syncs exactly as Approaches A/B do.

## 4. A real, documented engineering trade-off: the bounded-wait fallback

Splitting the ingress lock from the coordinator's own wait condition
introduces a genuine, narrow lost-wakeup race: a `submit()` to any shard
can notify the coordinator's `Condvar` in the window between the
coordinator observing every shard empty and actually parking on that
`Condvar`. Closing this race completely would require a shared lock
spanning both the per-shard push and the coordinator's own check —
exactly the contention this architecture exists to shard away. The
implemented fix is a generous (50ms), explicitly-documented bounded
fallback: notified immediately in the overwhelming common case, never
stuck longer than the bound even in the unlucky race window. This
satisfies "no unbounded blocking" without reintroducing a global
per-submission lock — a deliberate trade-off, not an oversight (see the
function's own doc comment in `sharded_ingress.rs` for the full
reasoning).

## 5. Ordering, durability, ownership

Identical to Approaches A/B's own sections of the same name — sharding
the *queue* changes nothing about how `seq` is assigned or how
durability is proven. Sequence order is, if anything, *further*
decoupled from submission order than in A/B (a later-submitted entry on
a shard visited earlier in a drain cycle can be appended, and thus
sequenced, before an earlier-submitted entry on a shard visited later)
— still fully consistent with this module's (and every Phase 2/2B
module's) standing "queue order is not sequence order" contract.

## 6. Result

No material improvement over Approach B at the one configuration tested
(`shard_count=8`, roughly this machine's logical core count) — see
`PHASE2B_FINAL_TEST_RESULTS.md` §8 for the full numbers. Directly
confirms the single shared queue was never the bottleneck in either A or
B. **Not adopted** — added complexity (per-shard capacity accounting,
round-robin assignment, the bounded-wait fallback above) with no
measured throughput benefit and a simpler, equally-fast alternative
(Approach B) already available.
