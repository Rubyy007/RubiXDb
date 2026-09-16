# RubiXDB Phase 3B — Performance

Companion to `PHASE3B_TEST_RESULTS.md` (the authoritative pass/fail
source) — this file collects Phase 3B's benchmark and soak numbers in
one place, mirroring `PHASE3_PERFORMANCE.md`'s role for Increment 3A.

## 1. Environment

Unchanged from every prior phase this session: Intel Core i7-7700 (4
physical / 8 logical cores), 16 GiB RAM, SATA SSD (`E:`), Windows 10
Home 10.0.19045, `rustc`/`cargo` 1.98.1. `TEMP`/`TMP` redirected to
`E:\RubiXDb\temp\rgc_bench`.

## 2. Baseline (frozen at commit `68d70ea`, before any Phase 3B change)

| Level | Runs (ops/sec) | Median |
|---|---|---|
| 100 writers | 17,582 · 18,151 · 17,229 | **17,582** |
| 1,000 writers | 92,987 · 92,183 · 92,671 | **92,671** |

## 3. Final, post-Phase-3B (after all hardening changes)

| Level | Runs (ops/sec) | Median |
|---|---|---|
| 100 writers | 17,621 · 15,800 · 16,133 | **16,133** |
| 1,000 writers | 90,309 · 91,512 · 91,208 | **91,208** |

**Regression-bound rule** (`PHASE3B_TEST_PLAN.md` §1, established
before this comparison was made): both medians fall inside the
historical noise band (100w: 13,700–18,500; 1000w: 90,700–97,600) and
are not reproducibly low across the 3 repetitions each → classified as
**normal machine noise, not a regression**. Both clear the absolute
floor (100w ≥15,000, margin +7.6%; 1000w ≥80,000, margin +14.0%).

**No measurable throughput regression from Phase 3B's hardening work.**
This is expected: every Phase 3B code change is either off the hot path
entirely (fault-injection hooks, `test`/`test-util`-gated) or adds
negligible cost to it (`saturating_add` vs. raw `+=` is the same cost at
the instruction level on this architecture; `queue_capacity`/
`queued_bytes_capacity` are pre-existing config values, not new
per-write work; `highest_sequence`/`segment_rotations` add one extra
cheap, memory-speed `wal` lock per `stats()` call — an
observability-only, caller-invoked path, never the write path itself).

## 4. Soak test throughput/latency (bounded duration — see `PHASE3B_TEST_RESULTS.md` §8 for the full account and the recovery-memory finding)

### 100 writers, 900s

| Sample | ops/sec | p50 (ms) | p95 (ms) | p99 (ms) | RSS (KB) |
|---|---|---|---|---|---|
| t=60s (start) | 17,170 | 5.419 | 7.466 | 14.575 | 7,808 |
| t=450s (mid) | 16,885 | 5.600 | 7.614 | 12.185 | 7,708 |
| t=900.8s (end) | 18,739 | 5.309 | 6.271 | 6.962 | 7,328 |

Mean over the full 900s: ≈17,217 ops/sec. Zero errors, zero timeouts,
zero backpressure rejections at any of the 16 samples.

### 1,000 writers, 900s

| Sample | ops/sec | p50 (ms) | p95 (ms) | p99 (ms) | RSS (KB) |
|---|---|---|---|---|---|
| t=60s (start) | 92,002 | 10.153 | 11.824 | 18.225 | 32,144 |
| t=450s (mid) | 96,131 | 10.036 | 11.548 | 17.944 | 32,344 |
| t=900.4s (end) | 96,706 | 10.071 | 11.458 | 11.965 | 30,016 |

Mean over the full 900s: ≈94,308 ops/sec. Zero errors, zero timeouts,
zero backpressure rejections at any of the 16 samples. `queue_depth`
non-zero (never above 792) at only 4 of 16 samples, never sustained or
growing.

**No throughput degradation, no latency drift, no RSS growth trend at
either writer level over the full run duration tested.**

## 5. Recovery timing

| Records recovered | Time | Rate |
|---|---|---|
| 15,495,498 (100w, 900s soak) | 72,965.9 ms | ≈212 Kelem/s |
| 8,506,743 (1000w, 90s supplementary soak) | 39,808.5 ms | ≈214 Kelem/s |
| 1,000,000 (final acceptance benchmark, per run) | not separately timed this increment | — |

Both large-scale recovery rates are consistent with each other and
with this project's own Phase 0 recovery-throughput benchmark
(~132–135 Kelem/s at 10,000-record scale, `PROGRESS.md`'s WAL
implementation entry) — recovery throughput does not degrade at this
larger scale; only the *memory* required to hold the fully-materialized
result scales with record count, which is the finding recorded in
`PHASE3B_TEST_RESULTS.md` §8, not a throughput problem.
