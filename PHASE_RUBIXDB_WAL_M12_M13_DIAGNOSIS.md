# PHASE RUBIXDB — WAL M1.2/M1.3 DIAGNOSIS

**Date:** 2026-10-04 · **Mode:** diagnosis only — no production file, test or threshold modified; no commit; no push. Raw data: `scratch/wal_diag/`. **Status: Phases 1–4 complete; Phase 5 (sections 6–7) NOT STARTED — waiting for user confirmation per the checkpoint.**

## 0. Environment (as recorded at start)
| Item | Value |
|---|---|
| Repo | `master` `0da9e14`, tree clean; `git diff master --stat` for `src/wal/` and all protected paths: **empty** (current HEAD *is* master) |
| **Baseline definition (deviation, flagged)** | The mission says baseline = `master`, but `master` already contains the flat-combining WAL (merge `4305a72`). The pre-flat-combining code ("unmodified baseline" in every earlier document) is `7b7aaaa`. Baseline worktree: `E:\rubixdb-baseline` at `7b7aaaa` (detached). `git worktree add ../rubixdb-baseline master` would have been refused (master is checked out) and would have compared the code with itself. |
| CPU | i7-7700 @ 3.6 GHz, 4C/8T, MaxClock 3601, CurrentClock 2300 at start (Balanced idle) |
| RAM | 17,060,876,288 B (16 GB) |
| Disks | Disk 0 SATA SSD 128 GB = `E:` (NTFS, 48.9 GB free of 127 GB); Disk 1 SATA "LAPCARE" = `C:`; D: 33.5 GB free. No NVMe |
| Power plan | **Balanced** (`381b4222-…`), captured before every run: identical in all 120 runs. Desktop, no battery device → AC |
| Defender | RealTimeProtectionEnabled=True, AntivirusEnabled=True. Exclusions **not viewable/modifiable (not administrator)** |
| Competing processes (not closed) | brave (many), devenv, explorer, claude, SearchApp, DevHub, msedgewebview2 — an ordinary developer machine, not a quiesced rig |
| Toolchain | cargo 1.98.1, rustc 1.98.1 |
| Binaries | current `E:\RubiXDb\target\release\deps\group_commit-719b233ae38aa9da.exe`; baseline same file name under `E:\rubixdb-baseline\target\release\deps\` (`--features test-util`, `--locked`). Baseline `rubixdb.exe` was not produced at `7b7aaaa` and is irrelevant: the benchmark is the test binary |
| Invocation | the compiled `group_commit` test binary was exec'd directly with `<test> --nocapture --test-threads=1` (what `cargo test … --test group_commit -- --test-threads=1 --nocapture` runs), `TMP=TEMP=E:\waltmp`, `RGC_TIMING_REPORT=1`, one scenario per process (M1.1 not run). Filters: `thousand_writers_throughput` (M1.3), `hundred_writers_throughput` (M1.2) |
| **Harness property found** | All runs executed at process priority **BelowNormal** (inherited from the agent's background job), affinity mask 255 (all 8 logical CPUs). Checked with 8 extra iterations at Normal priority (§4) |

### Measurements the mission asked for that this harness could NOT produce (stated, not invented)
* **p50/p95/p99/p99.9/max latency per M1 run** and **max records/batch**: the M1 tests do not record them and the timing report does not print them (it prints mean window/snapshot/fsync/coordination, batch count, close reasons). NOT MEASURED. *Average* records/batch = total records / batches is reported.
* **Baseline timing-report columns**: `7b7aaaa` printed no `[RGC_TIMING_REPORT]` line → NA.
* **Defender CPU** (`MsMpEng`): invisible without elevation → `mpeng_cpu_delta_s` = 0 in every row means *unobservable*, not "zero".
* `cpu_s = 0` in 8 rows is a harness defect (`TotalProcessorTime` unreadable after exit; a polled fallback was added after Phase 1). Those cells are not data.
* Phase 2's `Get-Counter -MaxSamples 600` background job was replaced by an equivalent 1 Hz in-loop sampler of the same nine counters. They are `PhysicalDisk(_Total)` — both disks summed, not per-process. Sampled runs: current Phase 1 iterations 11–20 and all H5/H7/priority runs; baseline iterations 11–20.

## 1. Baseline vs current four-way table
Classification as given: M1.3 FAST ≥ 95,000, SLOW < 85,000, 85–95 k AMBIGUOUS; M1.2 FAST ≥ 15,000, SLOW < 15,000.
| | current FAST | current SLOW | baseline FAST | baseline SLOW |
|---|---|---|---|---|
| **M1.3** count (50 current, 20 baseline) | 49 (97.0–108.2 k; median 103.1 k) | **0** | 0 | 20 (63.4–67.3 k; median 66.0 k) |
| M1.3 AMBIGUOUS | 1 current (93,533; external disk-activity signature, §3) | | | |
| **M1.2** count (30 current, 20 baseline) | 29 (15.8–20.4 k; median 17.1 k) | **1** (14,618) | 0 | 20 (9.4–12.1 k; median 10.5 k) |
| records/batch M1.3 (mean) | ~725–823 | – | NA | NA |
| records/batch M1.2 (mean) | 80–95 | 89 | NA | NA |
| window / fsync µs (M1.3) | 1,769–2,632 / 4,538–5,150 | – | NA | NA |
| window / fsync µs (M1.2) | 431–513 / 4,149–4,544 | 485 / **5,410** | NA | NA |
| process CPU s, M1.3 (valid rows) | 57.5–70.8 | – | 70.6–86.0 | |
| process CPU s, M1.2 (valid rows) | 6.8–14.5 | – | 7.3–16.3 | |
| threads / handles (max) | 1,005 / 1,061 (M1.3), 105 / 160 (M1.2) | | 1,005 / 1,065, 105 / 160 | |
| RSS MB (max) | 90 / 11 | | 113 / 15 | |
With the mission's thresholds every baseline run is "SLOW" by definition; the baseline distribution sits entirely below the current one.

**Answers**
* Bimodality present in baseline? **NO** for M1.3 (63.4–67.3 k, 6 % range). **M1.2 baseline shows a step**: 12.0–12.1 k in iterations 1–4, then 9.4–10.8 k in 5–20 (two levels, both below target).
* Bimodality present in current? **NOT REPRODUCED in this session** (M1.3: 0 SLOW in 58 runs; M1.2: one SLOW in 38). The earlier documents' bimodality (101–113 k vs 61–79 k) was **not observed**; this is not a refutation of it.
* Amplified by current? **INDETERMINATE** (the phenomenon was absent).
* Same-mode differences current vs baseline: M1.3 +56 % (103.1 k vs 66.0 k median); M1.2 +63 % (17.1 k vs 10.5 k); process CPU per M1.3 run 12–25 % lower; M1.3 context switches/s 0.92–1.09 M vs 0.41–0.44 M; M1.2 context switches/s 172–245 k vs 45–53 k; M1.2 system CPU 20–40 % vs 18–20 %.

## 2. Fast/slow mode frequency
| Series | M1.3 FAST / SLOW / AMBIG | M1.2 FAST / SLOW |
|---|---|---|
| Phase 1 current (20 interleaved) | 19 / 0 / 1 | 19 / 1 |
| H5 current back-to-back M1.3 (20) | 20 / 0 / 0 | – |
| H7 current, M1.2 first (10) | 10 / 0 / 0 | 10 / 0 |
| Normal-priority extra (8) | 8 / 0 / 0 | 8 / 0 |
| **Total current** | **57 / 0 / 1** (58) | **37 / 1** (38) |
| Baseline (20 interleaved) | 0 / 20 / 0 | 0 / 20 |
The earlier documents record the slow mode in ~25–30 % of quiet runs. At that rate, 0 of 58 independent runs has probability < 1e-8, so the slow mode is **state-dependent** (not a fixed per-run probability) and this session's machine was not in that state. What the state is remains unknown.

## 3. System-state correlation (which counters differ)
"Differs meaningfully" = the FAST mean is outside the non-FAST range or vice versa. **There is no SLOW M1.3 run; the only non-FAST runs are one AMBIGUOUS M1.3 and one SLOW M1.2 (n=1 each).** Values are per-run means (1 Hz).
**M1.3 — current FAST (39 sampled runs) vs the AMBIGUOUS run (p1 it20, 93,533 ops/s, wall 17.8 s vs 14.3–15.4 s):**
| Counter (mean) | FAST range | AMBIGUOUS | differs? |
|---|---|---|---|
| % Processor Performance | 107.6–111.6 | 111.1 | no |
| % Processor Time | 53.7–63.5 | **78.7** | yes |
| % Disk Time (_Total) | 2.8–5.1 | **81.3** | yes |
| Avg Disk Queue Length | 0.055–0.103 | **1.63** (max 6.4) | yes |
| Avg Disk sec/Write | 0.3–2.3 ms | 1.1 ms | no |
| Avg Disk sec/Read | 0.0–0.1 ms | **1.9 ms** | yes |
| Context switches/s | 0.92–1.09 M | 1.06 M | no |
| Processor queue length | 23–299 | 284 | no |
| Memory available MB | 9,132–9,589 | 9,213 | no |
This run coincided with heavy *read* activity and ~81 % disk busy (`_Total`), which none of the other 38 sampled runs show; its mean window was 1,769 µs (shorter) and batches 623 records (smaller) vs ~780. The test does not issue reads, but the counters are system-wide, so the source is **not identified**. Correlation only.
**M1.2 — current FAST (19 sampled) vs the SLOW run (p1 it17, 14,618 ops/s):** every counter mean of the slow run lies **inside** the FAST range (perf 56.2 vs 51.2–111.9; disk time 11.7 vs 4.3–48.7; queue 0.23 vs 0.09–0.97; write latency 0.9 ms vs 0.2–1.1; ctx switches 175 k vs 172–245 k) → **no counter differs**. The distinguishing measure is the run's own stage timing: mean fsync **5,410 µs** vs 4,149–4,544 µs in FAST runs (window 485 µs and batch 89 records are normal). Why that run's `FlushFileBuffers` was ~20 % slower is not shown by anything collected.
**Pre-run clock** (`% Processor Performance` just before each run, all 120 runs): 21.8–115.7 %. Ops/s do not track it (low-half vs high-half of pre-run clock, mean ops): current M1.3 102,848 vs 102,807; current M1.2 16,957 vs 17,058; baseline M1.3 66,103 vs 65,580; baseline M1.2 10,226 vs 10,968.
**Clock during a run depends on the workload**: M1.3 runs at 108–112 % (turbo) in every run; M1.2 at 51–112 % — M1.2 is fast with a low clock (baseline M1.2 at 60–72 %).

## 4. Hypothesis matrix (raw data in §10; CONFIRMED only with reproducible evidence)
| # | Hypothesis | Test and result | Verdict |
|---|---|---|---|
| H1 | CPU frequency scaling (prediction: fast runs have a consistently higher clock) | `% Processor Performance` before (all runs) and during (sampled). Pre-run value spans 22–116 % with **no relationship to ops/s** (§3). The one SLOW M1.2 run had during-run perf 56 %, inside the FAST range 51–112 %. No slow M1.3 exists. | **REJECTED** for the variation observed; INCONCLUSIVE for the documented 61–79 k mode (absent) |
| H2 | Defender interference | Not administrator: `Get-MpPreference` ExclusionPath = "N/A: Must be an administrator to view exclusions"; exclusions cannot be added; `MsMpEng` invisible to `Get-Process`. Not performed; Defender settings unchanged. | **INCONCLUSIVE** (untestable without elevation) |
| H3 | SATA queue saturation | FAST M1.3: queue 0.055–0.103, disk 2.8–5.1 % busy (mostly idle). The AMBIGUOUS run had queue 1.63 (max 6.4), 81 % busy, read latency 1.9 ms → external disk traffic coincided with it. M1.2 SLOW run: queue 0.23 (inside FAST range) but fsync stage +20 %. Baseline (~66 k, all runs): queue 0.05–0.08. | **INCONCLUSIVE** — coincident with the one ambiguous M1.3 run (n=1, system-wide, unattributed); no saturation in normal runs; no counter explains the slow M1.2 run |
| H4 | Scheduler wake-up latency / thread start | No SLOW M1.3 to compare. FAST M1.3: 0.92–1.09 M ctx/s, window 1.77–2.63 ms; the ambiguous run had a *shorter* window and ctx switches inside the FAST range. Thread-start cost is inside every timed run equally; H5 shows no trend. | **INCONCLUSIVE** (mode absent) |
| H5 | Harness warm-up | 20 back-to-back M1.3 (10 s gap): first 5 mean 103,169; last 5 mean 103,576 (+0.4 %); all 20 FAST (99.5–107.4 k). Phase 1 it1–5 vs it16–20: 102.1 k vs 99.8 k mean. | **REJECTED** |
| H6 | Temp dir / filesystem | `support::temp_dir` = `std::env::temp_dir()`; `TMP=TEMP=E:\waltmp` set identically for both binaries; `E:` = Disk 0, `C:` = Disk 1 (`Get-Partition`). The per-run directory was removed by the test; the path was not observed during a run. The tests' default TEMP is `C:` — **not run** (a different condition). | **REJECTED** as a cause of the current-vs-baseline difference (same filesystem by construction); not tested for the slow mode |
| H7 | Test ordering | M1.3 first in sequence (p1, 20 runs): 93.5–106.5 k, median 101.4 k; M1.3 second (h7, 10 runs): 98.4–108.2 k, median 103.0 k; all FAST (one ambiguous at position 1). | **REJECTED** in this session (mode absent) |
| H8 | Affinity / NUMA | Affinity mask 255 in every run (all 8 logical CPUs, no pinning); single socket. Priority **BelowNormal** in every run (harness-inherited). Extra: 8 iterations at **Normal**: M1.3 100.1–104.3 k (all FAST), M1.2 17.7–18.4 k (all FAST; vs 17.1 k median at BelowNormal, ~+5 %). | **REJECTED** for M1.3; a small M1.2 effect of priority is *suggested* (n=8, not tested for significance) |
| H9 | Combination (WAL × environment) | Only if H1–H8 fail to explain; they do not, because the slow mode never occurred. Observation only: the documented slow range (61–79 k) overlaps the baseline's 63.4–67.3 k; current M1.3 never fell below 93.5 k today. A same-mode slow-vs-slow comparison is impossible. | **INCONCLUSIVE** |

## 5. Root cause
**NOT ESTABLISHED.** The M1.3 slow mode (61–79 k) did not recur in 58 current runs (lowest 93,533), so no hypothesis could be confirmed against it. The single M1.2 slow run (14,618) had a longer `FlushFileBuffers` (5.41 ms vs 4.15–4.54 ms) that no collected counter explains; the single ambiguous M1.3 run (93,533) coincided with external disk read activity (81 % disk busy) absent from the other runs (§3, §10). Per rule 7 the classification is **OPEN, not PASS**: M1.3 is not shown to be hardware-limited, and no WAL-code cause was found.
Not excluded and untested (no evidence either way): time-varying SSD state (cache/garbage collection), other applications' background activity at the earlier session times, and the earlier sessions' concurrent campaigns/samplers.

## 6. Design options
**NOT STARTED.** Phase 5 requires user confirmation (checkpoint after Phase 4). With no root cause found, any option would be speculative.

## 7. Recommendation
**PENDING** (depends on §6). Suggestion for the next mission, not a design option: reproduce under the conditions in which the slow mode appeared (the production-operations phase), with the sampler on from the first run, the per-run stage report, and elevated rights (Defender CPU, per-disk and per-process I/O).

## 8. Protected-path boundary check
Root cause not identified, so it cannot be placed inside or outside a protected component. **Observed: no protected path and no source file was modified or touched** (`git status` shows only the new `scratch/wal_diag/` and this file).

## 9. Open items (out of scope or unresolved)
1. The slow M1.3 mode is not reproduced (state-dependent): §5.
2. Defender behaviour is untestable without administrator rights.
3. The M1 tests record no per-commit latency; per-run percentiles are unavailable (needs `examples/wal_commit_latency.rs` or a test-side recorder).
4. Baseline `7b7aaaa` has no timing report under `RGC_TIMING_REPORT`.
5. The agent's launcher runs test processes at BelowNormal; `scripts/wal_certify.ps1` sets no priority (inherits its parent's). M1.2 was ~5 % higher at Normal (n=8).
6. Baseline M1.2 stepped from ~12.1 k to ~10 k within one session (iteration 4→5, no cause found; pre-run clock 113 % before it4, 23 % before it5, but later runs at 97–115 % were also ~10.5 k).
7. Left on disk: `E:\rubixdb-baseline` worktree (registered in `.git/worktrees`), `E:\waltmp`, `scratch/wal_diag/`.
(CREATE INDEX timeout: not encountered.)

## 10. Raw data
Files: `scratch/wal_diag/{p1-cur,p3-base,h5-cur,h7-cur,prio-cur}-runs.csv` (one row per run, all columns incl. close reasons and power plan), `*-counters.csv` (1 Hz samples), `all_runs_with_counters.csv` (per-run counter mean/max), `analyze.py`.
### Phase 1 — current, interleaved M1.3 → 10 s → M1.2 → 10 s (20 iterations; power plan Balanced in every row)
| iter | test | ops | class | batches | rec_per_batch | window_us | fsync_us | cpu_s | wall_s | rss_mb | threads | handles | perfpct_pre | start_time |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | M13 | 104925 | FAST | 1316 | 759.9 | 2220.6 | 4657.0 | 57.5 | 14.9 | 88 | 1005 | 1059 | 104.4 | 14:00:36 |
| 1 | M12 | 16583 | FAST | 1192 | 83.9 | 483.4 | 4364.5 | 13.3 | 7.2 | 11 | 105 | 160 | 22.2 | 14:01:02 |
| 2 | M13 | 96975 | FAST | 1294 | 772.8 | 2473.3 | 5086.9 | 70.4 | 15.4 | 88 | 1005 | 1060 | 37.4 | 14:01:21 |
| 2 | M12 | 16161 | FAST | 1199 | 83.4 | 512.2 | 4408.0 | 13.4 | 7.1 | 8 | 105 | 160 | 30.8 | 14:01:47 |
| 3 | M13 | 106280 | FAST | 1235 | 809.7 | 2479.0 | 4753.5 | 64.2 | 14.3 | 84 | 1005 | 1060 | 26.3 | 14:02:05 |
| 3 | M12 | 16248 | FAST | 1197 | 83.5 | 513.7 | 4390.4 | 14.5 | 7.2 | 9 | 105 | 160 | 22 | 14:02:31 |
| 4 | M13 | 102536 | FAST | 1245 | 803.2 | 2548.2 | 4907.7 | 66.6 | 14.8 | 87 | 1005 | 1060 | 24.5 | 14:02:49 |
| 4 | M12 | 16123 | FAST | 1220 | 82 | 510.1 | 4324.0 | 0 | 7.2 | 9 | 105 | 160 | 37.9 | 14:03:15 |
| 5 | M13 | 104501 | FAST | 1281 | 780.6 | 2340.7 | 4759.3 | 66.6 | 14.9 | 81 | 1005 | 1060 | 46.2 | 14:03:33 |
| 5 | M12 | 16414 | FAST | 1182 | 84.6 | 507.2 | 4381.1 | 13.5 | 7.1 | 10 | 105 | 160 | 24.7 | 14:03:59 |
| 6 | M13 | 103589 | FAST | 1221 | 819 | 2568.2 | 4940.2 | 65.8 | 14.8 | 89 | 1005 | 1060 | 31.1 | 14:04:17 |
| 6 | M12 | 15809 | FAST | 1247 | 80.2 | 491.5 | 4334.8 | 14.4 | 7.2 | 6 | 105 | 160 | 75.2 | 14:04:43 |
| 7 | M13 | 100787 | FAST | 1215 | 823 | 2577.0 | 5149.6 | 68.5 | 14.9 | 85 | 1005 | 1060 | 22.3 | 14:05:02 |
| 7 | M12 | 15865 | FAST | 1240 | 80.6 | 499.3 | 4337.2 | 14.3 | 7.2 | 6 | 105 | 160 | 22.6 | 14:05:28 |
| 8 | M13 | 105976 | FAST | 1237 | 808.4 | 2439.9 | 4786.5 | 62.2 | 14.4 | 82 | 1005 | 1061 | 22.2 | 14:05:46 |
| 8 | M12 | 16276 | FAST | 1172 | 85.3 | 486.0 | 4543.8 | 11.2 | 7.2 | 10 | 105 | 160 | 22.9 | 14:06:11 |
| 9 | M13 | 100657 | FAST | 1239 | 807.1 | 2632.1 | 4975.0 | 66.3 | 14.9 | 85 | 1005 | 1061 | 22.7 | 14:06:30 |
| 9 | M12 | 17057 | FAST | 1141 | 87.6 | 481.5 | 4477.5 | 11.1 | 7.2 | 6 | 105 | 160 | 34.9 | 14:06:55 |
| 10 | M13 | 99659 | FAST | 1253 | 798.1 | 2556.7 | 5070.7 | 68.7 | 14.9 | 82 | 1005 | 1061 | 22.3 | 14:07:14 |
| 10 | M12 | 16204 | FAST | 1216 | 82.2 | 502.6 | 4332.9 | 13.7 | 7.1 | 8 | 105 | 160 | 22 | 14:07:40 |
| 11 | M13 | 101926 | FAST | 1284 | 778.8 | 2369.2 | 4878.3 | 67 | 15.4 | 86 | 1005 | 1060 | 22.1 | 14:07:58 |
| 11 | M12 | 17089 | FAST | 1135 | 88.1 | 484.0 | 4453.3 | 9.7 | 7.2 | 7 | 105 | 160 | 22.2 | 14:08:24 |
| 12 | M13 | 100730 | FAST | 1352 | 739.6 | 2206.9 | 4779.6 | 68.2 | 15.3 | 80 | 1005 | 1061 | 23.2 | 14:08:43 |
| 12 | M12 | 17202 | FAST | 1142 | 87.6 | 499.0 | 4389.0 | 10.8 | 7.2 | 7 | 105 | 160 | 22.3 | 14:09:09 |
| 13 | M13 | 99262 | FAST | 1294 | 772.8 | 2507.8 | 4894.9 | 68.9 | 15.3 | 80 | 1005 | 1061 | 21.9 | 14:09:27 |
| 13 | M12 | 17166 | FAST | 1135 | 88.1 | 496.2 | 4427.4 | 10.4 | 7.1 | 6 | 105 | 160 | 22.8 | 14:09:54 |
| 14 | M13 | 105956 | FAST | 1291 | 774.6 | 2234.2 | 4712.0 | 0 | 15.3 | 72 | 1005 | 1061 | 23.2 | 14:10:12 |
| 14 | M12 | 17210 | FAST | 1156 | 86.5 | 481.6 | 4362.5 | 0 | 7.1 | 6 | 105 | 160 | 24.2 | 14:10:38 |
| 15 | M13 | 104318 | FAST | 1285 | 778.2 | 2363.2 | 4766.3 | 62.5 | 15.3 | 88 | 1005 | 1061 | 28.3 | 14:10:57 |
| 15 | M12 | 17572 | FAST | 1122 | 89.1 | 488.1 | 4391.9 | 9.9 | 7.1 | 9 | 105 | 160 | 23.1 | 14:11:23 |
| 16 | M13 | 98976 | FAST | 1344 | 744 | 2305.5 | 4880.0 | 67.8 | 15.3 | 78 | 1005 | 1060 | 31.3 | 14:11:41 |
| 16 | M12 | 17868 | FAST | 1102 | 90.7 | 476.9 | 4412.7 | 8.9 | 7.1 | 9 | 105 | 160 | 22.6 | 14:12:08 |
| 17 | M13 | 97836 | FAST | 1328 | 753 | 2473.3 | 4837.7 | 69.4 | 15.3 | 77 | 1005 | 1061 | 89.7 | 14:12:26 |
| 17 | M12 | 14618 | SLOW | 1123 | 89 | 485.0 | 5409.6 | 0 | 8.2 | 7 | 105 | 160 | 34.8 | 14:12:52 |
| 18 | M13 | 100708 | FAST | 1303 | 767.5 | 2356.1 | 4910.6 | 67.3 | 15.4 | 83 | 1005 | 1061 | 22.9 | 14:13:12 |
| 18 | M12 | 17187 | FAST | 1134 | 88.2 | 491.7 | 4440.5 | 0 | 7.2 | 6 | 105 | 160 | 22.2 | 14:13:38 |
| 19 | M13 | 106494 | FAST | 1287 | 777 | 2237.3 | 4701.7 | 0 | 15.3 | 72 | 1005 | 1060 | 28.3 | 14:13:56 |
| 19 | M12 | 17496 | FAST | 1129 | 88.6 | 474.2 | 4387.2 | 11.8 | 7.2 | 8 | 105 | 160 | 22.6 | 14:14:23 |
| 20 | M13 | 93533 | AMBIGUOUS | 1604 | 623.4 | 1769.0 | 4538.1 | 0 | 17.8 | 77 | 1005 | 1061 | 111 | 14:14:41 |
| 20 | M12 | 20371 | FAST | 1057 | 94.6 | 431.2 | 4148.6 | 0 | 6.1 | 6 | 105 | 160 | 112.8 | 14:15:10 |

### Phase 3 — baseline `7b7aaaa`, same schedule
| iter | test | ops | class | cpu_s | wall_s | rss_mb | threads | handles | perfpct_pre | start_time |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | M13 | 65145 | SLOW | 82.8 | 21.1 | 89 | 1005 | 1065 | 113.7 | 14:15:44 |
| 1 | M12 | 12101 | SLOW | 7.3 | 9.2 | 9 | 105 | 160 | 113 | 14:16:16 |
| 2 | M13 | 65925 | SLOW | 78.5 | 20.5 | 88 | 1005 | 1061 | 115.7 | 14:16:36 |
| 2 | M12 | 12009 | SLOW | 8 | 9.2 | 8 | 105 | 160 | 115.7 | 14:17:08 |
| 3 | M13 | 64333 | SLOW | 81.3 | 21 | 81 | 1005 | 1061 | 115.4 | 14:17:28 |
| 3 | M12 | 12015 | SLOW | 7.8 | 9.2 | 8 | 105 | 160 | 97.2 | 14:18:00 |
| 4 | M13 | 65524 | SLOW | 76.6 | 20.5 | 84 | 1005 | 1061 | 111.8 | 14:18:20 |
| 4 | M12 | 12093 | SLOW | 8.4 | 9.2 | 9 | 105 | 160 | 112.6 | 14:18:52 |
| 5 | M13 | 63384 | SLOW | 70.6 | 21.5 | 87 | 1005 | 1061 | 111.2 | 14:19:12 |
| 5 | M12 | 9717 | SLOW | 15.5 | 11.2 | 8 | 105 | 160 | 23.1 | 14:19:44 |
| 6 | M13 | 66500 | SLOW | 79.7 | 19.9 | 83 | 1005 | 1060 | 22.1 | 14:20:07 |
| 6 | M12 | 10294 | SLOW | 14 | 10.7 | 9 | 105 | 160 | 67.6 | 14:20:38 |
| 7 | M13 | 67266 | SLOW | 79.4 | 20.1 | 113 | 1005 | 1061 | 23.1 | 14:20:59 |
| 7 | M12 | 10047 | SLOW | 15.5 | 10.8 | 6 | 105 | 160 | 21.8 | 14:21:31 |
| 8 | M13 | 67054 | SLOW | 81.2 | 20 | 87 | 1005 | 1061 | 25.4 | 14:21:52 |
| 8 | M12 | 9443 | SLOW | 15.7 | 11.7 | 12 | 105 | 160 | 26.8 | 14:22:23 |
| 9 | M13 | 66623 | SLOW | 83.6 | 20.1 | 86 | 1005 | 1061 | 24.5 | 14:22:46 |
| 9 | M12 | 9650 | SLOW | 16.3 | 11.3 | 8 | 105 | 160 | 24 | 14:23:17 |
| 10 | M13 | 66706 | SLOW | 81.1 | 20 | 86 | 1005 | 1061 | 21.9 | 14:23:40 |
| 10 | M12 | 9489 | SLOW | 15.4 | 11.7 | 6 | 105 | 160 | 29.8 | 14:24:11 |
| 11 | M13 | 66894 | SLOW | 78.3 | 20.6 | 86 | 1005 | 1061 | 23 | 14:24:34 |
| 11 | M12 | 10576 | SLOW | 12.3 | 10.3 | 5 | 105 | 160 | 22.7 | 14:25:05 |
| 12 | M13 | 64415 | SLOW | 78.1 | 20.6 | 73 | 1005 | 1060 | 22.7 | 14:25:27 |
| 12 | M12 | 10758 | SLOW | 10.8 | 10.2 | 5 | 105 | 160 | 23.2 | 14:25:58 |
| 13 | M13 | 63873 | SLOW | 86 | 20.6 | 73 | 1005 | 1061 | 22.8 | 14:26:20 |
| 13 | M12 | 10186 | SLOW | 13.7 | 11.2 | 8 | 105 | 160 | 32.8 | 14:26:51 |
| 14 | M13 | 67178 | SLOW | 79.4 | 20.4 | 85 | 1005 | 1061 | 30.4 | 14:27:14 |
| 14 | M12 | 10373 | SLOW | 13.7 | 11.3 | 12 | 105 | 160 | 24.4 | 14:27:45 |
| 15 | M13 | 65763 | SLOW | 78.7 | 20.5 | 80 | 1005 | 1061 | 23.3 | 14:28:08 |
| 15 | M12 | 10564 | SLOW | 12.6 | 10.3 | 5 | 105 | 160 | 27.7 | 14:28:39 |
| 16 | M13 | 65831 | SLOW | 81.1 | 20.4 | 78 | 1005 | 1060 | 33.3 | 14:29:01 |
| 16 | M12 | 10447 | SLOW | 12.8 | 11.2 | 11 | 105 | 160 | 29.2 | 14:29:32 |
| 17 | M13 | 66268 | SLOW | 80.9 | 20.4 | 79 | 1005 | 1061 | 88 | 14:29:54 |
| 17 | M12 | 10735 | SLOW | 13 | 10.2 | 5 | 105 | 160 | 23 | 14:30:26 |
| 18 | M13 | 66201 | SLOW | 77.4 | 20.4 | 79 | 1005 | 1060 | 29.3 | 14:30:47 |
| 18 | M12 | 10488 | SLOW | 13.8 | 11.2 | 12 | 105 | 160 | 27.6 | 14:31:19 |
| 19 | M13 | 66018 | SLOW | 78.4 | 20.5 | 81 | 1005 | 1061 | 30.1 | 14:31:41 |
| 19 | M12 | 10520 | SLOW | 12.5 | 11.2 | 15 | 105 | 160 | 22.9 | 14:32:13 |
| 20 | M13 | 65940 | SLOW | 78.5 | 20.5 | 81 | 1005 | 1061 | 22.1 | 14:32:35 |
| 20 | M12 | 10449 | SLOW | 13.2 | 11.4 | 6 | 105 | 160 | 22.1 | 14:33:06 |

### H5 — current, 20 back-to-back M1.3 (10 s gap)
| iter | ops | class | batches | rec_per_batch | window_us | fsync_us | cpu_s | wall_s | perfpct_pre |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 102865 | FAST | 1362 | 734.2 | 2100.7 | 4704.6 | 64.3 | 15.4 | 94.3 |
| 2 | 99764 | FAST | 1294 | 772.8 | 2413.3 | 4942.8 | 66.4 | 15.3 | 23 |
| 3 | 107366 | FAST | 1278 | 782.5 | 2220.4 | 4706.0 | 57.8 | 14.3 | 22.5 |
| 4 | 103152 | FAST | 1326 | 754.1 | 2281.3 | 4670.3 | 63.2 | 15.3 | 34.7 |
| 5 | 102700 | FAST | 1279 | 781.9 | 2298.8 | 4917.6 | 64.5 | 15.3 | 23.1 |
| 6 | 100966 | FAST | 1291 | 774.6 | 2438.8 | 4840.3 | 65.1 | 15.3 | 22.7 |
| 7 | 100273 | FAST | 1307 | 765.1 | 2227.3 | 5005.2 | 63.1 | 15.3 | 22.7 |
| 8 | 103289 | FAST | 1289 | 775.8 | 2397.6 | 4710.4 | 61.6 | 15.3 | 22.3 |
| 9 | 103332 | FAST | 1297 | 771 | 2294.4 | 4802.4 | 64.1 | 15.3 | 70.1 |
| 10 | 104342 | FAST | 1265 | 790.5 | 2361.4 | 4837.1 | 61.6 | 15.4 | 22.8 |
| 11 | 105472 | FAST | 1282 | 780 | 2294.2 | 4698.5 | 62.2 | 15.3 | 22.4 |
| 12 | 104742 | FAST | 1239 | 807.1 | 2540.3 | 4798.9 | 62.1 | 15.3 | 22.5 |
| 13 | 105433 | FAST | 1308 | 764.5 | 2167.2 | 4736.0 | 61.3 | 15.3 | 28.2 |
| 14 | 105608 | FAST | 1242 | 805.2 | 2515.9 | 4740.6 | 59.6 | 15.3 | 22.5 |
| 15 | 103142 | FAST | 1242 | 805.2 | 2581.6 | 4856.5 | 64 | 15.3 | 22.3 |
| 16 | 99499 | FAST | 1337 | 747.9 | 2193.7 | 4892.5 | 69.7 | 15.4 | 23.3 |
| 17 | 106619 | FAST | 1284 | 778.8 | 2350.1 | 4628.2 | 60.7 | 14.3 | 83.3 |
| 18 | 102373 | FAST | 1272 | 786.2 | 2436.2 | 4860.8 | 65.8 | 15.3 | 22.8 |
| 19 | 105378 | FAST | 1300 | 769.2 | 2178.2 | 4769.9 | 61.7 | 15.3 | 23.1 |
| 20 | 104013 | FAST | 1256 | 796.2 | 2408.8 | 4862.8 | 64.8 | 15.3 | 22.3 |

### H7 — current, M1.2 first then M1.3 (10 iterations)
| iter | pos | test | ops | class | rec_per_batch | window_us | fsync_us | cpu_s | wall_s | perfpct_pre |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1 | M12 | 17306 | FAST | 87.3 | 473.5 | 4399.1 | 11.8 | 7.1 | 60.5 |
| 1 | 2 | M13 | 105308 | FAST | 786.8 | 2340.8 | 4759.5 | 60.8 | 15.3 | 23.1 |
| 2 | 1 | M12 | 16991 | FAST | 85.8 | 485.2 | 4354.9 | 12.1 | 7.1 | 22.5 |
| 2 | 2 | M13 | 108235 | FAST | 784.3 | 2205.0 | 4683.5 | 59.1 | 14.3 | 24.1 |
| 3 | 1 | M12 | 17956 | FAST | 90.2 | 473.2 | 4369.0 | 9.2 | 7.1 | 23.3 |
| 3 | 2 | M13 | 107007 | FAST | 793 | 2307.8 | 4734.9 | 61.5 | 14.3 | 22 |
| 4 | 1 | M12 | 17267 | FAST | 87.2 | 473.7 | 4380.1 | 9.7 | 7.1 | 28.9 |
| 4 | 2 | M13 | 100914 | FAST | 793 | 2463.7 | 4999.7 | 65.2 | 15.3 | 31.9 |
| 5 | 1 | M12 | 17067 | FAST | 86.7 | 488.7 | 4388.1 | 12.5 | 7.2 | 22 |
| 5 | 2 | M13 | 102118 | FAST | 744 | 2237.8 | 4713.5 | 64.7 | 15.3 | 22.7 |
| 6 | 1 | M12 | 17520 | FAST | 87.6 | 495.5 | 4323.8 | 7.9 | 7.2 | 69.7 |
| 6 | 2 | M13 | 101269 | FAST | 724.6 | 2112.3 | 4714.4 | 59.1 | 15.3 | 42 |
| 7 | 1 | M12 | 16667 | FAST | 83.3 | 495.1 | 4298.1 | 9.9 | 7.2 | 26.7 |
| 7 | 2 | M13 | 103805 | FAST | 773.4 | 2337.6 | 4750.4 | 62 | 15.3 | 27.9 |
| 8 | 1 | M12 | 17604 | FAST | 88.4 | 477.6 | 4345.2 | 9 | 7.1 | 28.8 |
| 8 | 2 | M13 | 98373 | FAST | 751.9 | 2329.8 | 4922.0 | 70.8 | 15.3 | 22.7 |
| 9 | 1 | M12 | 17796 | FAST | 90.3 | 494.4 | 4389.3 | 8.8 | 7.2 | 23.1 |
| 9 | 2 | M13 | 101119 | FAST | 778.8 | 2427.3 | 4880.5 | 67.2 | 15.2 | 31.5 |
| 10 | 1 | M12 | 17548 | FAST | 88 | 480.0 | 4354.4 | 11 | 7.2 | 23.1 |
| 10 | 2 | M13 | 107288 | FAST | 792.4 | 2328.7 | 4685.0 | 61.8 | 15.3 | 23.6 |

### Extra — Normal priority (8 iterations)
| iter | test | ops | class | rec_per_batch | window_us | fsync_us | cpu_s | wall_s | prio | perfpct_pre |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | M13 | 100069 | FAST | 732.6 | 2163.4 | 4813.0 | 64.2 | 15.5 | Normal | 90.6 |
| 1 | M12 | 18430 | FAST | 92.3 | 474.3 | 4364.9 | 7.9 | 6.2 | Normal | 29.2 |
| 2 | M13 | 104302 | FAST | 730.5 | 2044.3 | 4642.5 | 57.7 | 14.9 | Normal | 25.4 |
| 2 | M12 | 17921 | FAST | 89.1 | 473.7 | 4335.8 | 8.1 | 7.3 | Normal | 29.1 |
| 3 | M13 | 102395 | FAST | 755.3 | 2334.7 | 4690.4 | 60 | 14.8 | Normal | 23.4 |
| 3 | M12 | 18359 | FAST | 92.2 | 474.4 | 4393.6 | 6.8 | 7.2 | Normal | 26.1 |
| 4 | M13 | 103174 | FAST | 725.7 | 2007.8 | 4705.5 | 61.5 | 15.3 | Normal | 29.8 |
| 4 | M12 | 17844 | FAST | 89.4 | 486.5 | 4343.7 | 10.8 | 7.2 | Normal | 24.4 |
| 5 | M13 | 103066 | FAST | 749.6 | 2130.5 | 4771.6 | 62.1 | 15 | Normal | 39.7 |
| 5 | M12 | 17889 | FAST | 91.1 | 494.2 | 4400.0 | 9.1 | 7.2 | Normal | 24.2 |
| 6 | M13 | 102128 | FAST | 752.4 | 2264.8 | 4753.4 | 63.6 | 15.4 | Normal | 23.4 |
| 6 | M12 | 17675 | FAST | 89.5 | 502.1 | 4362.8 | 8.2 | 7.2 | Normal | 62.5 |
| 7 | M13 | 101316 | FAST | 742.4 | 2188.9 | 4785.7 | 64.8 | 15.9 | Normal | 30 |
| 7 | M12 | 17769 | FAST | 89.8 | 490.7 | 4368.9 | 9.3 | 7.2 | Normal | 30.8 |
| 8 | M13 | 101528 | FAST | 745.7 | 2256.4 | 4736.4 | 61.8 | 14.9 | Normal | 35.5 |
| 8 | M12 | 17961 | FAST | 90.3 | 501.6 | 4348.4 | 10.4 | 7.2 | Normal | 28.3 |


## 11. Certification impact
* **Nothing is superseded or upgraded.** The addendum in `PHASE_RUBIXDB_WAL_CERTIFICATION_CLOSURE.md` (M1.3 = FAIL intermittent, M1.2 = OPEN) stands: this session neither reproduced the FAIL nor produced evidence that would justify PASS.
* Supporting evidence for the design comparison (not the gate): same schedule, current vs baseline M1.3 103.1 k vs 66.0 k, M1.2 17.1 k vs 10.5 k (1.56× / 1.63×); the closure's statement that the baseline fails both targets in isolation is **confirmed**.
* The "~25–30 % of quiet runs slow" figure is **not supported** by this session (0 of 58 M1.3 runs).
* Classification per rule 7: M1.3 = **OPEN** (not HARDWARE-LIMITED: no evidence; not PASS).
