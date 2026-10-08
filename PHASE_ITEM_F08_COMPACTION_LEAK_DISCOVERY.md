# PHASE ITEM F-08 COMPACTION LEAK — DISCOVERY: a failed compaction retries forever, leaks a `.sst.tmp` every time and reports nothing

**Date:** 2026-10-08. **Mission:** this finding only; discovery and ADR, no engine behaviour changed, nothing under `src/lsm/`, `src/compaction/`, `src/sstable/`, `src/manifest/`, `src/wal/` or `src/error.rs` touched. Companion document: `PHASE_ITEM_F08_COMPACTION_LEAK_ADR.md` (ADR-COMPACTION-LEAK-01, PROPOSED). Found by the F-08 mission (`PHASE_ITEM_F08_DISCOVERY.md` section 4.5 and 9.1; `OPEN_ITEMS.md` 2026-10-08).

## 1. Identity

| Item | Value |
|---|---|
| Repository | branch `master`, `git status --short` empty, HEAD `13dc534adb71fce4447ab2caea0843c9ea743cb3` |
| `git log -3 --oneline` | `13dc534` F-08: damaged SSTable -- discovery and ADR-SST-01 (PROPOSED); no code changed / `03295e0` docs: PROJECT_STATE -- remove F-07 from the blockers list / `8fdab76` F-07: implement ADR-WAL-01 (P4) |
| Binary | `cargo build --release --locked -p rubixdb-cli` -> `E:\rubixdb_f08_compaction\rubixdb_leak_base.exe`, SHA-256 `dfa2b0fcae842bae89ee3feb1cf8dfa6ecc9d6c89a4cb9b823210a78e0f81f73` (a rebuild of the same source as the F-08 binary; the reported revision is the new HEAD) |
| Fixture | the F-08 three-table fixture `E:\rubixdb_f08\runs\template3` (copied per run): table `t`, 60,000 rows of ~200-byte values, **3 live SSTables published by real flushes** (4,030,612 / 4,030,165 / 4,030,165 bytes, 956 data blocks each), WAL segment 1 purged, compaction trigger = 4 tables, not yet compacted away |
| Host | Windows 10 Home, release build, loopback, one instance at a time on port 302; the data volume had about 4.2 GB free, so observations are capped at 125 s |
| Engine-layer probe | one added example in an unmodified `git archive` copy of the F-08 tree (`E:\rubixdb_f08\tree\examples\f08c_open_hold.rs`); the repository was not modified |

## 2. The finding

With one flipped bit in a data block of a live SSTable, every compaction attempt fails, is retried on a fixed timer, leaves a partial `*.sst.tmp` behind each time, and is reported nowhere: `cycles_completed` stays 0, no failure counter or state exists, `storage_state` stays `Healthy`, `/readyz` stays `ready: true`. The only trace is one stderr line per attempt. The pile is reclaimed only by the **next start**.

## 3. The code paths that decide failure, cleanup, retry and reporting

Line numbers are for `13dc534`. Nothing here was modified.

| # | Question | Where | What it does |
|---|---|---|---|
| 1 | **Where is the failure detected?** | `src/compaction/mod.rs:209-218` (the k-way merge iterator), `src/sstable/reader.rs:195-205` + `src/sstable/format.rs:173-178` (the block read), `src/sstable/writer.rs:130` | an input cursor yields `Err(Corruption "block: checksum mismatch")`; the merge "fails closed immediately ... never continue[s] past it" and returns the `Err`; the writer's loop `let (key, seq, value) = item?;` returns it. The test of this contract is `src/compaction/tests.rs:296-330` (the merge stops and surfaces an `Err`). |
| 2 | **Where is the tmp file created?** | `src/sstable/writer.rs:107-115` | `OpenOptions::new().create(true).write(true).truncate(true).open(&tmp_path)` where `tmp_path = <dir>/<id:020>.sst.tmp` (`src/sstable/mod.rs:73-75`). The tmp file is consumed only by `fs::rename(&tmp_path, &final_path)` at `writer.rs:195`. |
| 3 | **Where is it removed on failure?** | **nowhere** | `writer.rs` contains no `remove_file`. Every `?` between the open (:115) and the rename (:195) returns with the partial file on disk: `item?` (:130), `encode_record(...)?` (:135), the block writes (:151, :164), the bloom / index / footer writes (:169, :175, :190), `sync_all` (:192), `rename` (:195), `fsync_dir` (:196). |
| 4 | **How does an error reach the worker?** | `src/lsm/mod.rs:2726-2732` | `compact_once_impl` calls `sstable::write_from_sorted_records(...)?`: the `Err` propagates, the `CompactionRunGuard` is released by RAII, **no cleanup of the output id's tmp file is attempted there either**. |
| 5 | **What does the worker do with the error?** | `src/lsm/mod.rs:2901-2960` (`catch_unwind` around `compact_once_impl`; `Ok(Err(e)) => { eprintln!("compaction: failed, will retry on the next trigger: {e}"); break; }` at :2949-2952; a panic prints `compaction: panicked, will retry ...` and breaks) | **prints one line to stderr, nothing else.** It does not classify the error, move `storage_state`, count the failure, remember it, stop retrying, or mark the source table. Success prints to **stdout** (`:2924-2937`); failure is on stderr only. |
| 6 | **Metrics** | `src/lsm/mod.rs:581-612`, `:614-640` (`record_compaction_cycle`: "a failed/deferred/no-op cycle contributes nothing"), `:2823` | `CompactionMetrics` has `cycles_completed`, byte / record totals, `duration_*`, `peak_temp_disk_bytes_max`, `last_cycle`. There is **no failure field of any kind**; the doc comment's premise is that "a failure leaves no observable partial state" (ADR-COMPACTION-001 §11). That premise is false for the tmp file. |
| 7 | **Storage state** | `src/lsm/mod.rs:166-195` (`StorageState`: `Healthy`, `StoragePressure`, `StorageFull`; `from_u8` maps every other value to `StorageFull`), `:2672-2675` ("Decision 11: observe only, never mutate `storage_state`"), `:3182` (`is_storage_exhausted()` is used **only** by the flush thread) | compaction reads the state (any value other than `Healthy` -> `Ok(None)`, compaction deferred) and never writes it. An ENOSPC during compaction is an ordinary `Err` like a checksum failure. |
| 8 | **Where is the cadence set?** | `src/lsm/mod.rs:148` (`storage_pressure_retry_interval: Duration::from_secs(5)`), `:1472` (passed to the worker as `fallback_interval`), `:2874` (`receiver.recv_timeout(fallback_interval)`), the worker's doc comment `:2838-2853` ("reusing `storage_pressure_retry_interval` as the cadence — no new config field"; "On `Err`, logs once ... no internal busy-retry, no artificial timer beyond the existing periodic fallback tick") ; `ADR-COMPACTION-001` Increment 2 §8/A2 (`PHASE_COMPACTION_ADR.md:1054-1072`) | a fixed 5 s timer **plus** an immediate extra attempt after each flush (`CompactionMsg::MaybeCompact`, `:260-267`, sent after a table is published). A damaged-table failure is **not distinguished** from disk-full or any other error: one arm handles all. ADR-WE-SP-001's classification and slower cadence exist only on the flush path. |
| 9 | **Is each attempt's id new?** | `src/lsm/mod.rs:2720` (`next_sstable_id.fetch_add(1)` per attempt); flush: `:3040` (per attempt too) | every failed attempt takes a new id, so its tmp file is never overwritten by the next one (an id reused with `truncate(true)` would have bounded the leak to one file). |
| 10 | **The orphan sweep** | `src/lsm/mod.rs:2482-2485` (`reconcile_sstables_with_manifest`, step "6a" of engine open: every `*.sst.tmp` is deleted); `src/sstable/mod.rs:112-117` (`discover`); no tmp handling in `shutdown()`; `src/ops/check.rs:893-896` (the offline check lists leftovers as **Info** `UNEXPECTED_FILE` "interrupted build output; removed at startup") | the sweep **runs once, at engine open**. It is why a crash can never leave a tmp file behind a restart; it is not why a running process does not accumulate them: nothing runs it between attempts or at shutdown. Tests assert only "after recovery / reopen no tmp survives": `src/lsm/tests.rs:4267-4276`, `:5056-5057` (asserted **after the reopen**), `:5321-5322`, `src/sstable/tests.rs:354-366`, `examples/compaction_crash_cycle_test.rs:257`, `examples/sstable_flush_crash_test.rs`. |
| 11 | **The contract that was promised** | `PHASE_COMPACTION_ADR.md:752-754` (test plan: "corrupt input (fail closed, **no partial output**)"), `src/lsm/mod.rs:614-619` | "no partial output" is verified only at the merge level (`compaction/tests.rs:296`); no test drives `compact_once_impl` over a corrupt input and then lists the directory. |
| 12 | **What the product shows** | `api/src/routes/compaction.rs`, `api/src/routes/admin.rs`, `api/src/observability/sampler.rs` read `compaction_metrics()` / the table count | `/v1/compaction/status`, `/v1/compaction/metrics`, `/v1/admin/status.compaction`, `/v1/metrics/system.compaction`: counters of successful cycles and `live_sstable_count`; nothing about failure. `disk.sstable_bytes` (the sampled size of the `sstables` directory **including** `.tmp` files) is the only number that moves. |

## 4. Reproducer

### 4.1 Method

`E:\rubixdb_f08_compaction\scripts\leak_repro.py` (cases A, B, C), `leak_restart.py` (next start and worker-disabled), `make_leak_tables.py`; helper library `E:\rubixdb_f08\scripts\f08_lib.py`. Real release `rubixdb gui`, black box, a fresh copy of the three-table fixture per case. **Damage:** one bit flipped in data block 478 of 956 of SSTable 1 (the F-08 scenario `m7`; ids 8603-8620). The server is started (it opens: data blocks are not checked at open). **Trigger:** 24,000 more rows (~4.3 MB) are inserted so a fourth table is flushed and the compaction trigger (4 tables) is reached. **t = 0** is the file-system *creation time* of the first leftover tmp file (the first failed attempt). A watcher thread polls the `sstables` directory every 0.2 s and records every tmp file's creation time, last-write time and size, so the cadence comes from file-system timestamps, not from the sampler's clock. At t = 0, 30, 60, 120 and 125 s the sampler records the tmp files and exact sizes, the live tables, `/v1/compaction/status`, `/v1/compaction/metrics`, `/v1/status`, `/v1/metrics/system`, `/v1/admin/status`, `/readyz`, stderr and `security.log`. Case A: no writes after the trigger. Case B: ~4 MB worth of rows inserted every 25 s (a flush each). Case C: damage in the **last** block of the **last** table (to measure what one attempt writes when the damage is late in the merge; observed 20 s). Raw: `E:\rubixdb_f08_compaction\raw\case_*\result.json`, `raw\restart\result.json`.

### 4.2 Timeline, case A (no writes after the trigger)

| t (s since the first failure) | `.sst.tmp` files | bytes in them (exact) | live tables | `cycles_completed` (`/v1/compaction/metrics`) | `/v1/status` `storage_state` / `/readyz` `ready` | `disk.sstable_bytes` (`/v1/admin/status`) | stderr `compaction: failed` lines (cumulative) | security.log lines |
|---|---|---|---|---|---|---|---|---|
| 0 | 1 | 1,991,320 | 4 | 0 | Healthy / true | 18,112,427 | 1 | 5 |
| 30 | 6 | 11,947,920 | 4 | 0 | Healthy / true | 28,069,027 | 6 | 5 |
| 60 | 12 | 23,895,840 | 4 | 0 | Healthy / true | 40,016,947 | 12 | 5 |
| 120 | 24 | 47,791,680 | 4 | 0 | Healthy / true | 63,912,787 | 24 | 5 |
| 125 | 25 | 49,783,000 | 4 | 0 | Healthy / true | 65,904,107 | 25 | 5 |


**(a) Cadence, from the file-system timestamps (25 attempts):** the gap between consecutive attempts is **5.022 - 5.066 s, mean 5.048 s** (24 gaps) — exactly `storage_pressure_retry_interval` (5 s) plus the attempt's own duration. Each attempt lasts **1-50 ms (median 34 ms)** from the creation to the last write of its tmp file.

**(b) Files and sizes:** one new `NNNN.sst.tmp` per attempt, every one **exactly 1,991,320 bytes** (the data blocks written before the merge reached the damaged block; no bloom block, index or footer): 1 / 6 / 12 / 24 files at t = 0 / 30 / 60 / 120 s, 25 at 125 s = **49,783,000 bytes**. Table ids 5-29 were consumed by the failed attempts; none was ever published.

**(c) `CompactionMetrics`** (`/v1/compaction/metrics`) at every sample: `cycles_completed: 0`, every total 0, `last_cycle: null`. `/v1/compaction/status`: `auto_trigger_enabled: true`, `trigger_count: 4`, `live_sstable_count: 4`, `cycles_completed: 0`. There is no field in which a failure could appear.

**(d) The other surfaces:** `/v1/status`: `storage_state: Healthy`, `storage_pressure_events: 0`, `sstable_count: 4`; `/readyz`: `ready: true`; `/v1/metrics/system`: `compaction {cycles_since_start: 0, running: false, live_sstable_count: 4}`, `errors.http_server_errors_since_start: 0` (no query touched the block); `/v1/admin/status`: `compaction.cycles_completed: 0`. **The only number that moves is `disk.sstable_bytes`** (the size of the `sstables` directory, tmp files included): 18.1 -> 28.1 -> 40.0 -> 63.9 -> 65.9 MB while `live_sstable_count` stays 4, and `volume_free_bytes` falls in step; nothing labels it.

**(e) stderr and security.log:** stderr: exactly one line per attempt, always `compaction: failed, will retry on the next trigger: corruption: block: checksum mismatch` (25 lines at t = 125 s); it names no table, no block and no file. stdout: no failure line. `security.log`: 5 lines at every sample, none about compaction (`instance.start`, `catalog.create`, `instance.stop`, ...).

**(f) Is the pile reclaimed?** **Not at a clean shutdown:** before the graceful stop 25 files, after it 25 files, 49,783,000 bytes. **At the next start, yes:** the engine's open sweeps them all (0 files when the instance answers `/healthz`, `leak_restart.py` f2) — and **the leak resumes**: the first new tmp file appears about 5 s after the start (the first periodic tick), a second at about 10 s, exactly as before (`raw\restart\result.json`; the new files reuse ids 5, 6, ... because the engine recomputes the next id from what is on disk).

**(g) With the automatic worker disabled** (`compaction_auto_trigger: false`, the setting `ops::open` uses for `rubixdb check` / restore): (g1) offline `rubixdb check --data-dir` on a copy of the 25-file directory: **the pile is left in place** (25 files after the command): the damaged data block is a physical error, so `check` skips its logical pass and never opens the engine; it lists each file as Info `UNEXPECTED_FILE ... interrupted build output; removed at startup` and exits 2 for the damaged block. (g2) the bare engine opened with the worker disabled and held 20 s (`f08c_open_hold`): **the open sweeps the pile (25 -> 0 files in 93 ms), nothing regrows over 20 s, `cycles_completed` stays 0, 0 files after shutdown**. So the leak exists only while the automatic worker runs; a manual `compact_once` (not callable from outside the crate, `pub(crate)`) goes through the same `compact_once_impl` and, by the code path above, leaks the same way.

### 4.3 Case B (writes continue: ~4 MB of rows every 25 s)

| t (s since the first failure) | `.sst.tmp` files | bytes in them (exact) | live tables | `cycles_completed` (`/v1/compaction/metrics`) | `/v1/status` `storage_state` / `/readyz` `ready` | `disk.sstable_bytes` (`/v1/admin/status`) | stderr `compaction: failed` lines (cumulative) | security.log lines |
|---|---|---|---|---|---|---|---|---|
| 0 | 1 | 1,991,320 | 4 | 0 | Healthy / true | 18,112,427 | 1 | 5 |
| 30 | 7 | 13,939,240 | 5 | 0 | Healthy / true | 34,090,512 | 7 | 5 |
| 60 | 13 | 25,887,160 | 6 | 0 | Healthy / true | 50,068,597 | 13 | 5 |
| 120 | 27 | 53,765,640 | 9 | 0 | Healthy / true | 90,037,572 | 27 | 5 |
| 125 | 28 | 55,756,960 | 9 | 0 | Healthy / true | 92,028,892 | 28 | 5 |


Continued writes add attempts and tables: an extra attempt follows each flush (the shortest gap between two attempts was **0.281 s**, right after a flush), so 28 attempts in 125 s instead of 25, and the live table count grows **4 -> 9** because compaction never succeeds (read amplification rises with every flush; the tmp size per attempt is unchanged, 1,991,320 bytes, because the failure still happens at the same block). After the graceful stop: 28 files, 55,756,960 bytes, 9 live tables.

### 4.4 Case C (the damaged block is the last one of the last table)

| t (s since the first failure) | `.sst.tmp` files | bytes in them (exact) | live tables | `cycles_completed` (`/v1/compaction/metrics`) | `/v1/status` `storage_state` / `/readyz` `ready` | `disk.sstable_bytes` (`/v1/admin/status`) | stderr `compaction: failed` lines (cumulative) | security.log lines |
|---|---|---|---|---|---|---|---|---|
| 0 | 1 | 11,939,728 | 4 | 0 | Healthy / true | 28,060,835 | 1 | 5 |
| 20 | 4 | 47,758,912 | 4 | 0 | Healthy / true | 63,880,019 | 4 | 5 |


What one attempt writes depends on **where the damage is**: here every attempt wrote **11,939,728 bytes** (the whole merge of the first three tables up to the last block) in 80-116 ms, at a 5.1 s cadence: about **2.3 MB/s of sustained disk writes**. The retained pile at 4 files is 47.8 MB after 20 s; extrapolated 8.4 GB per hour, 202 GB per day on a 16 MB database. In general one attempt writes up to the size of the data that precedes the damaged block in key order, i.e. up to the size of the whole database, every 5 s.

## 5. Extrapolation (labelled as such) and reach

* Case A: 1,991,320 B / 5.048 s = 394 KB/s = 23.7 MB/min = **1.42 GB/h = 34 GB/day**, with `Healthy` throughout. The pile is bounded by the volume (E: had 4.2 GB free), at which point the disk-full path of ADR-WE-SP-001 would engage on the flush side.
* **Even if every tmp file were deleted, the writes would continue**: the 394 KB/s (case A) or 2.3 MB/s (case C) of create-write-fail is the retry's own cost, and it scales with the database. Fixing only the leaked files bounds disk *usage*, not disk *traffic*.
* **Not reproduced, from reading the code:** the **flush path has the same leak**: a failed flush attempt takes a new id (`lsm/mod.rs:3040`) and calls the same writer (`:3041-3045`), whose early returns leave the tmp file; under ADR-WE-SP-001's ENOSPC retries (bounded fast retries, then the slow cadence in `StoragePressure`) each attempt would leave a partial file in the already-full volume. Not tested (ENOSPC was not simulated; there is no safe way to fill the data volume here).

## 6. Policies

| Policy | Rule | What it does for A / B / C | Failure mode it leaves open |
|---|---|---|---|
| **P1** fix the leak only | the writer removes its tmp file on every failure path | files: 0 leaked instead of 1 per attempt; disk usage bounded | the **silent retry stays**: the same 394 KB/s - 2.3 MB/s of disk writes (up to the whole database every 5 s), CPU, and a growing table count under writes (4 -> 9 in 125 s); nobody can see it; the file sizes that `disk.sstable_bytes` hinted at disappear, so the **only** symptom vanishes too |
| **P2** fix + report | P1 + `failures_total`, `consecutive_failures`, `last_error` in `CompactionMetrics`, on the existing endpoints | the failure is visible: `cycles_completed: 0` with `failures_total` rising | still retries forever; the I/O amplification continues; permanent and transient failures look the same |
| **P3** fix + report + stop retrying a permanently bad source | P2 + classify the error (structured variant, no string parsing); after the existing fast-retry budget of consecutive `Corruption` / `Unsupported` failures (or panics), the worker stops attempting until restart; state shown on the same surfaces | at most the budget's worth of attempts per process lifetime (3 by the existing `max_flush_retries`), none leaked, then silence on disk and a visible `blocked` | **compaction stays off until restart**, so the table count grows under writes (read amplification) and tombstones are never dropped; the damaged rows are still lost; a transient `Io` error mis-seen as permanent would wrongly stop compaction (mitigated by classifying only `Corruption`-shaped failures and by the streak budget) |
| **P4** other | (i) cleanup in `compact_once_impl` only (not the writer): removes the output id's tmp on `Err`; or (ii) a reused fixed tmp name so at most one leftover exists | (i) fixes compaction but not the flush path (same writer); (ii) bounds the pile to one file | neither reports nor stops the retry; (i) leaves the flush-path leak; (ii) changes naming and still wastes the writes |

### 6.1 Existing models, and why a new dimension

* **ADR-WE-SP-001** (`PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md`): `Healthy -> StoragePressure -> StorageFull` is a **write-availability** model driven by ENOSPC on the *flush* path (classification by structured error identity, a bounded fast-retry budget, a slower retry interval, explicit state, observability). It does not mention temporary files. Compaction deliberately does not drive it (Decision 11, `lsm/mod.rs:2672-2675`).
* **Extending `StorageState` is the wrong move**: it gates writes (`StorageFull` rejects them) and compaction itself (any non-`Healthy` value defers compaction), it is the `storage_state` string of `/readyz`, and `from_u8` maps an unknown value to `StorageFull` (`lsm/mod.rs:193-195`), so a fourth value would silently read as "disk full" anywhere it is not handled. A blocked compaction does not affect write availability. The reusable parts of ADR-WE-SP-001 are its **shape** (structured classification, bounded budget, slower interval, explicit observable state), and its **existing numbers** (`max_flush_retries` = 3, `storage_pressure_retry_interval` = 5 s), not its states.

## 7. Recommendation: P3

**Recommend exactly one: P3** (the leak fix and the report are its first two parts).

1. **P1 alone is not enough** and, worse, removes the one visible symptom (case C: 2.3 MB/s of writes for a 16 MB database, scaling with the data).
2. **P2 alone reports a loop that can never succeed**: the damaged block does not heal; every retry costs I/O and every flush adds a table (case B).
3. Stopping is safe: compaction is an optimisation; not running it never changes what a read returns; reads, writes and recovery are unaffected; tombstones are simply retained longer. Retrying a checksum failure on an immutable file produced the same failure 25 of 25 times (and 28 of 28).
4. It mirrors a pattern the project already decided for the flush path (ADR-WE-SP-001: classify, bounded budget, slower interval, state), reusing its two config values, without touching `StorageState`.
5. It is independent of F-08 (section 8) and needs an explicit authorisation, because every change is under the protected paths (`src/sstable/writer.rs`, `src/lsm/mod.rs`; possibly `src/compaction/mod.rs`).

## 8. Why this is a separate item from F-08

F-08 (`PHASE_ITEM_F08_DISCOVERY.md`, ADR-SST-01) is about **detection**: a damaged table must be found at open (the engine already refuses most damage; the product-layer preflight and the background data-block verification close the rest) and on read, and must be reported. This item is about **what the compaction worker does after detection has happened** — it detects the damage on its own, every five seconds, and then neither stops, nor cleans up, nor tells anyone. The two are independent: ADR-SST-01 changes nothing under the protected paths and does not touch the worker; this ADR changes only the worker, the writer's failure path and the metrics, and does not need ADR-SST-01. Approving either does not require the other, and with both the operator gets two corroborating reports (a table that fails verification; a compaction that is blocked). A data-block flip is the only way to reach this state with ADR-SST-01 in force (it refuses metadata damage before the engine opens), so the leak stays reachable until this item is fixed.

## 9. Not tested, limits

* **ENOSPC / disk-full during compaction or flush was not tested**; the statements about it are from reading the code.
* One host (NTFS), one fixture (4 MB tables, 200-byte values); damage in the first and the last position only; one damaged table at a time; a single bit flip.
* The extrapolated hourly / daily figures are linear projections of 25 (A) and 4 (C) real attempts; observations were capped at 125 s because the volume had 4.2 GB free.
* The manual `compact_once` path was not driven (`pub(crate)`); same function by code reading.
* A damaged table found only by a *query* (no compaction trigger, fewer than 4 tables) never reaches this state.

## 10. Separate findings

None new beyond what the F-08 mission recorded. The flush-path leak (section 5) is the same root cause in the same function and is recorded in the `OPEN_ITEMS.md` line of this item, not separately.

## 11. Evidence index

`E:\rubixdb_f08_compaction\raw\` (`case_A`, `case_B`, `case_C_end_of_last_table`: `result.json` with every sample, watcher timestamps and sizes, stderr and stdout; `restart\result.json`); `E:\rubixdb_f08_compaction\scripts\` (`leak_repro.py`, `leak_restart.py`, `make_leak_tables.py`, document generators); `E:\rubixdb_f08\tree\examples\f08c_open_hold.rs`; `E:\rubixdb_f08_compaction\rubixdb_leak_base.exe`.
