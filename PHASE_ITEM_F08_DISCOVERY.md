# PHASE ITEM F-08 — DISCOVERY: a damaged SSTable reports ready

**Date:** 2026-10-08. **Mission:** F-08 only; discovery and ADR, no engine behaviour changed, nothing under `src/sstable/`, `src/lsm/`, `src/manifest/`, `src/wal/`, `src/compaction/` or `src/error.rs` touched. Companion document: `PHASE_ITEM_F08_ADR.md` (ADR-SST-01, PROPOSED). Same shape as `PHASE_ITEM_F07_DISCOVERY.md`.

## 1. Identity

| Item | Value |
|---|---|
| Repository | branch `master`, `git status --short` empty, HEAD `03295e04439be50d28093ffd26812718bceaaaaf` |
| `git log -3 --oneline` | `03295e0` docs: PROJECT_STATE -- remove F-07 from the blockers list / `8fdab76` F-07: implement ADR-WAL-01 (P4) ... / `a3a7f96` F-07: WAL tail damage -- discovery and ADR-WAL-01 (PROPOSED) |
| Binary | `cargo build --release --locked -p rubixdb-cli` -> `E:\rubixdb_f08\rubixdb_f08_base.exe`, SHA-256 `904390ebd32af096f33b4f1286317c86a6f5e1bce49331fc903e333c9c461fb8` |
| Reported identity | `GET /v1/observability/version` -> `git_revision: 03295e04439b`, `build_identifier: 0.1.0-release-x86_64-windows` (raw: `raw\identity_version.json`) |
| Host | Windows 10 Home, release build, loopback, one instance at a time on port 302 |
| Fixtures | `runs\template`: table `t(id INTEGER PRIMARY KEY, v TEXT)`, 40,000 rows of ~200-byte values, 2 live SSTables (4.03 MB, 956 data blocks, 17.2k records each; ids 1 and 2), 5,590 rows still only in the WAL; `runs\template3`: 60,000 rows, 3 live SSTables (for the compaction case). WAL segment 1 is gone from both (purged after the flush checkpoint), so **the rows in SSTable 1 exist nowhere else in the directory.** |
| Engine-layer cost probe | an unmodified `git archive HEAD` copy plus one added example (`examples\f08_sst_probe.rs`) built to `E:\rubixdb_f08\tree_target`; the repository itself was not built into or modified |

## 2. What F-08 says, and where it comes from

* Baseline V-34 (`PHASE_RUBIXDB_CONFIGURATION_STARTUP_SHUTDOWN_BASELINE.md:174`): 16 bytes flipped mid-file in a 60,000-row table -> "opens and reports ready; `SELECT COUNT(*)` -> HTTP 500 `STORAGE_ERROR`; a primary-key lookup of an unaffected row -> 200; offline `rubixdb check` -> exit 2". F-08 (`:567`): "A damaged SSTable does not stop startup: the instance reports `ready` and some statements return HTTP 500 ... the damage is only reported by an explicit offline `rubixdb check`". Open question 6 (`:591`): "Should the product refuse to report `ready` when a physical integrity problem is detectable at open (SSTable damage), or is `rubixdb check` the intended detector?" Certification `:65`, `:157`, `:209` (L-28): unchanged, OPEN.
* The mission statement under test: "a damaged SSTable reports ready."

## 3. The code paths that decide SSTable integrity

Line numbers are for `03295e0`. Nothing here was modified.

| # | Question | Where | What it does |
|---|---|---|---|
| 1 | Validated **at write**? | `src/sstable/writer.rs:107-196` | temp file `<id>.sst.tmp`, `file.sync_all()` (:192), atomic `rename` (:195), `fsync_dir` (:196). **No read-back verification.** Because publication is atomic, a checksum failure in a published table can never be a torn write: it is damage. |
| 2 | Validated **at open**: footer | `src/sstable/reader.rs:90-126`, `src/sstable/format.rs:397-429` | file >= 72 bytes; magic (:405) -> `Corruption "footer: bad magic"`; `format_version` != 1 (:408) -> `Unsupported` (checked **before** the CRC); CRC32C of the footer (:414) -> `Corruption`; offsets must fit the file and tile exactly (data region, bloom, index, footer) (`reader.rs:102-126`). |
| 3 | at open: bloom | `reader.rs:128-139`, `format.rs:336-362` | block CRC32C (:342), declared size, no trailing bytes. |
| 4 | at open: index | `reader.rs:141-147`, `format.rs:252-313` | block CRC32C (:260), blocks contiguous from offset 0 (:269), within the data region, `last_key` non-decreasing (:294), cover the whole data region (:307). |
| 5 | **Not at open: data blocks** | `reader.rs:86-89` (doc), `reader.rs:195-205` | "Data blocks are **not** read here — bounded memory at `open()` regardless of table size." Each block carries its own CRC32C (`format.rs:153-192`), checked only when that block is read. |
| 6 | **Not at open: the Manifest's record of the table** | `src/lsm/mod.rs:2492-2512` vs `src/manifest/state.rs:12-17` | the Manifest stores `file_size`, `min_seq`, `max_seq` for every live table; `reconcile_sstables_with_manifest` opens the file and **compares none of them** (the comparison exists only in `rubixdb check`, `src/ops/check.rs:914`). |
| 7 | Which tables are opened | `src/lsm/mod.rs:2464-2533` (`reconcile_sstables_with_manifest`, called from `LsmEngine::open`) | `*.sst.tmp` deleted (:2482); a table in the Manifest's live set -> `SsTable::open`, any error is a hard `Err` naming the path (`wrap_sstable_path_error`, :2430-2440); a live table with no file -> `Corruption "manifest: sstable id N is recorded live but ... is missing"` (:2515-2523); a valid `*.sst` the Manifest never heard of is **adopted** and an `ADD_SSTABLE` is appended (:2501-2511, LSM Spec §7.2); a removed-but-undeleted one is deleted (:2495-2499). |
| 8 | At **first read** of a block | `reader.rs:217-263` (`get_versioned`, `?` at :247), `:403-409` (`range_scan_raw` yields one `Err` then stops), `format.rs:173-178` | `Err(EngineError::Corruption { detail: "block: checksum mismatch" })`. Fail closed for that read; nothing is cached, so the same read fails every time. |
| 9 | **Mid-run** | `reader.rs:67-82` | footer, bloom and index are decoded once at open and held in memory; they are never read from disk again. Data blocks are read from the file at every access (no block cache). |
| 10 | A failed compaction | `src/lsm/mod.rs:2950` (`eprintln!("compaction: failed, will retry on the next trigger: {e}")`), `src/sstable/writer.rs:130` (`let (key, seq, value) = item?;`) | the failure is printed to stderr only (no counter, no state, no event) and retried; the writer returns early at :130 and **never removes its `.sst.tmp`** (there is no `remove_file` in `writer.rs`). |
| 11 | What the product reports | `api/src/observability/sampler.rs:270-281` (`pub const fn ready() -> bool { true }`, Decision D5 / option R2: "whatever `storage_state` says"), `api/src/routes/health.rs:43-51`, `src/lsm/mod.rs:166-195` (`StorageState` has three values: `Healthy`, `StoragePressure`, `StorageFull`) | `/readyz` is a constant `true` plus `storage_state` plus `index_recovery`; there is no value that could express "a table is damaged". |
| 12 | How a failed read is reported | `api/src/error.rs:135-140` (KV routes: `ApiError::Engine(Corruption)` -> HTTP 500 `CORRUPTION` "the engine detected on-disk corruption" + detail), `api/src/error.rs:309-317` (SQL: `SqlError::Storage(detail)` -> HTTP 500 `STORAGE_ERROR` "a storage error occurred"; the detail goes to `tracing::error!`), `cli/src/host.rs:469-475` (the embedded host persists **only** `rubixdb_security` events; every other `tracing` event is unsunk on purpose) | the typed variant survives on the KV routes and is erased on the SQL route, and the detail that would name the table reaches no log in the supported host. |
| 13 | Product guards | `src/ops/format.rs:111-135, 161-195` (`startup_guard`, `startup_guard_with_tail_policy`), `src/ops/wal_tail.rs:879` | **no SSTable check at all.** The F-07 guard consumes `WAL_CLEAN_STOP` (:879) *before* the engine opens, so an engine refusal for an SSTable reason has already modified the directory. |
| 14 | The existing detectors | `src/ops/check.rs:898-956` (physical: manifest size, `SsTable::open`, full scan of every block, `SSTABLE_MISSING` / `SSTABLE_SIZE_MISMATCH` / `SSTABLE_CORRUPT`), `src/ops/check.rs:236-` (`check_engine`, logical, via `/v1/admin/check`) | `rubixdb check` finds every case below; nothing runs it automatically. |
| 15 | The contract | `RubixDB-LSM-Engine-Specification-v1.0.md` §2.3 test 6 (:268), §7.1 step 3 (:454-457), §7.4 (:477), §7.5 test 7 (:487); `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.5 (:404) | see section 5. |

## 4. Reproducers

### 4.1 Method (black box, current tree)

`E:\rubixdb_f08\scripts\f08_repro.py` (library `f08_lib.py`; template builders `build_template.py`, `build_template3.py`; mid-run `f08_midrun.py`). For each scenario: a fresh copy of the template; **one** mutation in SSTable 1 of the *stopped* instance (offsets read from the file's own footer and index); offline `rubixdb check` on a copy; start of the real binary; if it opens — `/healthz`, `/readyz`, `/v1/status`, `/v1/metrics/system`, `/v1/admin/status`, `/v1/compaction/status`, then a query matrix (every id stored in the damaged block, read from outside; 1,082 primary-key lookups over the whole table, each compared with the expected value; `COUNT(*)`; a range scan; an `INSERT`), the same endpoints again, a graceful stop and a SHA-256 comparison of every file; if it refuses — exit code, stderr, and whether the directory changed. Mid-run scenarios mutate a **running** instance. Raw: `E:\rubixdb_f08\raw\<scenario>\result.json`.

The mission's seven experiments: (a) `a_data_block_bitflip`; (b) `b1`-`b4` (footer magic, footer CRC, index offset with the CRC recomputed, record count with the CRC recomputed); (c) `c_bloom_bitflip`; (d) `d1`/`d2` (index body flip; index entry length +1 with the index CRC recomputed); (e) `e1`/`e2` (footer `format_version`: bit flip, and 2 with the CRC recomputed); (f) `f_truncated_half`; (g) `g_missing_file`. Extra: `k` (a data block of the *second* table), `h` (SSTable 1 replaced by a byte copy of SSTable 2: a fully valid file that is not what the Manifest recorded), `i` (a valid foreign `.sst` the Manifest never recorded), `j` (random bytes named `00000000000000000009.sst`). `S0` is the control.

### 4.2 Results (start of a stopped, damaged directory)

| scenario | opened? | exit | refusal text (path shortened) | /readyz | lookups in the damaged block | sample of 1,082 lookups | COUNT(*) | INSERT | an SSTable modified by the start? | other files changed by the start | offline `rubixdb check` |
|---|---|---|---|---|---|---|---|---|---|---|---|
| S0_control | yes (0.522 s) | - | - | 200 `ready:true` `Healthy` | - | {'correct': 1082} | 200 | 200 | no | WAL_CLEAN_STOP, wal/wal-00000000000000000003.log | exit 0 |
| a_data_block_bitflip | yes (0.525 s) | - | - | 200 `ready:true` `Healthy` | {'error': 18} | {'correct': 1082} | 500 | 200 | no | WAL_CLEAN_STOP, wal/wal-00000000000000000003.log | exit 2 |
| k_data_block_other_table | yes (0.526 s) | - | - | 200 `ready:true` `Healthy` | {'error': 18} | {'correct': 1081, 'error': 1} | 500 | 200 | no | WAL_CLEAN_STOP, wal/wal-00000000000000000003.log | exit 2 |
| b1_footer_magic | NO | 1 | corruption: sstable <data>/sstables\00000000000000000001.sst: footer: bad magic | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 2 |
| b2_footer_crc | NO | 1 | corruption: sstable <data>/sstables\00000000000000000001.sst: footer: checksum mismatch | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 2 |
| b3_footer_index_offset | NO | 1 | corruption: sstable <data>/sstables\00000000000000000001.sst: footer: index block extends past the file | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 2 |
| b4_footer_record_count | yes (0.505 s) | - | - | 200 `ready:true` `Healthy` | - | {'correct': 1082} | 200 | 200 | no | WAL_CLEAN_STOP, wal/wal-00000000000000000003.log | exit 2 |
| c_bloom_bitflip | NO | 1 | corruption: sstable <data>/sstables\00000000000000000001.sst: bloom: checksum mismatch | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 2 |
| d1_index_body_bitflip | NO | 1 | corruption: sstable <data>/sstables\00000000000000000001.sst: index: checksum mismatch | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 2 |
| d2_index_structure | NO | 1 | corruption: sstable <data>/sstables\00000000000000000001.sst: index: block_offset is not contiguous with the preceding block (gap or overlap detected) | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 2 |
| e1_footer_version_bitflip | NO | 1 | unsupported: sstable <data>/sstables\00000000000000000001.sst: sstable format_version 0 | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 2 |
| e2_footer_version_2 | NO | 1 | unsupported: sstable <data>/sstables\00000000000000000001.sst: sstable format_version 2 | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 2 |
| f_truncated_half | NO | 1 | corruption: sstable <data>/sstables\00000000000000000001.sst: footer: bad magic | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 2 |
| g_missing_file | NO | 1 | corruption: manifest: sstable id 1 is recorded live but <data>/sstables\00000000000000000001.sst is missing from disk | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 2 |
| h_swapped_valid_file | yes (0.518 s) | - | - | 200 `ready:true` `Healthy` | - | {'error': 1082} | 404 | 404 | no | WAL_CLEAN_STOP, wal/wal-00000000000000000003.log | exit 2 |
| i_foreign_valid_sst | yes (0.508 s) | - | - | 200 `ready:true` `Healthy` | - | {'correct': 1082} | 200 | 200 | no | MANIFEST, WAL_CLEAN_STOP, wal/wal-00000000000000000003.log | exit 1 |
| j_foreign_junk_sst | NO | 1 | corruption: sstable <data>/sstables\00000000000000000009.sst: footer: bad magic | - | - | - | - | - | no | WAL_CLEAN_STOP | exit 3 |


All opened cases answered `/healthz` 200 and `/readyz` `{"ready": true, "storage_state": "Healthy", "index_recovery": "complete"}` within about 0.5 s; every refused start exited **1** with `rubixdb gui: failed to start: engine open failed: ...` naming the first failing table.

### 4.3 What the table says

1. **Everything the engine validates at open is already refused, not "reported ready".** Footer magic (b1), footer CRC (b2), index offset (b3), bloom (c), index body (d1), index structure (d2), format version (e1/e2, `Unsupported` — checked before the CRC, so the version bit flip reports `format_version 0`), a file truncated to half (f; the new last 72 bytes are not a footer: `bad magic`), a live table whose file is missing (g) and a junk foreign `.sst` (j): **11 of 11 refuse, exit 1, the damaged file untouched.** Unlike F-07's C1-C3 (refused only by a *product* guard because the engine ignores corrupted WAL segments, ADR-ENG-OPS-001), the SSTable refusal is in the engine itself and covers the bare-engine path too.
2. **Data-block damage is the gap.** Case (a): one bit flipped in block 478 of 956. The server opens and is `ready: true` / `Healthy`. The 18 rows stored in that block (ids 8603-8620) fail on every lookup with HTTP 500 `STORAGE_ERROR`; all 1,082 sampled lookups that miss the block return the **correct value**; `COUNT(*)` and a range scan over ids 100-400 return 500 (the SQL executor scans); an `INSERT` succeeds (200). Case (k) is the same in the second table. **No lookup in any scenario returned a wrong value for a bit-level fault**: the per-block CRC32C turned every one into an error. Those 18 rows are **not recoverable from the directory** (WAL segment 1 is purged): the fixture's SSTable 1 is their only copy.
3. **What the product shows for (a).** `/readyz`: `ready: true`, `Healthy`. `/v1/status`: `storage_state: Healthy`, `sstable_count: 2`, `live_sstable_count: 2`. `/v1/admin/status`, `/v1/metrics/system`, `/v1/compaction/status`: no field about integrity. The only trace is generic: `errors.http_server_errors_since_start` and `errors.sql_errors_since_start` rise by the number of failed requests (18 in the run), `server.err` stays empty. On the **KV** routes the typed error survives: `GET /v1/kv/<key>` for a key in the damaged block -> 500 `CORRUPTION` "the engine detected on-disk corruption", detail `block: checksum mismatch`, while a key in a clean block -> 200 (`raw\kv_endpoints_data_block.json`); on the **SQL** route it is `STORAGE_ERROR` "a storage error occurred" (section 3 row 12). Neither names the table or the block.
4. **The Manifest's record of a table is not enforced (h).** SSTable 1 replaced by a byte copy of SSTable 2 — a perfectly valid file — opens, `ready: true`, and *every* statement, including `INSERT`, returns HTTP **404 `NOT_FOUND`** "the referenced object was not found": the file that held the catalog rows is gone, the engine returned "no such key", and nothing checked the file against the Manifest's recorded size (4,030,612 vs 4,030,165) or sequence range (1-88 vs 89-175). This is a silent loss of data presented as a missing object, not an error. Offline `rubixdb check` flags it (`SSTABLE_SIZE_MISMATCH`, exit 2). Likewise (b4): a footer `record_count` that lies (CRC recomputed) opens and serves correct data (the field is informational), but `check` exits 2.
5. **A valid foreign table is adopted (i)**: it opens, the engine appends `ADD_SSTABLE` to the Manifest (the `MANIFEST` file is the one extra file changed), reads stay correct. That is LSM Spec §7.2/§7.5 test 4 (a crash between the table's publication and its `ADD_SSTABLE`), by design; it means the Manifest is authoritative for *removal* but not for *addition*.
6. **A refused start is not byte-for-byte clean.** In every refused scenario the only changed file is `WAL_CLEAN_STOP`: the F-07 startup guard consumes the attestation before the engine runs its SSTable checks (`wal_tail.rs:879`). The data, WAL and Manifest are untouched, but the next start (after the operator repairs the table) is unattested, so the F-07 protection is silently gone for that cycle. A new finding (section 9).

### 4.4 Damage that appears **after** the engine opened the table

| scenario | done to the RUNNING instance | lookups in the damaged block, after | sample, after | COUNT(*) | after stop: restart |
|---|---|---|---|---|---|
| m1_running_data_block | data block bit flipped while running | {'error': 18} | {'correct': 1082} | 500 | opened |
| m2_running_footer | footer magic bit flipped while running | {'correct': 18} | {'correct': 1082} | 200 | REFUSED: corruption: sstable <data>/sstables\00000000000000000001.sst: footer: bad magic |
| m3_running_bloom | bloom bit flipped while running | {'correct': 18} | {'correct': 1082} | 200 | REFUSED: corruption: sstable <data>/sstables\00000000000000000001.sst: bloom: checksum mismatch |
| m4_running_index | index byte flipped while running | {'correct': 18} | {'correct': 1082} | 200 | REFUSED: corruption: sstable <data>/sstables\00000000000000000001.sst: index: checksum mismatch |
| m5_running_truncate | file truncated to half while running | {'correct': 18} | {'correct': 852, 'error': 230} | 500 | REFUSED: corruption: sstable <data>/sstables\00000000000000000001.sst: footer: bad magic |
| m6_running_delete | file deleted while running (delete succeeded; the engine still holds an open handle) | {'correct': 18} | {'correct': 1082} | 200 | REFUSED: corruption: manifest: sstable id 1 is recorded live but <data>/sstables\00000000000000000001.sst is missing from disk |


* **Footer, bloom and index damage while running is not detected at all until the next open** (m2, m3, m4): every lookup stays correct (the decoded structures are in memory), `/readyz` stays `ready`, and after the stop the next start is refused with the same message as in section 4.2. The same for a file **deleted** while running (m6): on Windows the delete succeeds, the engine's open handle keeps serving every read correctly, and the next start is refused (`recorded live but ... is missing`). Nothing in the product reports either condition while the instance runs.
* **Data-block damage while running is detected by the first read of that block** (m1): immediate 500 for the 18 rows, no caching, the rest correct, and the next start *opens* (the data block is not checked at open).
* **Truncating the file while running** (m5): reads of blocks past the new end fail (230 of 1,082 sampled lookups return an error, the rest are correct), `COUNT(*)` 500; the next start is refused (`footer: bad magic`).

### 4.5 A damaged data block meets compaction (m7, three-table fixture, trigger = 4 tables)

One bit flipped in block 478 of SSTable 1 (ids 8603-8620); started; 24,000 more rows inserted so the fourth table is flushed and compaction triggers. Result over 60 s (`raw\m7_compaction_over_damaged_block\result.json`):

* `cycles_completed` stays **0**; `/v1/compaction/status`, `/v1/admin/status`, `/v1/metrics/system` show no failure count or state; `/readyz` `ready: true` / `Healthy`; `/v1/status` healthy.
* `server.err` gets `compaction: failed, will retry on the next trigger: corruption: block: checksum mismatch` — **once every 5 seconds** (13 lines in 60 s).
* Each failed attempt leaves a partial output behind: **13 `*.sst.tmp` files of 1,991,320 bytes** (one per attempt, ids 5-17, timestamps 5 s apart), 41 MB in the `sstables` directory after one minute, none removed while the process runs (the startup sweep deletes them at the next start). Extrapolated at that size and cadence, about 24 MB per minute, 1.4 GB per hour, 34 GB per day, with the instance reporting `Healthy` the whole time. The size of each leftover depends on how far the merge got before the damaged block; the cadence and the absence of any report do not.
* Writes continue to succeed; the damaged ids still fail on every read.

This is outside the F-08 decision (the cause is in `src/sstable/writer.rs:130` and `src/lsm/mod.rs:2950`, both protected) and is recorded as a separate finding (section 9); it matters here because it is what a damaged data block does to an otherwise healthy instance over time.

## 5. The contract: honoured, or divergent

| Question | Answer, with the evidence |
|---|---|
| Where is integrity validated today? | **At write:** only by construction (fsync + atomic rename, no read-back; `writer.rs:192-196`). **At open:** footer, bloom, index and the file layout of every table the engine opens (`reader.rs:90-159`); presence of every Manifest-live table (`lsm/mod.rs:2515-2523`). **At first read:** each data block's CRC32C, per block, on every read (`format.rs:173-178`). **Nowhere:** data blocks at open; the Manifest's recorded `file_size` / `min_seq` / `max_seq` against the file (recorded, never compared by the engine; compared only by `rubixdb check`, `check.rs:914`); footer / bloom / index again after open. |
| A read through a corrupted SSTable: fail closed, fail open, or silently wrong? | **Fail closed, per read.** `get_versioned` / `contains_versioned` / `range_scan_raw` return `Err(EngineError::Corruption { detail: "block: checksum mismatch" })`; the engine does not skip the table or fall through to an older one (observed: 18 of 18 lookups in the block fail, 0 wrong values in any scenario). The one **silent** wrong answer found is not a bit error but a *valid-but-wrong file* (h): the engine returns "no such key" for keys the file never had, because nothing ties the file to the Manifest. |
| What does the product report when a table is damaged but the engine is healthy? | `/healthz` 200 `ok`; `/readyz` 200 `ready: true`, `storage_state: Healthy` (a constant by decision D5, `sampler.rs:270-281`); `/v1/status`, `/v1/metrics/system`, `/v1/admin/status` identical to a healthy instance except generic error counters; SQL errors are the generic `STORAGE_ERROR`, KV errors the typed `CORRUPTION` without the table. Offline `rubixdb check` exits 2 on every case (`SSTABLE_CORRUPT` / `SSTABLE_SIZE_MISMATCH` / `SSTABLE_MISSING`); the **online** `rubixdb check` of a running instance reports it too (`CHECK_INCOMPLETE ... row scan failed: corruption: block: checksum mismatch`, exit 2, `raw\online_check_data_block.txt`). |
| LSM Engine Spec: honoured? | **§2.3 test 6** (a corrupted byte in a data block is detected by that block's checksum and not returned as valid): honoured. **§7.1 step 3, §7.4, §7.5 test 7** (a footer or index checksum failure on a live-set file escalates the engine to DEGRADED): honoured in effect — the product refuses to start, which is stricter than the spec's degraded state; the *degraded-but-serving* state of Architecture Spec §3.1 does not exist anywhere in the product. **Manifest authority:** §7.1 builds the live set from the Manifest and §7.2 adopts unknown valid files; the spec never says to compare the Manifest's recorded size and sequence range with the file, and the engine does not: recorded but unenforced (h). |
| A stale premise | `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.5 and `src/sstable/mod.rs:6-16` say SSTable corruption is "a fail-closed *availability* concern, never a *durability* one" because "the WAL is **never** truncated or purged as a result of any flush this phase". Since Phase 5 the flush sequence ends with `pool.purge_before(meta.max_seq)` (`lsm/mod.rs:3123`): the fixture's WAL segment 1 is gone. **After Phase 5, SSTable damage is data loss**, not unavailability. Neither document was edited. |

## 6. Policies

"Current" first, because the mission's P1 is largely already true.

| Policy | Rule | What it does for the scenarios | Failure mode it leaves open |
|---|---|---|---|
| **Current** | metadata (footer / bloom / index / presence): refuse at open (engine). Data blocks: lazy, first read. Nothing else. | b1-b3, c, d1, d2, e1, e2, f, g, j refuse; **a, k open ready**; **h opens ready and serves a missing catalog**; b4 opens (harmless) | data-block loss is invisible until touched, reported only as a generic 500; swapped / wrong-generation files are accepted; the refused start still modifies the directory (attestation); nothing reports damage that appears mid-run (footer / bloom / index: never until restart); compaction loops on a damaged block |
| **P1** | validate every SSTable at open and refuse. As worded (footer / CRC / index) this **is the status quo**; extended to every data block it costs one full read of the dataset per start | with data blocks: (a), (k) refuse | **Cost:** measured full-block scan 8.8 ms / 97 ms / 389 ms for 4 / 64 / 256 MiB (470-680 MiB/s, warm cache, one core; `raw\cost_probe.txt`) — about 1.5 s per GiB warm, more cold; the embedded host gives the engine **30 s** to become ready (`cli/src/host.rs`, `recv_timeout(30)`), the same bound that made index recovery move after readiness (`host.rs` comment on the 21-35 s measurement), so a large database would fail to start for being large. **Availability:** the intact 99.9 % cannot even be opened for export. **Still blind** to damage after the scan, and to h unless the Manifest is also compared. |
| **P2** | lazy only (query-time errors; health stays ready) | = current for data blocks | the operator learns from a user's 500; no signal at all if the damaged rows are not queried; loss grows unnoticed and backups of a damaged directory are taken unknowingly |
| **P3** | open validates metadata, reports invalid tables as *degraded* through the health surface; queries on them fail typed | for data blocks: needs something to find them (a scrub) | for **metadata** damage it would *weaken* today's refusal (the engine refuses to open; opening around a bad table is an engine change in protected paths); `ready` is a constant `true` by decision D5 and `StorageState` has no degraded value, so the health surface needs new additive fields; a table dropped from service makes older versions of keys reappear and deleted keys come back, which is worse than the refusal |
| **P4** (recommended) | **keep the engine's refusal; make it earlier, directory-preserving and Manifest-authoritative (product-layer preflight); find data-block damage with a throttled background verification and put the result on the existing health surface (additive fields, `ready` unchanged)** | refusals unchanged in outcome, now before any file is changed; h and size / sequence mismatches refuse; (a), (k) detected within one scan pass and reported by name | detection is delayed by the scan (about 1.5 s per GiB warm, throttled lower), damage after a pass is found only by the next pass / first read / `rubixdb check`; the product still keeps serving and accepting writes with known-lost rows; the compaction retry leak needs an engine fix (separate finding) |

### 6.1 The mid-run case, and the older design

* **After open, today:** footer / bloom / index damage is never detected (held in memory, row 9), a deleted or truncated file is noticed only where a read lands (m5, m6), data-block damage is detected at the first read of that block (m1), and compaction detects it every five seconds and tells nobody (4.5).
* **Manifest-authoritative read path:** the engine builds its table set from the Manifest (live) plus a sweep (adoption of unknown valid tables, `lsm/mod.rs:2464-2533`), reads through `Arc<SsTable>` handles it opened once, and never goes back to the Manifest's recorded size / sequence range. That is why h is accepted and why a post-open change to the file system is invisible: the design verifies a table **once, at the metadata level, and trusts the handle afterwards.** Phase 4B made that safe because the WAL was the durable copy (SSTable spec §3.5); Phase 5 removed that safety net (section 5) without adding a data-level check.

## 7. Recommendation: P4

**Recommend exactly one: P4.** The reasoning, from the measurements:

1. The refusals that exist are right and must not be weakened (CLAUDE.md): every footer / bloom / index / version / missing-file case refuses with a path-naming message. P3 would trade them for a degraded-serving state that does not exist and that would be unsafe (resurrected versions).
2. What is wrong is **when and how** the damage is found and shown: (i) the refusal happens after the F-07 guard has already changed a file; (ii) the Manifest's recorded size and sequence range are ignored, so a swapped file is accepted (h); (iii) data blocks are checked only when read, so the loss is silent; (iv) the health surface cannot say it; (v) the SQL route erases the typed error and the supported host persists no log that would name the table.
3. A synchronous full scan (P1 extended) is measurable (1.5 s per GiB warm) and in direct conflict with the 30 s readiness bound; a background scan is not, and it has precedent in this product (index recovery runs after readiness for the same reason).
4. Everything proposed lives above the engine (`src/ops`, `cli/src/host.rs`, the api crate's health / status routes): **no change under any protected path**, the same gate as F-07.

## 8. Comparison to F-07 (WAL tail vs SSTable)

Same gate: the certified engine cannot change, so both are product-layer ADRs, and both reuse the startup-only guard (`startup_guard_with_tail_policy`). The decisions they need are different:

| | F-07, WAL tail | F-08, SSTable |
|---|---|---|
| Is a damaged file *ambiguous*? | Yes: a torn last frame and bit-rot of the last frame have the same bytes (Spec §6.2 step 5), so the decision was how to **disambiguate** (an attestation written at a graceful stop) and not to destroy the evidence (quarantine). | **No.** Publication is atomic (tmp + fsync + rename), so every checksum failure is damage, never a crash artifact. There is nothing to disambiguate and no quarantine to make. |
| What does the engine do today? | Silently truncates (by specification); the product guard ignored the signal. | Already refuses at open for everything it validates; reads fail closed. |
| What was missing? | A fact (was the stop graceful?) and a signal. | **Coverage** (data blocks; the Manifest's record), **timing** (before any file is touched; after open), and a **place to say it** (readiness is a constant; no degraded value; typed error erased; no log sink). |
| Size of the loss | A suffix of history (un-checkpointed tail); the database stays prefix-consistent. | The middle of history; after Phase 5 the damaged rows exist nowhere else; excluding the table would make old versions reappear. |
| Can the operator override? | Yes — it only accepts a bounded, prefix-consistent loss that had already happened. | **No** (see the ADR): an override would mean serving a database with a hole in it. |
| Can the product layer fix all of it? | Yes. | Detection and reporting, yes. The compaction retry loop and any per-table quarantine need an engine change and stay out. |

## 9. Separate findings (recorded in `OPEN_ITEMS.md`, not fixed, not touched further)

1. **A failing compaction leaks a `*.sst.tmp` every retry and reports nothing.** With a damaged data block in a compaction input, every attempt (every 5 s) leaves a partial output (1,991,320 bytes each in the run) and prints one stderr line; no counter, no state, `Healthy` throughout. Cause: `src/sstable/writer.rs:130` returns early without removing the temp file; `src/lsm/mod.rs:2950` retries. Reproduction: `python E:\rubixdb_f08\scripts\f08_midrun.py m7_compaction_over_damaged_block`. Protected paths; needs its own mission and ADR.
2. **A refused start modifies the data directory when the refusal is not a WAL one.** The F-07 startup guard consumes `WAL_CLEAN_STOP` before the engine runs its own checks; any SSTable refusal (b1-j) therefore changes one file, and the next start is unattested. Reproduction: any refused scenario in `raw\*\result.json` (`changed_files: ["WAL_CLEAN_STOP"]`). Product layer (`src/ops/wal_tail.rs`).
3. **`rubixdb check --instance NAME` against a running instance checks the wrong instance.** `check` finds that `NAME` is running through `--instance`, but `online()` connects through `resolve_connection()`, which reads only `RUBIXDB_INSTANCE_NAME` (`cli/src/main.rs:176-193`); with the variable unset it connects to — and auto-creates — the *default* instance, prints "online logical check ... (instance NAME is running)" and reports `0 error(s)`, exit 0, for an empty database it just created. Reproduction: `raw\online_check_cli.json` (the data-block case, `--instance recon`, exit 0, snapshot_seq 0, tables_checked 0) versus `raw\online_check_data_block.txt` (same instance selected through the environment, exit 2). A false "clean" from the intended detector.
4. **`check` calls an SSTable data-block failure `CHECK_INCOMPLETE` while reporting `complete: true`.** Cosmetic and confusing (`SSTABLE_CORRUPT` exists for the physical pass). Not recorded separately.

## 10. Not tested, limits

* **Power loss and real disk errors were not tested**; all damage is injected by editing files from outside. Whether a power cut can leave a published SSTable damaged is unverified (publication is fsynced and renamed atomically).
* One host, one filesystem (NTFS), one table shape (200-byte values, ASCII). Tables of 4 MB; the cost probe (`raw\cost_probe.txt`) extends to 256 MiB with the real writer and reader but with a warm cache; **a cold-cache figure was not measured** (disk-bound; unknown on this host).
* Single-bit and structured single-field damage only; no random-damage campaign; no multi-table simultaneous damage (the engine reports the first failing table it meets in directory order and stops).
* The standalone `rubixdb-api` binary was not run (unsupported for v1); it shares `LsmEngine::open` and therefore the engine-level behaviour above.
* Online reads of a **bloom filter that is wrong but passes its CRC** cannot occur from a single fault; not tested.

## 11. Evidence index

`E:\rubixdb_f08\raw\` (17 open-time scenarios, 6 mid-run scenarios, the compaction case `m7_...`, `cost_probe.txt`, `identity_version.json`, `kv_endpoints_data_block.json`, `online_check_cli.json`, `online_check_data_block.txt`, `matrix_summary.txt`, `midrun_summary.txt`); `E:\rubixdb_f08\scripts\` (`f08_lib.py`, `f08_repro.py`, `f08_midrun.py`, `build_template.py`, `build_template3.py`, `make_tables.py`); `E:\rubixdb_f08\tree\examples\f08_sst_probe.rs` (the cost probe, in an unmodified copy of HEAD); `E:\rubixdb_f08\rubixdb_f08_base.exe`.
