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
| run | start | ops/s | elapsed s | records | batches | rec/batch | window us | fsync us | coord us | cpu s | RSS MB | thr | hnd | pre clk % | clk % mean | cpu % | disk time % | disk q | wr ms | rd ms | mem avail MB | ctx/s | n | result |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 15:48:52 | 18992 | 5.265 | 100000 | 1058 | 94.5 | 456.5 | 4387.8 | 107.8 | 6.5 | 6 | 105 | 159 | 39.7 | 87 | 29.5 | 5.2 (max 8.6) | 0.10 (max 0.17) | 0.28 (max 0.45) | 0.00 | 9084 | 178591 | 6 | ok |
| 2 | 15:49:10 | 18028 | 5.547 | 100000 | 1114 | 89.8 | 472.7 | 4335.0 | 118.4 | 9.3 | 11 | 105 | 160 | 31.5 | 66 | 24.4 | 5.7 (max 15.5) | 0.11 (max 0.31) | 0.31 (max 0.72) | 0.00 | 9088 | 193987 | 7 | ok |
| 3 | 15:49:28 | 17133 | 5.837 | 100000 | 1158 | 86.4 | 475.4 | 4363.3 | 130.5 | 12.2 | 8 | 105 | 160 | 26.3 | 58 | 29.9 | 8.2 (max 21.5) | 0.16 (max 0.43) | 0.43 (max 0.99) | 0.00 | 9104 | 218799 | 7 | ok |
| 4 | 15:49:46 | 16704 | 5.987 | 100000 | 1090 | 91.7 | 478.8 | 4859.7 | 111.8 | 5.9 | 6 | 105 | 160 | 58.9 | 74 | 34.6 | 10.6 (max 27.9) | 0.21 (max 0.56) | 0.74 (max 1.97) | 0.00 | 9102 | 180101 | 7 | ok |
| 5 | 15:50:05 | 19082 | 5.241 | 100000 | 1070 | 93.5 | 459.1 | 4309.5 | 105.3 | 6.5 | 6 | 105 | 160 | 56.8 | 86 | 37.9 | 7.9 (max 21.5) | 0.16 (max 0.43) | 0.38 (max 0.99) | 0.00 | 9087 | 188550 | 6 | ok |
| 6 | 15:50:22 | 17805 | 5.616 | 100000 | 1116 | 89.6 | 475.2 | 4379.8 | 119.8 | 9.6 | 11 | 105 | 160 | 40.8 | 66 | 24.9 | 7.3 (max 19.7) | 0.15 (max 0.39) | 0.39 (max 0.90) | 0.06 | 9047 | 192118 | 7 | ok |
| 7 | 15:50:40 | 17173 | 5.823 | 100000 | 1151 | 86.9 | 485.2 | 4371.5 | 130.1 | 11.7 | 6 | 105 | 160 | 49 | 57 | 31.7 | 8.7 (max 20.0) | 0.17 (max 0.40) | 0.45 (max 0.91) | 0.00 | 9208 | 213122 | 7 | ok |
| 8 | 15:50:58 | 17402 | 5.747 | 100000 | 1139 | 87.8 | 477.8 | 4385.2 | 123.6 | 10.3 | 8 | 105 | 160 | 25.8 | 61 | 28.7 | 8.4 (max 29.0) | 0.17 (max 0.58) | 0.42 (max 1.32) | 0.00 | 9280 | 203648 | 7 | ok |
| 9 | 15:51:17 | 18285 | 5.469 | 100000 | 1113 | 89.8 | 467.1 | 4301.3 | 109.9 | 8.4 | 13 | 105 | 160 | 68 | 73 | 30.9 | 5.6 (max 13.1) | 0.11 (max 0.26) | 0.29 (max 0.58) | 0.06 | 9228 | 181184 | 7 | ok |
| 10 | 15:51:35 | 17999 | 5.556 | 100000 | 1101 | 90.8 | 473.3 | 4402.7 | 116.2 | 6.2 | 12 | 105 | 160 | 94.6 | 80 | 24.0 | 8.6 (max 21.8) | 0.17 (max 0.44) | 0.46 (max 1.01) | 0.00 | 9186 | 187624 | 7 | ok |
| 11 | 15:51:53 | 19962 | 5.009 | 100000 | 1050 | 95.2 | 436.9 | 4216.5 | 93.8 | 5.8 | 6 | 105 | 160 | 112.4 | 95 | 36.2 | 9.2 (max 26.1) | 0.18 (max 0.52) | 0.44 (max 1.17) | 0.13 | 9153 | 168923 | 6 | ok |
| 12 | 15:52:10 | 20340 | 4.916 | 100000 | 1031 | 97 | 434.2 | 4218.1 | 92.4 | 3.5 | 6 | 105 | 160 | 43.9 | 104 | 35.2 | 9.7 (max 23.4) | 0.19 (max 0.47) | 0.46 (max 1.00) | 0.00 | 9186 | 149511 | 6 | ok |
| 13 | 15:52:28 | 17389 | 5.751 | 100000 | 1097 | 91.2 | 473.7 | 4607.1 | 109.3 | 6.9 | 8 | 105 | 160 | 24.1 | 74 | 25.8 | 10.0 (max 17.9) | 0.20 (max 0.36) | 0.57 (max 0.83) | 0.00 | 9229 | 168923 | 7 | ok |
| 14 | 15:52:46 | 17219 | 5.807 | 100000 | 1148 | 87.1 | 481.2 | 4369.5 | 130.4 | 10.4 | 8 | 105 | 160 | 41.4 | 60 | 24.0 | 7.7 (max 16.5) | 0.15 (max 0.33) | 0.43 (max 0.80) | 0.00 | 9269 | 212678 | 7 | ok |
| 15 | 15:53:04 | 18440 | 5.423 | 100000 | 1089 | 91.8 | 463.1 | 4352.4 | 112.6 | 7.3 | 6 | 105 | 160 | 32.2 | 71 | 21.5 | 6.9 (max 13.8) | 0.14 (max 0.28) | 0.34 (max 0.66) | 0.00 | 9265 | 199611 | 6 | ok |
| 16 | 15:53:22 | 17670 | 5.659 | 100000 | 1141 | 87.6 | 482.3 | 4303.8 | 121.2 | 9.2 | 10 | 105 | 160 | 72.4 | 64 | 25.0 | 7.6 (max 18.0) | 0.15 (max 0.36) | 0.40 (max 0.83) | 0.06 | 9235 | 196315 | 7 | ok |
| 17 | 15:53:40 | 18210 | 5.491 | 100000 | 1095 | 91.3 | 486.0 | 4346.3 | 119.5 | 8.2 | 6 | 105 | 160 | 74.7 | 64 | 20.8 | 6.2 (max 15.3) | 0.12 (max 0.31) | 0.36 (max 0.74) | 0.00 | 9242 | 169784 | 7 | ok |
| 18 | 15:53:58 | 17923 | 5.579 | 100000 | 1099 | 91 | 497.7 | 4371.0 | 121.2 | 10.2 | 11 | 105 | 160 | 32 | 62 | 23.5 | 7.4 (max 17.8) | 0.15 (max 0.36) | 0.41 (max 0.85) | 0.00 | 9284 | 177455 | 7 | ok |
| 19 | 15:54:16 | 17803 | 5.617 | 100000 | 1115 | 89.7 | 485.9 | 4360.0 | 122.9 | 10.1 | 8 | 105 | 160 | 26 | 58 | 24.8 | 9.9 (max 21.6) | 0.20 (max 0.43) | 0.52 (max 1.03) | 0.00 | 9295 | 185280 | 7 | ok |
| 20 | 15:54:35 | 18009 | 5.553 | 100000 | 1107 | 90.3 | 494.8 | 4324.9 | 119.0 | 9.2 | 11 | 105 | 160 | 40.6 | 60 | 22.1 | 4.5 (max 10.2) | 0.09 (max 0.20) | 0.27 (max 0.49) | 0.00 | 9286 | 186586 | 7 | ok |

## 5. M1.3 — 20 consecutive runs (1,000 writers × 1,000 records = 1,000,000 records)
| run | start | ops/s | elapsed s | records | batches | rec/batch | window us | fsync us | coord us | cpu s | RSS MB | thr | hnd | pre clk % | clk % mean | cpu % | disk time % | disk q | wr ms | rd ms | mem avail MB | ctx/s | n | result |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 15:54:53 | 104120 | 9.604 | 1000000 | 1341 | 745.7 | 2187.8 | 4653.3 | 123.2 | 64.6 | 80 | 1005 | 1060 | 27.7 | 111 | 58.3 | 3.2 (max 7.7) | 0.06 (max 0.15) | 0.26 (max 0.50) | 0.04 | 9241 | 1081218 | 14 | ok |
| 2 | 15:55:19 | 97288 | 10.279 | 1000000 | 1305 | 766.3 | 2397.6 | 5081.3 | 180.2 | 65.9 | 81 | 1005 | 1060 | 27.6 | 108 | 54.6 | 4.6 (max 28.0) | 0.09 (max 0.56) | 0.55 (max 3.27) | 0.00 | 9241 | 920822 | 15 | ok |
| 3 | 15:55:46 | 102848 | 9.723 | 1000000 | 1319 | 758.2 | 2323.0 | 4709.1 | 115.9 | 65.1 | 76 | 1005 | 1060 | 30.3 | 111 | 59.8 | 4.0 (max 13.1) | 0.08 (max 0.26) | 0.34 (max 0.89) | 0.00 | 9249 | 1066402 | 14 | ok |
| 4 | 15:56:12 | 105873 | 9.445 | 1000000 | 1274 | 784.9 | 2360.4 | 4681.8 | 134.3 | 60.2 | 85 | 1005 | 1060 | 30.8 | 109 | 61.3 | 3.4 (max 9.1) | 0.07 (max 0.18) | 0.30 (max 0.59) | 0.00 | 9265 | 960778 | 14 | ok |
| 5 | 15:56:38 | 99784 | 10.022 | 1000000 | 1336 | 748.5 | 2382.0 | 4772.7 | 137.2 | 53.7 | 76 | 1005 | 1060 | 31.3 | 110 | 64.1 | 6.6 (max 54.6) | 0.13 (max 1.09) | 1.15 (max 10.86) | 0.00 | 9269 | 966266 | 15 | ok |
| 6 | 15:57:05 | 99955 | 10.004 | 1000000 | 1316 | 759.9 | 2456.2 | 4810.2 | 117.2 | 53.6 | 74 | 1005 | 1060 | 32.4 | 112 | 65.6 | 3.6 (max 10.3) | 0.07 (max 0.21) | 0.32 (max 0.68) | 0.00 | 9266 | 1065823 | 14 | ok |
| 7 | 15:57:31 | 103149 | 9.695 | 1000000 | 1305 | 766.3 | 2257.1 | 4796.6 | 150.6 | 53.1 | 77 | 1005 | 1060 | 48.9 | 111 | 65.3 | 4.1 (max 9.7) | 0.08 (max 0.19) | 0.49 (max 1.15) | 0.39 | 9262 | 993923 | 14 | ok |
| 8 | 15:57:57 | 101891 | 9.814 | 1000000 | 1401 | 713.8 | 1999.8 | 4659.4 | 152.2 | 53.3 | 75 | 1005 | 1060 | 71.3 | 112 | 62.6 | 4.2 (max 9.9) | 0.08 (max 0.20) | 0.32 (max 0.47) | 0.13 | 9186 | 1067718 | 14 | ok |
| 9 | 15:58:23 | 103371 | 9.674 | 1000000 | 1340 | 746.3 | 2160.9 | 4709.7 | 138.6 | 61.9 | 78 | 1005 | 1060 | 25.8 | 111 | 59.1 | 4.1 (max 11.6) | 0.08 (max 0.23) | 0.35 (max 0.84) | 0.02 | 9192 | 1026159 | 14 | ok |
| 10 | 15:58:49 | 103271 | 9.683 | 1000000 | 1332 | 750.8 | 2201.7 | 4702.3 | 144.1 | 62.2 | 78 | 1005 | 1060 | 28.5 | 111 | 58.3 | 3.3 (max 7.8) | 0.07 (max 0.16) | 0.27 (max 0.46) | 0.00 | 9210 | 1008215 | 14 | ok |
| 11 | 15:59:15 | 102761 | 9.731 | 1000000 | 1366 | 732.1 | 2162.1 | 4647.3 | 127.2 | 61.1 | 73 | 1005 | 1060 | 39.1 | 109 | 56.7 | 4.3 (max 21.8) | 0.09 (max 0.44) | 0.41 (max 1.37) | 0.07 | 9225 | 1042629 | 15 | ok |
| 12 | 15:59:42 | 109019 | 9.173 | 1000000 | 1272 | 786.2 | 2264.2 | 4608.8 | 120.9 | 58.1 | 89 | 1005 | 1060 | 31.7 | 109 | 53.3 | 3.2 (max 9.3) | 0.06 (max 0.19) | 0.26 (max 0.61) | 0.00 | 9222 | 942853 | 14 | ok |
| 13 | 16:00:08 | 102718 | 9.735 | 1000000 | 1367 | 731.5 | 2152.3 | 4625.0 | 142.2 | 58 | 79 | 1005 | 1060 | 32.9 | 110 | 55.8 | 3.1 (max 8.8) | 0.06 (max 0.18) | 0.28 (max 0.58) | 0.00 | 9221 | 1069933 | 14 | ok |
| 14 | 16:00:34 | 100333 | 9.967 | 1000000 | 1347 | 742.4 | 2122.3 | 4959.9 | 123.2 | 61.1 | 89 | 1005 | 1060 | 40.2 | 108 | 55.2 | 5.2 (max 23.8) | 0.10 (max 0.48) | 0.54 (max 2.97) | 0.00 | 9221 | 1047757 | 15 | ok |
| 15 | 16:01:00 | 98664 | 10.135 | 1000000 | 1366 | 732.1 | 2264.4 | 4803.3 | 149.5 | 65.9 | 75 | 1005 | 1060 | 29.5 | 110 | 59.3 | 2.9 (max 6.6) | 0.06 (max 0.13) | 0.32 (max 0.66) | 0.00 | 9213 | 998779 | 15 | ok |
| 16 | 16:01:28 | 100995 | 9.902 | 1000000 | 1360 | 735.3 | 2246.3 | 4690.4 | 139.7 | 64.1 | 74 | 1005 | 1060 | 25.7 | 108 | 56.8 | 3.3 (max 7.8) | 0.07 (max 0.16) | 0.74 (max 3.94) | 0.00 | 9217 | 994018 | 15 | ok |
| 17 | 16:01:55 | 102035 | 9.801 | 1000000 | 1381 | 724.1 | 2054.8 | 4731.6 | 116.1 | 63.5 | 77 | 1005 | 1060 | 34.5 | 111 | 60.0 | 4.0 (max 9.3) | 0.08 (max 0.19) | 0.32 (max 0.63) | 0.08 | 9214 | 1144215 | 14 | ok |
| 18 | 16:02:21 | 106297 | 9.408 | 1000000 | 1332 | 750.8 | 2089.9 | 4643.9 | 122.6 | 60.6 | 82 | 1005 | 1060 | 47.1 | 111 | 58.6 | 4.0 (max 10.9) | 0.08 (max 0.22) | 0.37 (max 0.92) | 0.00 | 9184 | 1097834 | 11 | ok |
| 19 | 16:02:46 | 98977 | 10.103 | 1000000 | 1401 | 713.8 | 2204.7 | 4652.6 | 173.6 | 52.8 | 89 | 1005 | 1060 | 31.8 | 108 | 65.6 | 3.7 (max 14.6) | 0.07 (max 0.29) | 0.32 (max 0.93) | 0.00 | 9168 | 1038720 | 15 | ok |
| 20 | 16:03:13 | 97674 | 10.238 | 1000000 | 1346 | 742.9 | 2306.4 | 4898.9 | 182.0 | 55.4 | 75 | 1005 | 1060 | 35.8 | 111 | 68.5 | 4.4 (max 14.1) | 0.09 (max 0.28) | 0.70 (max 4.80) | 0.00 | 9180 | 963631 | 14 | ok |

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
