# Write-Engine Realistic Soak — Storage Budget & Provisioning Check (E:)

**Date:** 2026-09-19
**Purpose:** Pre-flight check before re-running the realistic full-pipeline soak (`examples/realistic_full_pipeline_soak.rs`, 200 writers, 14,400s, `LsmConfig::default()`) that failed on 2026-09-19 08:51 with ENOSPC (`PHASE5_ENOSPC_FAILURE_ANALYSIS.md`) — now that `ADR-WE-SP-001` (`PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md`) has fixed the underlying retry/backpressure defect, this check answers a *separate* question: does the target volume have enough real headroom that the re-run tests "does the fix work," not "does this volume have enough space."

---

## 1. E: volume measurement

Measured directly (`Get-Volume -DriveLetter E`, `Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='E:'"`), no data modified or deleted:

| | |
|---|---|
| Filesystem | NTFS |
| Health status | Healthy |
| Operational status | OK |
| Allocation unit size | 4,096 bytes |
| Total capacity | 127,355,359,232 bytes ≈ **118.61 GB** |
| Free space | 101,644,873,728 bytes ≈ **94.66 GB** (at measurement time) |
| Used space | ≈ 23.94 GB |

`E:\RubiXDb\temp` (this project's existing scratch area on this same volume) currently holds ≈244 MB total, entirely prior harness logs/scripts (`temp/long_soak_logs` ≈615 KB, `temp/rgc_bench`, `temp/soak_logs`, `temp/system-commandline-sentinel-files`) — no leftover RubiXDB database directories found elsewhere on `E:`. Nothing was modified or deleted by this check.

## 2. Estimated soak storage requirement

Derived from the **healthy-period** portion of the failed 2026-09-19 08:51 run (before ENOSPC onset at t≈5,100s), not the full failed run (which stalled and is not representative of undisturbed growth):

- SSTable count grew 0 → 1,293 over the first 4,812.5s of healthy operation → **0.26868 SSTables/sec**.
- Projected over the full undisturbed 14,400s target duration: 0.26868 × 14,400 ≈ **3,869 SSTables**.
- Per-SSTable size: `LsmConfig::default().memtable_max_size_bytes` = 4 MiB is the memtable's *logical* accounting cap (key+value+`ENTRY_OVERHEAD` per entry); the actual on-disk RUBIC SSTable additionally carries block headers, a sparse index, and a Bloom filter (~10 bits/key default). Conservatively padded **+15%** over the 4 MiB logical cap → **4.6 MiB/table** used for this estimate (this is a padding factor applied to a known configuration constant, not a measurement of an actual produced file — the smoke test in §5 below cross-checks it against a real measured file size before the full soak launches).

| Component | Estimate | Basis |
|---|---|---|
| SSTables | 3,869 × 4.6 MiB ≈ **17.38 GB** | Measured healthy-period growth rate × padded per-table size |
| WAL | 0.05 GB | Observed max 13.5 MB in the failed run even *during* the ENOSPC stall (bounded by group-commit + periodic purge); padded generously |
| Manifest | 0.01 GB | Observed 103,674 bytes at 1,401 tables in the failed run; scales linearly and stays tiny (no compaction, but also no per-table overhead beyond one `AddSstable` edit) |
| Temp/flush workspace (`.sst.tmp`) | 0.05 GB | Bounded by at most one in-flight SSTable write at a time |
| Test logs (harness stdout/stderr) | 0.05 GB | A healthy run's stderr should be near-empty (no ENOSPC to log); padded for a stray non-fatal retry burst |
| **Subtotal** | **17.54 GB** | Sum of the above |
| Filesystem overhead (+2%) | 17.89 GB | NTFS allocation-unit rounding, MFT growth |
| **Safety margin (×2)** | **35.78 GB** | Deliberately substantial, per this check's own §3 requirement — not a bare "free > estimate" comparison |

This is an order-of-magnitude planning estimate derived from a real measured growth rate, not a contractual figure — stated explicitly, same as `PHASE5_ENOSPC_FAILURE_ANALYSIS.md` §7's original estimate (10–15 GB unpadded, which this more careful calculation is consistent with: 17.38 GB padded-per-table vs. that estimate's simpler 4 MiB/table × 3,900 tables ≈ 15.2 GB).

## 3. Headroom

| | |
|---|---|
| Estimated required space (with 2× safety margin) | **35.78 GB** |
| Available `E:` free space | **94.66 GB** |
| Headroom | **58.88 GB** |
| Headroom percentage | **164.6%** of the padded requirement |

**Conclusion: sufficient headroom. Proceed.** Free space exceeds the padded, safety-margined estimate by more than 2.6×; even the unpadded, no-safety-margin subtotal (17.54 GB) alone would leave 77+ GB free. No workload reduction, no silent scope change.

## 4. Directory location fix

Traced `examples/realistic_full_pipeline_soak.rs`'s directory selection: `temp_dir()` (the example's own helper, not `wal`/`lsm` module code) called `std::env::temp_dir()` unconditionally — the Windows default `%TEMP%`, which on this machine resolves to the chronically ~97%-full `C:` drive (`FINAL_WAL_ANALYSIS.md` §5). This is a **test-fixture** path-selection choice, not a production storage-semantics decision — `LsmEngine::open` itself already takes an arbitrary `&Path` from its caller and has no opinion about which volume that path lives on.

**Smallest safe fix applied** (`examples/realistic_full_pipeline_soak.rs`): a new `soak_base_dir()` helper checks the `RUBIXDB_SOAK_BASE_DIR` environment variable first, falling back to the previous `std::env::temp_dir()` default when unset — so every other caller of this example (and the example's own default behavior when run without the variable) is unchanged. The run now also prints the canonicalized resolved directory and which source (`RUBIXDB_SOAK_BASE_DIR` vs. the OS default) supplied it, at startup, before any engine I/O.

The corrected harness (`temp/realistic_soak_harness.ps1`) sets `RUBIXDB_SOAK_BASE_DIR = E:\RubiXDb\temp\soak_data` before launching the soak process.

Verification that this actually lands the database on `E:` (WAL/SSTable/Manifest, not just the top-level directory) is §5/§6 below, performed with a real smoke-test run before the 4-hour soak launches.
