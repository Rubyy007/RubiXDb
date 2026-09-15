# RubiXDB Phase 2 — Performance

Numbers only, cross-referencing `PHASE2_TEST_RESULTS.md` (the
authoritative source — this file restates the shape of the result for
readability, not new data) and `PHASE2_ADR.md` (why).

## 1. The core relationship

At both logical-writer-count levels tested, `avg_batch_records` tracks
`worker_count`, not the logical writer count:

```text
1,000 writers:  worker_count ->  1     2     4     8     16    32    64    1000
                avg_batch    ->  1.00  2.00  3.99  7.97  15.92 31.75 62.89 442.48
                ops/sec      ->  245   283   539   1023  1951  3701  7603  43705

100 writers:    worker_count ->  8     32    100
                avg_batch    ->  7.99  31.55 96.15
                ops/sec      ->  996   3868  11060
```

Phase 1 direct-thread baseline, same session, same commit: **100
writers → 10,093 ops/sec median (93.4 records/sync); 1,000 writers →
60,726 ops/sec median (693.5 records/sync)** (`PHASE2_TEST_RESULTS.md`
§6, reusing `FINAL_WAL_ANALYSIS.md` §8's records/sync figures for the
same commit's direct-thread architecture).

## 2. Reading the curve

- Every `worker_count` below the logical writer count caps
  `avg_batch_records` at (approximately) `worker_count` — batches simply
  cannot include requests still sitting in the pool's own queue, however
  many are waiting there.
- `mean_processing` (dequeue-to-completion) stays flat at ~7–8.6ms
  across `worker_count` 2 through 64 at 1,000 writers — this machine's
  already-established `fsync`-dominated batch-cycle cost
  (`FINAL_WAL_ANALYSIS.md`/`FINAL_WAL_TEST.md`: 4.0–6.1ms `fsync` alone).
  Throughput scales because batch size scales, not because each batch
  gets cheaper.
- At `worker_count = writer_count` (the pool's best-case configuration,
  where it stops throttling batch formation at all), 100-writer
  throughput matches Phase 1 within this machine's documented noise
  band (+9.6%, not a confirmed gain), but 1,000-writer throughput is
  **28.0% lower** than Phase 1's own direct-thread number — the pool's
  own queue/`Completion`/allocation overhead, paid on top of
  `GroupCommitter`'s unchanged cost, with nothing offsetting it once the
  batch-throttling effect is eliminated.

## 3. What this rules out, and what it doesn't

**Rules out**: this specific architecture (one shared queue, N workers
each independently calling `GroupCommitter::append`/`await_durable`) as
a throughput improvement over Phase 1's direct-thread model, at every
configuration tested or reasonably extrapolatable from the data — see
`PHASE2_TEST_RESULTS.md` §15 for the full decision.

**Does not rule out**: a differently-shaped worker-pool design could
behave differently — e.g. one where a worker, once it becomes the
`GroupCommitter` leader, actively pulls *additional* already-queued
requests into its own batch before calling `append`/`fsync` (rather than
one request per worker per cycle, the shape actually implemented and
measured here). That is a genuinely different hypothesis from the one
this cycle tested, not a parameter tweak of it, and was not
implemented or measured — named explicitly as a candidate for a future
cycle in `PHASE2_ADR.md`, not silently assumed to fail.

## 4. Hardware/software attribution (unchanged from Phase 1)

Nothing in this cycle's measurements changes `FINAL_WAL_TEST.md` §17's
attribution table — the dominant factor remains storage `fsync` latency,
confirmed again here by `mean_processing`'s flat ~7–8.6ms floor
regardless of worker count. Phase 2's own contribution to the picture is
purely architectural: it demonstrates that the *coordination* layer in
front of `GroupCommitter` can make things measurably worse (by
constraining batch formation) without a corresponding hypothesis about
how it would make things better ever being confirmed.
