

---
# Update 2026-10-04 (second section) — Rule A applied; workspace M1.3 regression investigated
*The sections above are the earlier, historical statement of this document and are not edited. Where this section differs, it supersedes them (see "Supersession").*

## A. Acceptance Rule (maintainer-supplied, applied exactly)
**RULE A:** every run included in the certification set must individually satisfy the machine-enforced threshold (M1.2 ≥ 15,000 ops/s; M1.3 ≥ 80,000 ops/s). No outlier tolerance, median, percentile or K-of-N. One included run below threshold = that gate FAIL for that set. A run is excluded only if the certification procedure itself defines it invalid for a documented technical reason; no invalid-run rule was invented after seeing results.
**What Rule A does not define (stated, not filled in):** the *boundary* of a certification set. I therefore used the most inclusive set that has run-level data in the repository, excluding nothing for being low.
**Set S (all runs of the current WAL source with run-level values on file).** `git diff 3ec5034 HEAD -- src/wal` is empty, so `src/wal/` is identical for every run below (the diagnosis runs, the P1 runs and HEAD `0da9e14`; the 10-03/10-04 production-operations runs post-date `3ec5034`, 2026-10-03 20:53). One scenario per process, release, `--test-threads=1` or the equivalent test-binary invocation.
| Component of S | M1.2 runs | M1.3 runs | Below threshold |
|---|---|---|---|
| `scratch/prod_ops/wal_certify_3runs.txt` (normative command) | 3 | 3 | none |
| `scratch/prod_ops/wal_certify_10runs.txt` (normative command, `failed=5`) | 10 | 10 | M1.2: **12,305**; M1.3: **68,067; 34,059; 35,863; 51,860; 76,737** |
| `scratch/prod_ops/wal_ab_interleaved.txt` (current runs of the A/B) | 6 | 6 | none |
| `scratch/prod_ops/m13_runs_with_sampler.txt` | – | 6 | M1.3: **78,862; 76,614** |
| Diagnosis, 2026-10-04 (`scratch/wal_diag/{p1,h5,h7,prio}-cur-runs.csv`) | 38 | 58 | M1.2: **14,618** |
| This mission, TEMP comparison (`p1tmp-*`, 6 on `C:`, 6 on `E:\waltmp`) | – | 12 | M1.3: **61,195** (`C:` run 3) |
| **Total in S** | **57** | **95** | **M1.2: 2; M1.3: 8** |
**Excluded from S and why (nothing excluded for being low):** (1) the earlier aggregate tables ("18/18", 9 + 9 runs; only min/median/max on file, and the code identity before the last constant change is not confirmed): all stated ≥ target, so including them cannot change a Rule A result; (2) full-workspace runs (§C), which are a separate gate per closure §1 and are accounted there; (3) baseline `7b7aaaa` runs (different code).
**Discrepancy found in the historical record:** `wal_certify_10runs.txt` contains M1.3 values of 34,059 / 35,863 / 51,860, which are lower than the "slow mode 61–79 k" band stated in the closure addendum. The addendum under-describes its own raw file. The cause of those three runs is not recorded (it was a production-operations session with other campaigns possibly active); no documented invalid-run rule applies, so under Rule A they stay in S.

## B. Classification under Rule A
**M1.2 = FAIL** (for set S): 2 of 57 included runs are below 15,000 — 12,305 (historical, `wal_certify_10runs.txt`; it is the "12.3 k outlier" of the closure addendum) and 14,618 (diagnosis, 2026-10-04). 55 of 57 met the threshold.
**M1.3 = FAIL** (for set S): 8 of 95 included runs are below 80,000 — 34,059; 35,863; 51,860; 61,195 (new, this mission); 68,067; 76,614; 76,737; 78,862. 87 of 95 met the threshold, including **all 58 of the 2026-10-04 diagnosis runs**.
Neither result says "a performance defect was found in WAL code". Per Rule A they say that S contains runs that individually missed the machine-enforced threshold; this is a project-policy application, and no individual failing invocation is described as having passed.
*Informational only (not a classification):* the 2026-10-04 diagnosis session alone gives M1.2 FAIL (14,618 of 38) and M1.3 PASS (58/58). Rule A does not define whether sessions are separate sets; if the maintainer defines a fresh set boundary, the status would be re-evaluated on that set. A run that "passed" is evidence for that run only.

## C. Workspace regression (separate gate; not made identical to the normative command)
**Where the workspace test writes.** `tests/group_commit/support.rs::temp_dir` = `std::env::temp_dir().join("rubixdb_group_commit_it_<tag>_…")`, i.e. `%TMP%`/`%TEMP%`. Default here: `C:\Users\Ruby\AppData\Local\Temp` (Disk 1, a different SATA SSD, 89 % full). The normative runs of earlier phases set `TMP=TEMP=E:\waltmp` (Disk 0). The environment of the three recorded workspace failures is not recorded.
**Recorded history of the gate (unchanged):** release workspace, M1.3 failed in all three recorded runs (61,202; 35,531 with a stray server process from a failing test; 48,298; the last with 1,192 / 1 / 26).
**Measured in this mission (two complete `cargo test --release --workspace --no-fail-fast`, 1 Hz sampler of per-disk write latency / busy, CPU, clock, presence of `group_commit` and of other `rubixdb|python|node|chrom*` processes; raw: `scratch/wal_diag/ws-*`):**
| Run | TMP | Result | M1.2 / M1.3 | Notes |
|---|---|---|---|---|
| ws-default (15:17, 662 s) | default (`C:`) | **1,193 passed / 0 failed / 26 ignored** | both `ok` (values not printed for passing tests) | `group_commit` binary 8/8; M1.3 phase ≈ 10 s CPU-busy (consistent with a FAST run); no other watched process present |
| ws-etmp (15:28, 610 s) | `E:\waltmp` | **1,192 passed / 1 failed / 26 ignored** | both `ok` | the failure is `concurrent_first_run_processes_race_safely_to_one_owner` (`cli/tests/gui_instance_integration.rs:153`, message `a: ` empty), which passed in ws-default; **not a WAL test**, not investigated (out of scope, logged to `OPEN_ITEMS.md`) |
**Standalone P1 test (`C:` default TEMP vs `E:\waltmp`, the same test binary, workload, power plan, Normal priority, strictly interleaved, 6 + 6 M1.3 runs):**
| | ops/s | note |
|---|---|---|
| `C:` | 107,899 / 116,216 / **61,195** / 114,174 / 113,297 / 112,205 | run 3: mean fsync stage 9,859 µs (others ≈ 4,000–4,090), device `Avg. Disk sec/Write` mean **10.4 ms** (max 33 ms) vs 0.3–0.5 ms in the other `C:` runs, wall 23 s |
| `E:` | 92,055 / 105,943 / 102,052 / 101,645 / 102,177 / 102,920 | fsync stage 4,560–5,616 µs; one run (92,055) with fsync 5.6 ms |
Observation: in the single slow run the time went into the device's write/flush latency (counter and stage timing agree), not CPU (system CPU 45 % vs 56–65 %) or context switches (0.73 M vs 0.92–0.98 M/s). `C:` was faster than `E:` in 5 of 6 runs.
**P2 (workspace harness), by inspection and measurement.** The binary-wide `RwLock` is process-local; `crash_consistency` spawns child processes of the same test executable but its parent holds the shared lock until they finish, so M1.3 cannot start while children run (sampler: `group_commit` process count falls to 1 before the M1.3 CPU burst, other watched processes = 0 throughout). Cargo runs test *binaries* sequentially; the preceding binaries write heavily (lib unit tests ≈ 204 s, then `crash_consistency`, which showed `C:` write latency up to 85 ms in ws-default) — device state at the start of `group_commit` is therefore different from an isolated run. No overlap of throughput tests with other tests, no leftover process, no TEMP collision was observed. These observations explain nothing about the recorded failures, because those runs have no per-stage or counter data and did not recur.
**Classification of the recorded workspace M1.3 failure: UNRESOLVED.** P1 (TEMP location) is **not confirmed**: both TEMP settings passed the workspace M1.3 in this mission and a slow standalone run occurred on `C:` in 1 of 6 (against 0 of 6 on `E:`, a difference this small cannot be called significant). P2 found no harness defect. P3: the only mechanism measured for a slow M1.3 run is device write-latency inflation (single observation, direct counter and stage evidence); that it also caused the recorded workspace failures is not shown. "Environmental" is **not** claimed.
**FULL RELEASE REGRESSION = FAIL.** Not exempted. Evidence: 3 recorded runs failed `m1_3`; of the two runs in this mission one was fully clean (1,193 / 0 / 26) and one failed a different, non-WAL test. The gate is not met by a single clean run; no later series of clean runs exists.

## D. Unchanged gates
**POWER LOSS = NOT TESTED** (no physical power-loss test exists; process kill — 1.47 M oracle acks, 0 lost — is not power loss). **NVMe = HARDWARE UNAVAILABLE** (both disks SATA; no NVMe result is claimed).

## E. Overall
* **WAL certification: FAIL** — M1.2 FAIL and M1.3 FAIL under Rule A for set S; full release regression FAIL; power loss NOT TESTED. (Correctness evidence — ack durability, atomicity, ordering, recovery, crash consistency under process kill — is unchanged and was not re-run.)
* **rubiXDb production readiness: NOT PRODUCTION READY** (mandatory gates FAIL / NOT TESTED). This mission certifies only the WAL status.
* **Not resolved:** the trigger of slow M1.3/M1.2 runs; the cause of the recorded workspace failures; the boundary of a certification set; whether `docs/PROJECT_STATE.md` / `missions/ACTIVE.md` (required by `CLAUDE.md`, absent from the repository) will be supplied — recorded OPEN.

## F. Supersession (additive; earlier text stays in place)
| Earlier statement | Now |
|---|---|
| §6 above: "M1.2 = OPEN" and "M1.3 = OPEN" (no acceptance policy) | **superseded:** Rule A supplied → M1.2 FAIL, M1.3 FAIL for set S |
| §6 above: "FULL RELEASE REGRESSION = FAIL" | reaffirmed, with new runs (§C) |
| §6 above: "WAL CERTIFICATION CLOSURE = OPEN" | **superseded:** WAL certification = FAIL |
| §1/§6 above: "acceptance policy undefined" | superseded for the per-run requirement (Rule A); the set-boundary question remains open |
| Closure addendum band "slow mode 61–79 k" | preserved as history; the raw file also holds 34–52 k runs (see §A); not rewritten |
| "not reproduced" (diagnosis) | still true for the 58 diagnosis runs; a slow run (61,195) *was* observed on 2026-10-04 in this mission's TEMP test, so the slow mode is not absent from the current code on this machine; its trigger remains unidentified |
