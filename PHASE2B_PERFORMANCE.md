# RubiXDB Phase 2B — Performance

Numbers only, cross-referencing `PHASE2B_FINAL_TEST_RESULTS.md` (the
authoritative source — this file restates the shape of the results for
readability) and `PHASE2B_ADR.md` (why).

## 1. The core relationship every Phase 2B architecture shares

All three architectures succeed for the same underlying reason: each
exposes essentially the **entire logical writer population** to every
batch, closing the `durable_ops_per_sync` gap `FINAL_WAL_ANALYSIS.md`
§8 identified as the actual measured bottleneck in both Phase 1's
direct-thread model (93.4 / 693.5 records/sync at 100/1,000 writers)
and Phase 2's rejected worker pool (capped at ≈`worker_count`):

| Architecture, best config | avg_batch_records, 100w | avg_batch_records, 1000w |
|---|---:|---:|
| Phase 1 direct | 93.4 | 693.5 |
| Phase 2 `WriteWorkerPool` (rejected) | capped at `worker_count` | capped at `worker_count` |
| A, `worker_count=1` | 99.4–99.8 | 993–994 |
| A, `worker_count=2` (coordinated) | 99.4–99.8 | 994.04 |
| B (coordinator) | 99.3–99.7 | 988–994 |
| C (sharded, 8 shards) | 99.3–99.6 | 989–993 |

Every Phase 2B architecture reaches essentially **99% batch-formation
efficiency** at both required levels — the ceiling isn't full 100%
because a small fraction of requests genuinely arrive *after* a batch
has already started draining, not because of any structural limitation.

## 2. Why more workers hurt Approach A without coordination, and why that's fixable

Attempt A1's own sweep (`PHASE2B_FINAL_TEST_RESULTS.md` §6) showed
throughput *falling* as `worker_count` rose past 1 (85,158 → 45,555–
65,800 ops/sec across `worker_count` 2–64) — the exact opposite of
Phase 1's own direct-thread finding that more concurrent demand helps
(`FINAL_WAL_ANALYSIS.md` §7.2). The mechanism: with `N` workers each
independently free to drain whenever the queue is non-empty, a burst of
arrivals gets fragmented across however many workers happen to wake up
before any one of them finishes its own batch — the more workers, the
finer the fragmentation, the smaller each resulting `fsync`'s payload,
the more total syncs needed for the same volume of work. Attempt A2's
single-active-drain-leader coordination (`draining_active` + `DrainLeaderGuard`)
directly removes this mechanism by construction: at most one worker is
ever actively draining, so fragmentation cannot occur regardless of how
many *standby* workers exist. The result (`worker_count=2` recovering to
`worker_count=1`'s throughput, `PHASE2B_FINAL_TEST_RESULTS.md` §6)
confirms the mechanism, not just the symptom.

## 3. Why Approach C (sharding) didn't help

Sharding the *ingress queue* only pays off if that queue's own lock is a
measured bottleneck. It wasn't, in either A or B — both already reached
~99% batch-formation efficiency with a single shared queue, meaning
producers were never meaningfully blocked waiting for that lock relative
to the ~5ms `fsync` cost dominating every batch cycle regardless of
architecture. Sharding therefore removed a contention point that was
not, in fact, contended — consistent with (not a new finding beyond)
`FINAL_WAL_ANALYSIS.md`'s own §11 conclusion that lock contention is a
secondary, not primary, factor on this hardware.

## 4. Latency shape

None of the three architectures showed a qualitatively different p50/
p95/p99 *shape* at comparable, near-target-throughput configurations —
all are governed by the same `fsync`-dominated batch cycle
(`PHASE2B_FINAL_TEST_RESULTS.md` §12). The meaningful latency
differences observed in this cycle were between *configurations within
an architecture* (e.g., Approach A's `worker_count=1` sweep at 1,000
writers: p50 ≈9.6–10.8ms across worker counts 1–64, `PHASE2_TEST_
RESULTS.md`-style sweep data in the raw benchmark logs), not between
architectures at their respective best configurations.

## 5. Hardware/software attribution (unchanged conclusion, reconfirmed)

`fsync` latency (4.0–6.1ms, `FINAL_WAL_ANALYSIS.md`/`FINAL_WAL_TEST.md`)
remains the dominant, stable cost underneath every Phase 2B
architecture — `mean_processing` per batch stays in the same few-
millisecond neighborhood regardless of which architecture or
configuration is measured. Phase 2B's entire contribution is closing
the *software* batch-visibility gap Phase 1/Phase 2 evidence identified;
it does not and cannot change the hardware floor. The margin achieved
above target (+16.7% at 100w, +17.0% at 1,000w, Approach B) reflects how
much headroom existed between "target" and "what 99%-efficient batching
against this hardware's `fsync` floor can deliver" — not a claim that
further software work could push materially higher without also
addressing that floor (see `FINAL_WAL_TEST.md` §24's own hardware
counterfactual analysis, unchanged and still applicable here).
