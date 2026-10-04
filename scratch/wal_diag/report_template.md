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
@@P1@@
### Phase 3 — baseline `7b7aaaa`, same schedule
@@B@@
### H5 — current, 20 back-to-back M1.3 (10 s gap)
@@H5@@
### H7 — current, M1.2 first then M1.3 (10 iterations)
@@H7@@
### Extra — Normal priority (8 iterations)
@@PR@@

## 11. Certification impact
* **Nothing is superseded or upgraded.** The addendum in `PHASE_RUBIXDB_WAL_CERTIFICATION_CLOSURE.md` (M1.3 = FAIL intermittent, M1.2 = OPEN) stands: this session neither reproduced the FAIL nor produced evidence that would justify PASS.
* Supporting evidence for the design comparison (not the gate): same schedule, current vs baseline M1.3 103.1 k vs 66.0 k, M1.2 17.1 k vs 10.5 k (1.56× / 1.63×); the closure's statement that the baseline fails both targets in isolation is **confirmed**.
* The "~25–30 % of quiet runs slow" figure is **not supported** by this session (0 of 58 M1.3 runs).
* Classification per rule 7: M1.3 = **OPEN** (not HARDWARE-LIMITED: no evidence; not PASS).
