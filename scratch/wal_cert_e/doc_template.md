# PHASE RUBIXDB — WAL M1.2/M1.3 E-DRIVE CERTIFICATION CAMPAIGN

**Date:** 2026-10-04 (15:48:47–16:03:39 local) · **Mission:** performance certification only. No production, WAL or test code and no threshold was modified; nothing was committed or pushed.
> **This certification set is bounded to the 20 fresh consecutive M1.2 runs and 20 fresh consecutive M1.3 runs executed on E: under the documented controlled environment. Historical runs are retained as historical evidence and are not part of this set.**
This document certifies only M1.2 and M1.3. It does not certify the product (not "production ready"), power loss, NVMe, Backup/Restore, DR, Security, Observability, Maintenance, CREATE INDEX, Advanced SQL or distributed execution.

## 1. Environment
| Item | Value |
|---|---|
| Repo | `master` `0da9e14c9ca86b6d26f84061c0c60f1cc17cae2b`; `git diff HEAD --stat -- src/wal/` and `-- src tests`: **empty** before the campaign; `src/wal/` identical to the last certified implementation `3ec5034` (`git diff 3ec5034 HEAD -- src/wal` empty). Working tree: only documentation and `scratch/` changes by earlier missions (no source change) |
| Drive / TEMP / TMP | `E:` (Disk 0, SATA SSD, 48.5 GB free of 127 GB) / `E:\waltmp` / `E:\waltmp` (set by the runner for every run; the tests write via `std::env::temp_dir()`) |
| Priority | **Normal** (set immediately after process start; observed `Normal` in all 40 rows; the agent shell's own default is BelowNormal, so it was set explicitly) |
| Power | AC (desktop, 0 battery devices), plan **Balanced** (`381b4222-…`), identical in all 40 per-run captures |
| CPU | i7-7700 @ 3.6 GHz, 4C/8T; affinity mask 255 (all 8 logical CPUs) in all 40 rows; clock 3601/3601 at start |
| Build | release, `--features test-util` (required for the timing report) |
| Procedure | the compiled `group_commit` test binary executed directly with `<test> --nocapture --test-threads=1`, one scenario per process (what `scripts/wal_certify.ps1`'s `cargo test --release --features test-util --test group_commit -- --test-threads=1 --nocapture` runs), `RGC_TIMING_REPORT=1`; M1.2 campaign first, then the M1.3 campaign, strictly sequential; 10 s gap between runs; no concurrent scenario; no other cargo build during the campaign (the binary was verified up to date beforehand: `cargo … --no-run` finished in 0.26 s, nothing rebuilt) |
| Background | Not a quiesced rig: Brave, DevHub, Task Manager, Visual Studio, the agent were open (top CPU totals before start: brave 846/446/277 s, DevHub 222 s, Taskmgr 149 s …). No intentional workload was started |
| System counters | 1 Hz, every run (`Get-Counter`): `% Processor Performance`, `% Processor Time`, `PhysicalDisk(_Total)` % Disk Time / queue / sec per Write / sec per Read, available MB, context switches/s, processor queue. **`PhysicalDisk(_Total)` sums both SATA disks** (not per-disk); all counters were populated (6–15 samples per run) |

## 2. Binary identity
`E:\RubiXDb\target\release\deps\group_commit-719b233ae38aa9da.exe` · 1,280,000 bytes · modified 2026-10-04 13:59:33 · **SHA-256 `A5CA2B71E308A5847B906911CDCCDA759433C6638985ADEB0FC925CB4011C011`** (pre-campaign snapshot: `scratch/wal_cert_e/pre-campaign.txt`).

## 3. Certification-set definition
* **M1.2:** exactly 20 consecutive fresh runs (100 writers × 1,000 records). **M1.3:** exactly 20 consecutive fresh runs (1,000 writers × 1,000 records).
* **Rule A:** every run individually ≥ threshold (M1.2 ≥ 15,000 ops/s; M1.3 ≥ 80,000 ops/s, taken from the tests, unchanged). No median/percentile/K-of-N/outlier tolerance/best-run selection. No run was rerun or replaced; no run was excluded.
* Correctness assertion preserved: each test also asserts, after reopen, no corrupted segments, record count == acknowledged count and gap-free ordered sequence numbers; all 40 invocations ended `test result: ok`, so those assertions held in every run (column "result").

## 4. M1.2 — 20 consecutive runs (100 writers × 1,000 records = 100,000 records)
Columns: elapsed = workload time printed by the test; batches/rec per batch/window/fsync/coord = the engine's timing report (mean per batch, µs); CPU = process CPU seconds, RSS = peak working set, threads/handles = peak; "pre clk" = `% Processor Performance` immediately before the run, "clk" = mean during the run; CPU %, disk time, queue, write/read latency, memory, context switches = means over the run (disk time/queue/write latency also show the per-run max). Reads of 0.00 are measured values.
@@M12TABLE@@

## 5. M1.3 — 20 consecutive runs (1,000 writers × 1,000 records = 1,000,000 records)
@@M13TABLE@@

## 6. System measurements (summary)
| | M1.2 | M1.3 |
|---|---|---|
| fsync stage (mean per batch) | 4,217–4,860 µs | 4,609–5,081 µs |
| batch window | 434–498 µs | 2,000–2,456 µs |
| records / batch | 86.4–97.0 | 713.8–786.2 |
| disk write latency (`_Total`, per-run mean) | 0.27–0.74 ms | 0.26–1.15 ms |
| disk busy (per-run mean) / queue | 4.5–10.6 % / 0.09–0.21 | 2.9–6.6 % / 0.06–0.13 |
| clock during run | 57–104 % (workload-dependent; M1.2 does not boost) | 108–112 % |
| CPU (process s) / RSS / threads / handles | 3.5–12.2 s / 6–13 MB / 105 / 159–160 | 52.8–65.9 s / 73–89 MB / 1,005 / 1,060 |
| `% Processor Time` (system) | 20.8–37.9 % | 53.3–68.5 % |
No run showed the device-latency inflation seen earlier (the 2026-10-04 `C:` slow run: 10.4 ms write latency, 9.9 ms fsync stage). Highest single-second disk write latency in the campaign: M1.3 run 5, 10.9 ms (max); that run measured 99,784 ops/s.

## 7. Failures
None. 0 of 20 M1.2 runs and 0 of 20 M1.3 runs were below threshold.

## 8. Invalid-run decisions
None: all 40 runs are valid and included. Harness defects that did not affect any measurement: (a) the per-run raw stdout/stderr copy command in my runner failed ("Illegal characters in path") after each run's CSV row was already written, so the raw per-run test output files were **not preserved** — the parsed values and the "result = ok" flag are in `scratch/wal_cert_e/{m12,m13}-cur-runs.csv`; (b) counters are 1 Hz samples of a PowerShell sampler running in the same loop (small unmeasured perturbation; it was also present in the earlier sets); (c) per-run latency percentiles (p50/p99.9 of commit latency) are not produced by the M1 tests and were not measured.

## 9. Rule A calculation
| | M1.2 | M1.3 |
|---|---|---|
| runs | 20 | 20 |
| ≥ threshold | **20** (≥ 15,000) | **20** (≥ 80,000) |
| < threshold | **0** | **0** |
| minimum | **16,704** (run 4) | **97,288** (run 2) |
| maximum | 20,340 (run 12) | 109,019 (run 12) |
| median | 17,961 | 102,377 |
| mean | 18,078 | 102,051 |
| p5 / p50 / p95 / p99 (linear interpolation of the 20 per-run throughputs; not latency percentiles) | 17,112 / 17,961 / 19,981 / 20,268 | 97,655 / 102,376 / 106,433 / 108,502 |
Margins of the lowest run over the threshold: M1.2 +11.4 %, M1.3 +21.6 %.
(Median/percentiles are descriptive only; the decision uses Rule A.)

## 10. Final status (for this bounded set)
* **M1.2 = PASS** (20/20 ≥ 15,000; minimum 16,704)
* **M1.3 = PASS** (20/20 ≥ 80,000; minimum 97,288)
Scope: PASS applies to this set — 20 + 20 fresh consecutive runs of `src/wal/` at `0da9e14`, on `E:\waltmp`, Normal priority, Balanced plan, AC. A set of runs passing is not a guarantee about runs outside it (§11).

## 11. Historical comparison (kept separate; not part of the set, not altered)
Historical evidence is unchanged in its own documents. For context only, earlier runs of the same source on this machine include: M1.2 12,305 and 14,618; M1.3 34,059 / 35,863 / 51,860 / 61,195 / 68,067 / 76,614 / 76,737 / 78,862 (the 2026-10-04 mission's Rule A set S, M1.2 2/57 and M1.3 8/95 below threshold, which stays **FAIL for set S** in `PHASE_RUBIXDB_WAL_M12_M13_CERTIFICATION_FINAL.md`). Among those, the 61,195 run was on `C:` and the other temp locations of the older slow runs were not recorded. This campaign does not retract, explain or cancel them. The current campaign's results fall in the "fast" regime documented earlier (M1.3 101–113 k historically; here 97–109 k; M1.2 17–23 k historically; here 16.7–20.3 k).

## 12. Limitations
* The set is bounded: 20 + 20 runs in about 15 minutes on one day. It cannot show that the slow mode (historical, trigger unidentified) will not occur in another session, drive state or load condition; it shows that it did not occur in these 40 runs.
* Not a quiesced rig; Brave/Visual Studio/Task Manager open. System-wide counters only (both disks summed).
* SATA hardware (no NVMe: **HARDWARE UNAVAILABLE**); power loss **NOT TESTED**; Balanced plan, AC; M1.1 not run; per-run latency percentiles not measured; raw per-run output files not preserved (§8).
* The workspace release regression (a separate gate) was not run in this mission and keeps its earlier status (**FAIL** in `…_CERTIFICATION_FINAL.md`); it is not changed by this document.
* This document does not declare rubiXDb production ready.

## 13. Files
`scratch/wal_cert_e/`: `pre-campaign.txt`, `m12-cur-runs.csv`, `m13-cur-runs.csv`, `m12-cur-counters.csv`, `m13-cur-counters.csv` (1 Hz samples), `analysis.md`, `analyze_cert.py`.
