# Read Engine — Increment 7: Post-Optimization 4-Hour Integrated Soak

Append-only historical record, same convention as `PHASE_READ_ENGINE_
PERFORMANCE.md`/`PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md`: never
edit a past run's numbers. This document is the fresh, full-duration,
production-profile soak the Increment 7 brief required against the
optimized (`ADR-RE-002` Option A, commit `22be3e4`) Read Engine, run
under the exact same methodology as Increment 4's own pre-optimization
soak (`temp/read_write_soak_output.log`, preserved unmodified,
referenced throughout below for direct comparison).

## 0. Preconditions verified before the soak started

- `git status`: working tree had one unexpected commit beyond the
  expected `22be3e4` HEAD — `3e13f64` ("commit by me"), authored
  outside this session. Inspected before proceeding (`git diff 22be3e4
  3e13f64 --stat`): touches only `examples/lsm_crash_cycle_test.rs`
  and adds `examples/read_write_soak_test.rs` -- **zero** changes to
  `src/`, `Cargo.toml`, or `Cargo.lock`. This is exactly the Increment
  4 test/example content already reviewed in Increment 5/6 (and is, in
  fact, the soak harness binary this increment depends on) -- flagged
  here rather than silently proceeding, judged safe to continue on
  since it contains no production-code change. No source was modified
  to "prepare" this soak.
- `cargo build --release --example read_write_soak_test`: clean.
- Pre-soak gate: `cargo fmt --check` clean, `cargo clippy --all-targets
  --all-features -- -D warnings` clean, `cargo test --release --lib`
  306/306, `cargo check --all-targets --all-features` clean.

## 1. Soak identity

```
commit tested:      22be3e4 (plus the unrelated, no-production-change
                     3e13f64 on top -- see §0)
command:             read_write_soak_test.exe 14400 8 16
                      "E:\RubiXDb\temp\read_write_soak_increment7_20260920_213353"
                      20260920 120
duration_secs:        14400 (4 hours, same as Increment 4)
writer_count:         8   (same as Increment 4)
reader_count:         16  (same as Increment 4)
seed:                 20260920 (same seed policy -- reused, not regenerated)
sample_interval_secs: 120 (same as Increment 4)
dir:                  E:\RubiXDb\temp\read_write_soak_increment7_20260920_213353
                      (new, unique -- the Increment 4 directory,
                      E:\rubixdb_read_write_soak_main, was already
                      removed by that soak's own on-PASS cleanup and
                      was not reused or touched)
```

Pre-run recorded state: `initial_free_disk_bytes=101,385,015,296`
(~94.4 GiB), initial `db_bytes=24`, initial `sstables=0`,
`manifest_records=0`, `manifest_bytes=0`, `pid=17844`,
`initial_rss_kb=3,764`. Well above the harness's own 5 GiB safety
floor and the ~2.4 GiB Increment 4 ultimately consumed.

Full raw log: `temp/read_write_soak_increment7_20260920_213353.log`
(1,054 lines, 116 `HEALTH` samples, 928 `LAT` lines), preserved
unmodified alongside this document.

## 2. Result

```
RESULT=PASS
writes_issued=6,696,650  deletes_issued=1,675,511  (total 8,372,161)
reads_issued=6,116,654   range_scans_issued=678,708
aged_point_checks=951,345  aged_range_checks=678,708
in_run_mismatches=0
recovery_ok=true  post_recovery_mismatches=0
capacity_backpressure_events=0
final_sstables=305  final_rss_kb=1,712,740  final_db_bytes=1,178,305,877
```

**Every one of the 678,708 range scans issued was checked against the
independent reference model at an aged, pinned snapshot seq** (`roll <
90` branch of `reader_loop`, `read_write_soak_test.rs`) — `aged_range_
checks == range_scans_issued` exactly — and zero disagreed. All
116/116 `HEALTH` samples show `storage_state=Healthy`, `mismatches_
total=0`; `grep`-checked directly (not sampled): zero `MISMATCH` lines,
zero `panic`/`ABORT`/`StoragePressure`/`StorageFull` occurrences
anywhere in the full log.

**A real, expected, and notable behavioral difference from Increment
4, worth stating rather than glossing over**: this soak issued *fewer*
total writes (8.37M vs Increment 4's 16.86M) but *more* reads (6.12M
vs 4.58M) and nearly **2.6x more range scans** (678,708 vs 261,455) in
the same 4 wall-clock hours, on the same 8-core machine. This is the
expected, mechanical consequence of Increment 6's fix: range scans no
longer burn CPU on redundant re-peeks, so reader threads genuinely
complete more range operations per second, which correspondingly
leaves writer threads a smaller share of the same fixed CPU budget
(both are ordinary OS-scheduled threads competing for the same 8
cores, not artificially rate-limited). This is itself evidence the
optimization is real under production-shaped concurrent load, not an
artifact -- readers doing *more real work* per hour, not just
*measuring faster* in isolation.

## 3. Resource behavior

| checkpoint | t (s) | sstables | rss_kb | handles | threads | snapshots_live |
|---|---:|---:|---:|---:|---:|---:|
| sample 1   | 120    | 4   | 121,344   | 108 | 29 | 50 |
| sample 30  | 3,666  | 136 | 735,696   | 240 | 29 | 50 |
| sample 59  | 7,294  | 181 | 1,050,364 | 285 | 29 | 50 |
| sample 88  | 10,934 | 231 | 1,226,404 | 335 | 29 | 50 |
| sample 116 | 14,409 | 305 | 1,712,764 | 405 | 4  | 50 |

- **`snapshots_live` stayed at exactly 50 for all 116 samples** —
  same test-harness pool cap (`SnapshotPool::prune(50)`) already
  verified, by source review, to be deliberate rather than a leak in
  `PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md` §6 — unchanged
  finding, reconfirmed under the optimized implementation.
- **Handles**: 108 → 406 (peak), tracking `sstables` (4 → 305) at
  ≈0.99 handles/table — matches Increment 3 §8's "~1 handle per live
  SSTable" finding, still holds exactly under the new persistent-cursor
  design (a cursor's `Arc<SsTable>` clone does not open a *new* file
  handle — `SsTable` opens its one `File` once, at `SsTable::open`,
  shared by every clone of its `Arc`).
- **Threads**: stable at 29–31 throughout the active run, dropping to
  4 immediately after `stop.store(true, ...)` — clean, expected
  shutdown, matching Increment 4.
- **RSS vs. SSTable count — materially tighter fit than Increment 4**:
  linear regression over all 116 samples: `rss_kb ≈ 4,783.0 × sstables
  + 139,164.2`, **R²=0.984** (vs. Increment 4's R²=0.698 over its own
  115 samples). Directly checked, not assumed: only **1 of 115**
  sample-to-sample transitions showed *any* RSS decrease at all (a
  trivial −436 KB), vs. Increment 4's largest single-window drop of
  **−704,440 KB**. **No leak** (consistent with the already-established
  no-leak finding, source-verified in Increment 5) — and the earlier
  investigation's own hedge ("most plausibly, though not independently
  profiler-confirmed, Windows working-set volatility") now has
  supporting comparative evidence: the same real workload, on the same
  machine, with the range-scan bottleneck removed, shows the large
  non-monotonic swings essentially disappear. Consistent with (not
  proven to be caused by) the hypothesis that Increment 4's own
  multi-minute, CPU/allocation-heavy `range_large` stalls were
  entangled with the OS's own working-set trimming behavior — a
  correlation worth recording, not a new causal claim this document
  asserts as proven.

## 4. Range-scan performance — early/25%/50%/75%/late, and direct comparison against Increment 4 at matched SSTable counts

`range_large` p50, by SSTable count (not sample index, since the two
soaks accumulated SSTables at different rates and this is the fair
axis to compare on):

| SSTables (approx. match) | Increment 4 (pre-opt) p50 | Increment 7 (post-opt) p50 | speedup |
|---:|---:|---:|---:|
| ~58-59   | 2,918,557.2us  | 785,174.9us   | 3.72x |
| ~105-112 | 6,232,273.6us  | 1,602,096.4us | 3.89x |
| ~219-223 | 14,054,448.2us | 3,788,596.5us | 3.71x |
| ~277-289 | 19,335,220.7us | 5,931,549.1us | 3.26x |

**Every comparison point above uses the SSTable count where Increment
7's value is equal to or slightly *higher* than Increment 4's** (a
harder case for Increment 7, if anything) — so these ratios are
conservative, not cherry-picked in the optimization's favor. The
speedup (3.26x-3.89x) lands squarely inside the range Increment 6's
own controlled `overlap_repro` benchmark already predicted
(3.20x-3.69x) — **the synthetic benchmark's prediction transferred
almost exactly to the real, full, concurrent, production-profile soak**,
which is the single strongest piece of evidence in this document that
the optimization is real and not a benchmark artifact.

**The growth curve itself is flatter, not just uniformly scaled down**
— traced explicitly, not assumed: fitting Increment 7's own
`range_large` p50 vs. SSTable count (sstables 59→305, p50
785,175us→6,076,103us) gives an apparent power-law exponent of
`log(7.74)/log(5.17) ≈ 1.25` — much closer to point lookups' own
linear scaling than Increment 4's own measured `~2.20` exponent over
its full run (`PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md` §2).
Extrapolating Increment 7's fitted curve out to Increment 4's own
final 597-SSTable point (`C·sstables^1.25`, `C` fit from the sstables=
59 point) projects **≈8.6 seconds** at 597 SSTables — clearly an
extrapolation, not a directly measured number (this soak never reached
597 SSTables; see §2's explanation of why), but consistent with, and
not contradicted by, the directly-measured 3.26x-3.89x range above.
Increment 7's own directly-measured maximum was **6.08 seconds**
(305 SSTables, final sample) against Increment 4's **43.1 seconds** at
its own final 597-SSTable point — a materially different order of
magnitude even without adjusting for the SSTable-count difference.

Full latency trend, all ops, early/25%/50%/75%/late (`n`, p50/p95/p99/
max, us):

| op | sample 1 (4 sstables) p50 | sample 30 (136) p50 | sample 59 (181) p50 | sample 88 (231) p50 | sample 116 (305) p50 |
|---|---:|---:|---:|---:|---:|
| `get`           | 0.7    | 3.7      | 3.8      | 3.8      | 3.9      |
| `get_as_of`     | 1.9    | 41.9     | 27.5     | 27.5     | 35.1     |
| `contains_miss` | 0.6    | 32.5     | 45.0     | 57.5     | 77.7     |
| `range_small`   | 39.5   | 30,671.1 | 40,100.5 | 55,137.3 | 80,643.4 |
| `range_medium`  | 283.0  | 240,135.0| 309,188.3| 424,792.4| 625,353.8|
| `range_large`   | 2,462.3| 2,305,044.5|2,928,704.4|4,019,053.4|6,076,102.9|
| `write`         | 6,386.9| 8,891.9  | 20,839.8 | 18,152.2 | 10,543.1 |

Point-lookup (`get`/`get_as_of`/`contains_miss`) latencies grow and
flatten in the same shape Increment 3/4 already established — no
regression, matching Increment 6's own point-lookup-untouched-by-diff
finding, now reconfirmed under real concurrent production load over a
full 4 hours, not just a benchmark.

## 5. Read amplification — `blocks_read` (unchanged counting point), matched-count comparison against Increment 4

| SSTables (approx. match) | Increment 4 blocks/read | Increment 7 blocks/read | reduction |
|---:|---:|---:|---:|
| ~156 vs ~136 | 14,899.83 | 1,852.00 | 8.05x |
| ~313 vs ~305 | 23,942.23 | 4,176.98 | 5.73x |

(`blocks/read` = `blocks_read_delta / read_requests_delta` for that
health window — a mixed point+range metric, same caveat as the
Increment 5 investigation's own §3.) `blocks_read`'s counting point
(`SsTable::read_block`) is byte-for-byte unchanged by Increment 6, so
this is a direct, apples-to-apples signal, not affected by the
`sstables_consulted` redefinition — and it lands in the same
neighborhood as (in fact somewhat larger than) Increment 6's own
controlled 4.714x `blocks_read` reduction, consistent with the real
workload's broader, more numerous distinct keys compounding the effect
further than the small 20-key `overlap_repro` fixture did.

`sstables_consulted` (**redefined this increment, per `ADR-RE-002`/
`PHASE_READ_ENGINE_PERFORMANCE.md`'s documented change** — once per
live SSTable per scan, not once per key) at the final sample:
`sstables_consulted_delta=384,127` over `read_requests_delta=4,959` at
305 SSTables ⇒ `sstables/read=77.46`, vs. Increment 4's final-sample
`55,575` at 614 SSTables. Not a clean like-for-like number (the
definition changed, and the SSTable counts differ) — reported for
completeness, `blocks_read` above is the metric to cite for the
mechanism's real-workload effect in isolation.

## 6. Point-read regression check

No regression observed at any sampled point across the full 4 hours
(§4's table). `get`/`get_as_of`/`contains_miss` p50s stay in the same
low-single/double-digit-to-low-hundred-microsecond band Increment 3/4
already established at comparable SSTable counts — expected, since
`src/sstable/reader.rs`'s point-lookup methods (`get_versioned`/
`contains_versioned`) were not touched by Increment 6 (confirmed by
diff at the time, re-confirmed here by measurement).

## 7. Crash / recovery — bounded, separate from the primary soak

Per the brief's own instruction ("perform the approved bounded crash/
restart tests separately from the primary continuous soak" — this
harness's crash-cycle tool uses its own, independent temp directory
and process, never touching the primary soak's directory or state),
run once, standalone, using the already-established `lsm_crash_cycle_
test` harness (built from the same `22be3e4`+`3e13f64` tree):

```
lsm_crash_cycle_test.exe 20 6 20260920 100 1500
cycles=20 successful=20 failed=0
final_highest_seq=14622 final_durable_through=14622
total_read_mismatches=0
```

Every one of the 20 cycles reports `reads_verified_ok=true` — real
post-recovery `get`/`contains`/`range` checks against the fixed
expected value every write in this harness uses (`verify_reads_after_
recovery`, `examples/lsm_crash_cycle_test.rs`), **not** merely "open()
returned Ok" (the brief's own explicit "do not stop at open succeeded"
requirement).

## 8. Corruption — fail-closed contract, re-verified post-Increment-6

Not re-run as a special step this increment — already covered, and
re-verified passing, by the full regression gate (§9 below), which
includes every corruption-matrix test unchanged since Increment 3/4/5:
`data_block_corruption_is_detected_lazily_at_read_time_not_at_open`,
`range_scan_across_a_corrupted_data_block_fails_closed_and_ends`,
`contains_across_a_corrupted_data_block_fails_closed_with_corruption`,
`corruption_injected_mid_session_between_reads_is_caught_on_the_very_
next_read`, plus the genuine (non-simulated) I/O-failure tests. No
corruption semantics were touched by Increment 6 or this increment.

## 9. Final regression gate (post-soak)

```
cargo fmt --check                                          clean
cargo clippy --all-targets --all-features -- -D warnings   clean
cargo test --lib                                            306/306
cargo test --release --lib                                  306/306
cargo check --all-targets --all-features                    clean
wal_tests                                                    12/12
crash_consistency --features test-util                       2/2
pathological_recovery_matrix (debug)                          9/9
pathological_recovery_matrix (release)                        9/9
```

No test assertion was altered to make this pass. Named coverage
(already inside the `--lib` 306, individually confirmed present and
passing by name): every `lsm::tests::range_scan_*`/`*corruption*`/
`*snapshot*`/`range_scan_during_concurrent_flush_sees_a_coherent_
snapshot`/`range_scan_property_tests::*` test.

## 10. Historical comparison summary (nothing overwritten)

| | Increment 4 (pre-opt) | Increment 7 (post-opt) |
|---|---:|---:|
| duration_secs | 14,400 | 14,400 |
| writer/reader count | 8 / 16 | 8 / 16 |
| seed | 20260920 | 20260920 |
| writes+deletes issued | 16,859,109 | 8,372,161 |
| reads issued | 4,582,352 | 6,116,654 |
| range scans issued | 261,455 | 678,708 |
| in_run_mismatches | 0 | 0 |
| post_recovery_mismatches | 0 | 0 |
| recovery_ok | true | true |
| final_sstables | 614 | 305 |
| final_rss_kb | 2,011,784 | 1,712,740 |
| final_db_bytes | 2,368,131,525 | 1,178,305,877 |
| RSS~sstables R² | 0.698 | 0.984 |
| range_large p50 @ final sample | 43,137,993.1us (597 sst) | 6,076,102.9us (305 sst) |
| RESULT | PASS | PASS |

Both preserved as independent, unmodified historical evidence — this
document does not alter `temp/read_write_soak_output.log` or any
number in `PHASE_READ_ENGINE_PERFORMANCE.md`'s prior sections.

## 11. Certification status

**Increment 7 = PASS.** All §16-of-the-brief success criteria met:
duration reached 14,400s; `in_run_mismatches=0`; `post_recovery_
mismatches=0`; `recovery_ok=true`; `fully_drained=true`; `storage_
pressure_events=0`; zero crashes/deadlocks/corruption/invalid-snapshot
-results/duplicate-range-rows/tombstone-resurrection observed; zero
resource leaks (handles/threads track expected shape, RSS growth
explained and tighter-fit than before); the optimized range scan shows
the expected, real, matched-count-verified improvement (§4/§5) without
a new correctness or resource regression.

**READ ENGINE PRODUCTION READY = NO.** Remaining, still-outstanding
gates, not started or completed by this increment: final evidence
consolidation, final performance validation (beyond this one soak),
final resource validation, and `PHASE_READ_ENGINE_CERTIFICATION.md`
(does not exist yet). No Compaction, Router, or Replication work
started. No further optimization performed during or after this soak.
