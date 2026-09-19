# Phase 3C — Clean-Machine 1,000-Writer Remeasurement

**Status: FINAL** (this document, standalone; additive evidence only —
does not modify `PHASE3C_TEST_RESULTS.md` or `PHASE5_TEST_RESULTS.md`,
both currently owned by a concurrent certification session working in
this same worktree)

**Purpose**: a second Claude Code session working this same
certification task, in parallel, measured 1,000-writer throughput
immediately after the two Phase 3C soak legs completed (8 continuous
hours of disk I/O) and got 66,535-94,075 ops/sec across four pipeline
configurations plus a 75,822 ops/sec soak mean — 15-25% below the
80,000 ops/sec hard target and below the 89,157-98,666 historical band.
That session correctly flagged the measurement as taken on a
non-idle machine and recommended a clean re-run before treating it as a
confirmed code regression. This document is that re-run.

## 1. Machine state at measurement time

| | |
|---|---|
| Date/time | 2026-09-19, starting ~08:35 IST |
| Time since soak completion | ~2h48m (soak ended 2026-09-19 05:46:47 IST) |
| System boot time | 2026-09-18 12:55:58 (unchanged — **no reboot performed**; see rationale below) |
| CPU load (3 samples, 2s apart, immediately before benchmarking) | 5%, 6%, 9% |
| Physical disk queue length | 0 |
| Stray `long_soak_test`/`cargo`/`rustc` processes | none |
| OS | Windows 10 Home, 10.0.19045 |
| CPU | Intel(R) Core(TM) i7-7700 @ 3.60GHz, 4 cores / 8 logical processors |
| RAM | 16,271 MB |
| Rust | rustc 1.98.1 (48a229cea 2026-09-01) |
| Cargo | cargo 1.98.1 (797e8a9bc 2026-08-05) |
| Repository commit | `8c90433edb21c5dcf518fe33adad1261bd96901e` (HEAD, unchanged throughout) |

**Reboot decision**: a reboot was considered (as the certification task
permits, "if necessary") but not performed. Rationale: this machine
runs two live Claude Code sessions and a reboot would kill both
mid-task; the idle-state evidence above (CPU 5-9%, disk queue 0, no
stray processes, ~2h48m quiet since the soak) already indicates a
genuinely idle machine without that disruption. This is a judgment
call, stated explicitly rather than silently made — if the results
below are later found not to hold up, a clean-reboot re-run remains
the next diagnostic step.

## 2. Benchmark commands

```
cargo run --release --example batch_coordinator_load_test -- 1000 1000     # WAL-only (WAL + Group Commit + Dedicated Batch Coordinator)
cargo run --release --example lsm_load_test -- 1000 1000                   # WAL + MemTable (flush never triggered)
cargo run --release --example lsm_flush_load_test -- 1000 1000 4194304     # WAL + MemTable + SSTable + Manifest (4 MiB memtable)
```

5 repetitions each, run back-to-back, no other workload concurrent.

## 3. Results

### WAL-only (`batch_coordinator_load_test`)

| Rep | ops/sec |
|---|---|
| 1 | 73,212 |
| 2 | 76,634 |
| 3 | 92,456 |
| 4 | 96,329 |
| 5 | 93,542 |

**Median: 92,456 ops/sec**

### WAL + MemTable (`lsm_load_test`)

| Rep | ops/sec |
|---|---|
| 1 | 85,180 |
| 2 | 70,000 |
| 3 | 82,491 |
| 4 | 85,165 |
| 5 | 82,383 |

**Median: 82,491 ops/sec**

### WAL + MemTable + SSTable + Manifest (`lsm_flush_load_test`, 4 MiB memtable)

| Rep | ops/sec |
|---|---|
| 1 | 70,034 |
| 2 | 58,436 |
| 3 | 87,002 |
| 4 | 93,587 |
| 5 | 90,615 |

**Median: 87,002 ops/sec**

## 4. Comparison against target and historical band

| Layer | Median | Hard target (≥80,000) | Historical band (89,157-98,666) |
|---|---|---|---|
| WAL-only | 92,456 | Above | **Within** |
| WAL + MemTable | 82,491 | Above | Below |
| Full pipeline | 87,002 | Above | Below |

## 5. Interpretation

- The WAL-only median is above the 80,000 hard target and within the
  historical range.
- The WAL + MemTable median is above the 80,000 hard target but below
  the historical range.
- The full-pipeline median is above the 80,000 hard target but below
  the historical range.
- Individual runs dip below 80,000 in every layer tested, including
  the architecturally-unchanged WAL-only layer (reps 1-2: 73,212 and
  76,634).
- **The earlier 75,822/66,535-94,075 ops/sec result (measured
  immediately post-soak) does not reproduce as a stable ceiling on a
  clean, idle machine.** All three layers now median above the
  80,000 hard target.
- **This does not constitute a confirmed code regression.** The
  earlier post-soak measurement is better explained by transient
  machine state (8 continuous hours of prior disk I/O) than by a
  defect in the write path.
- **The full pipeline is not being claimed as inside the historical
  band** — its median (87,002) sits below the 89,157-98,666 range,
  same as the WAL+MemTable layer's median (82,491).
- **Variance is not being claimed as solved.** Every layer, including
  WAL-only, shows a wide spread (WAL-only: 73,212-96,329; WAL+MemTable:
  70,000-85,180; full pipeline: 58,436-93,587) compared to the
  historical band's tighter ~9.6% spread. This is classified as an
  **open secondary environment/measurement characteristic** — no
  specific root cause (thermal, OS scheduler, background services,
  filesystem cache state) has been established. It is not attributed
  to a particular pipeline layer, since the unchanged WAL-only layer
  shows the same pattern.

## 6. What this does and does not resolve

**Resolved**: the specific question of whether the post-soak 75-76K
figure represents a stable, reproducible ceiling — it does not.

**Not resolved**:
- The root cause of the run-to-run variance seen at every layer.
- Whether the full-pipeline layer's median sitting below the
  historical band (while still clearing the hard target) reflects a
  real, small architectural cost from SSTable/Manifest integration, or
  is within this variance's own noise band — insufficient repetitions
  at this specific layer to distinguish the two with confidence.
- **The realistic Phase 5 multi-hour full-pipeline soak remains NOT
  RUN.** The completed 4h+4h Phase 3C soak (`examples/
  long_soak_test.rs`) exercises WAL + Group Commit + Dedicated Batch
  Coordinator only — it never constructs an `LsmEngine` and never
  exercises MemTable, freeze, SSTable flush, Manifest, checkpoint, or
  WAL purge under sustained multi-hour load. `PHASE5_TEST_PLAN.md` §8
  itself explicitly scoped Phase 5's own soak as bounded-duration, not
  multi-hour, so no prior phase has ever produced this evidence — it
  is a new requirement for this certification, not a re-run of an
  existing authoritative configuration.

## 7. Next step

Per the certification task's own sequencing: the realistic full-pipeline
multi-hour soak (exercising the complete WRITE → WAL → MemTable →
freeze → SSTable → Manifest → checkpoint → WAL purge path under
sustained load, with periodic real external-process crashes) is the
next required evidence before the deferred final regression suite,
final acceptance benchmarks, and final certification.
