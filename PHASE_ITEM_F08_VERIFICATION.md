# F-08 — verification of the implemented SSTable-integrity work (ADR-SST-01): named gaps closed or classified

**Date:** 2026-10-11. **Start:** `master`, HEAD `a95e2f4aa52986e4a797921948751e7db16adf5b`, `git status --short` empty. **Scope:** audit and verify what ADR-SST-01 (ACCEPTED, option P4) implemented; reproduce the named gaps; change only what the Fix Policy allowed. **Code changed: none.** Every source and test file is byte-identical to `a95e2f4`; the only tracked-file change is an additive erratum in `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` and the records. No protected path was touched. Failure model: process termination and file damage applied from outside to a stopped or running instance; power loss, a real disk error and other platforms are **NOT TESTED**.

**Verdict: F-08 PARTIALLY VERIFIED — NAMED GAPS REMAIN.** Everything ADR-SST-01 decided is verified on the real binary. Four items are policy decisions that belong to the maintainer, none is an implementation defect against the written ADR, and one stale comment sits in a protected file.

Environment: Windows 10 Home 19045, 15.9 GiB RAM, 8 logical CPUs, volume `E:` 128 GB SSD (no NVMe on this machine, earlier missions), rustc/cargo 1.98.1. Binary: `cargo build --release --locked -p rubixdb-cli` at `a95e2f4`, SHA-256 `f655de734840d02f6f134b28b1f88e53b38bded0c4716ced198c6569dc1bd05d` (`E:\rubixdb_f08b\rubixdb_head.exe`). Evidence: `scratch/f08_verification/` (scripts, raw JSON, logs; the copy under `E:\rubixdb_f08b\` is the original). Harness helper `recon.py` lives in `E:\rubixdb_closure\` and is not versioned.

## 1. Acceptance criteria used (the ADR's Decision, nothing added)

AC-1 read-only preflight before the F-07 attestation is consumed and before engine open; a refused start leaves the directory byte-identical. AC-2 Manifest-authoritative: live tables exist, match recorded size and sequence range, open with the engine's own `SsTable::open`. AC-3 unrecorded tables the engine would adopt must open. AC-4 no operator override. AC-5 throttled, cancellable background verification of every data block of the tables live at start. AC-6 additive `/readyz.sstable_verification`, `/v1/status.sstable_integrity`, security event, stderr line; `ready` unchanged. AC-7 `RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC`: 0-1024, default 64, 0 disables, bad value stops startup. AC-8 no engine / protected-path / format change.

## 2. Acceptance matrix

| ID | Status | Evidence (all reproduce from the commands in section 9) | Remaining gap |
|---|---|---|---|
| AC-1 | **PASS** | Open-time matrix on HEAD: 12 damaged cases refused, exit 1, `dir_unchanged=True` for each (`raw/open/*/result.json`, `logs/open_matrix.txt`). F-07 interaction: SSTable-refused start leaves `WAL_CLEAN_STOP` byte-identical, the later WAL damage is refused `WAL_TAIL_DAMAGED` (`raw/extra/f07_interaction_after`). Unit `every_open_time_damage_scenario_is_refused_before_any_file_changes_and_the_engine_agrees`. | none |
| AC-2 | **PASS** | truncated half -> `SSTABLE_MISMATCH` (size); valid-but-wrong file -> `SSTABLE_MISMATCH`; missing -> `SSTABLE_MISSING`; unit test for sequence range. | none |
| AC-3 | **PASS** | valid foreign table `i`: opens and is adopted; junk foreign table `j`: refused `SSTABLE_CORRUPT`, directory unchanged. | none |
| AC-4 | **PASS** | No SSTable override exists (`cli/src/startup_env.rs` has only the WAL-tail override). With `RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL=1` a bloom-damaged table and a missing table are still refused, directory unchanged (`raw/cancel_and_override.json`). | none |
| AC-5 | **PASS** (throttle, cancel) | Section 3 (3.24 GiB fixture). Cancel at scale: pass at 1 MiB/s on the fixture, graceful stop 10 s in took 0.4 s, exit 0. Unit `a_cancel_stops_the_pass_at_the_next_block`. | default-rate impact on foreground latency: see GAP-A |
| AC-6 | **PASS** | `a`, `k`: `/readyz` `{ready:true, Healthy, sstable_verification:"damaged"}`, `/v1/status.sstable_integrity` names the table and the counts, one stderr line, exactly one `sstable.damaged` event per table in `security.log` (`table=1 records_before=8612`). No key or row data in any of them. | none |
| AC-7 | **PASS** | Real binary: unset / `""` -> 64 (pass runs); `0` -> `disabled`; `abc`, `1025`, `-1`, `" 5"` -> exit 1 naming the variable, directory unchanged (`logs/extra.txt`). Unit: `the_budget_setting_is_parsed_strictly`, `startup_env` 10 of 10. | none |
| AC-8 | **PASS** | `git diff a95e2f4 --stat` shows no file under `src/`, `api/`, `cli/`, `sql/`, `tests/`; `git diff a95e2f4 -- src/wal src/manifest src/sstable src/compaction src/error.rs` empty. | none |

## 3. A. Is the verification rate a real throttle? **PASS (accuracy); the default's cost is a decision (OPEN)**

Fixture: 3 SSTables of 1,160,465,903 bytes each (278,406 records of 4 KiB values; 3,320 MiB = 3.24 GiB), written by the real engine through a scratch generator (`gen/`), plus a 320-record WAL tail; real release binary, one instance, `E:` volume. Method: from outside, `/v1/status.sstable_integrity` every 0.5 s, the server process' read bytes and CPU seconds (psutil `io_counters`, `cpu_times`), and optionally a closed-loop KV point-read probe (4 clients, persistent connections, random keys, all hits). "Nominal" is `bytes_verified` / pass time; "process" is read bytes of the server process / pass time. Warm = file cache populated by the fixture write; cold = after writing and reading a 12-14 GiB junk file (read back at 371-373 MiB/s, i.e. disk speed).

| Run | Setting (MiB/s) | Pass time | Nominal | Process read | CPU of the pass | Cache |
|---|---|---|---|---|---|---|
| R64_warm_noprobe | 64 | 52.0 s | 63.8 MiB/s | 3,320 MiB | 2.4 s (4.7 % of one core) | warm |
| A1_default_unset_warm | unset (=64) | 52.3 s | 63.4 | 3,355 MiB | 2.6 s (5.0 %) | warm |
| R256_warm_noprobe | 256 | 13.4 s | 247 | 3,320 MiB | 3.4 s (25 %) | warm |
| R1024_warm_noprobe | 1024 | 3.6 s | 926 | 3,320 MiB | 3.5 s (97 %) | warm |
| R1024_cold_noprobe | 1024 | 20.6 s | 161 | 3,302 MiB | 3.9 s (19 %) | cold |
| A64_cold / A64_cold2 | 64 | 53.6 s / 56.5 s | 61.9 / 58.7 | (with probe) | (with probe) | cold |

Findings. (1) The setting is a **real throttle**, not nominal: process read bytes equal the fixture size (3,320 MiB) and the achieved rate tracks the setting (64 -> 63.4-63.8, 256 -> 247-253; within 1-4 %). The extra process reads in the probe runs are the probe's own. (2) 1024 is above what one core scans warm (926 MiB/s, CPU-bound) and above what the disk delivers cold (161 MiB/s, I/O-bound); the cap is not the limiter there. (3) Time is linear in size: about 16 s per GiB at 64 MiB/s, so 100 GiB is about 27 min (ADR prediction confirmed at this scale). (4) Resident memory of the process stayed at 90-92 MB in every run. (5) The pass thread's CPU at the default is about 5 % of one core. (6) Start-up: ready after 0.57-0.67 s warm with 3.2 GiB of tables, 1.5-8.8 s cold (the preflight and the engine each read every table's footer, bloom and 8.6 MB index); the embedded host's bound is 30 s. Index reading scales with table count and block count, not with data size per se; larger databases are NOT TESTED.

Foreground effect of the default (4 clients, p50 / p95 / p99 in ms, ~6,000 req/s warm):

| Condition | During the pass | Without the pass |
|---|---|---|
| warm, 64 MiB/s | 0.60 / 1.09 / 1.35 (6,228 req/s) | 0.60 / 1.08 / 1.36 (A0_disabled_warm) |
| warm, 256 MiB/s | 0.63 / 1.12 / 1.40 (-4 % req/s) | same |
| warm, 1024 MiB/s | 0.61 / 1.10 / 1.46 (-3.5 % req/s) | same |
| **cold, 64 MiB/s, sample 1 / 2** | **2.11 / 11.16 / 17.71** and **1.74 / 10.94 / 16.73** | **3.15 / 5.68 / 7.23** and **3.10 / 5.58 / 6.96** |

Warm cache: no measurable effect. Cold cache: the median is lower during the pass (the pass warms the cache) but p95 doubles and p99 rises 2.4x in both samples; each cold sample needed a 7-8 minute eviction, so n = 2 per arm and the arms are confounded by cache warm-up. **This is a measurement, not a validation of 64 MiB/s**; it was taken with read-only traffic, one disk, no concurrent writes or compaction. Whether +5 ms p95 / +10 ms p99 on cold random reads is acceptable is a maintainer decision (GAP-A below). The three isolated M1.3 numbers are in section 8.

## 4. B. Corruption detection (disposable copies of one template: 2 live SSTables, 40,000 rows; real binary)

| Case | How introduced | Detected by / when | Result | `rubixdb check --data-dir` | Status |
|---|---|---|---|---|---|
| Footer magic (b1), footer CRC (b2), index offset with CRC fixed (b3), version bit (e1), version 2 (e2) | bit flip / field rewrite, stopped copy | startup preflight (`SsTable::open`) | refused, exit 1, names the file, `The directory has not been modified.` | exit 2, `SSTABLE_CORRUPT` | **PASS** |
| Bloom (c), index body (d1), index structure with CRC fixed (d2) | same | preflight | refused, directory unchanged | exit 2 | **PASS** |
| Truncated (f), swapped valid file (h) | rewrite | preflight size check | `SSTABLE_MISMATCH`, unchanged | exit 2 `SSTABLE_SIZE_MISMATCH` | **PASS** |
| Missing live table (g) | delete | preflight | `SSTABLE_MISSING`, unchanged | exit 2 | **PASS** |
| Junk foreign table (j) | add file | preflight | refused `SSTABLE_CORRUPT`, unchanged | exit 3 (INCOMPLETE) | **PASS** (exit 3 vs 2 is a classification difference of `check`, noted) |
| Data block (a, k) | bit flip | background pass, or first read | opens, `damaged` on both surfaces, 18 rows fail, rest correct, writes accepted | exit 2 | **PASS** (as decided by the ADR) |
| Damage **while running**: footer, bloom, index, delete, truncate (m2-m6) | applied to a running instance after the pass finished | **not detected while running**; every one refused at the next start with the right code (`SSTABLE_CORRUPT` / `SSTABLE_MISMATCH` / `SSTABLE_MISSING`), directory unchanged | `/readyz` says `complete` and `ready:true` throughout; truncation makes 230 of 1,082 lookups fail | `--data-dir` on the live dir exit 2 | **NOT REQUIRED** (ADR "Failure semantics": a stated limit) |
| Data block while running (m1) | flip after the pass | first read only; the next start's pass reports it | as ADR | exit 2 | **NOT REQUIRED** (same limit) |
| Footer `record_count` lie, CRC fixed (b4) | rewrite 17210 -> 17211 | **nothing at start or in the pass**; `rubixdb check` reports `SSTABLE_CORRUPT: 17210 records readable, footer says 17211` | opens, serves correct data, `sstable_verification:"complete"` | exit 2 | **OPEN** (policy, GAP-B) |
| Bloom zeroed with CRC recomputed (L1) | rewrite | **nothing**: preflight, pass and `check` (exit 0) all clean | opens `ready:true`; all 40,000 lookups fail (the catalog rows lived in that table) | exit **0** | **OPEN** (not covered by ADR or format, GAP-C) |
| Index `last_key` altered, CRC recomputed (L2) | rewrite | nothing | no observable effect in this instance (no key lies between the old and new `last_key`) | exit 0 | **NOT TESTED** (inconclusive probe) |

Online check: `rubixdb check --instance NAME` against a **running** instance reports `0 error(s), check complete`, exit 0, for a deleted live table and for footer/bloom/index damage (m2, m3, m4, m6); it finds data-block damage and truncation (m1, m5, exit 2). Its heading says "online logical check at one snapshot". `check --data-dir` on the same live directory reports the physical damage (exit 2). **OPEN** (GAP-D): the all-clear is truthful about what it checked and misleading about what it did not; the ADR does not cover it.

All disposable: copies under `E:\rubixdb_f08b\runs\`. Compaction over a damaged block (regression of the compaction-leak fix, `m7`): 3 attempts, then `compaction: blocked after 3 consecutive permanent failures`, no leaked `.sst.tmp`, `sstables_on_disk` constant 4. The message names no table (known, recorded 2026-10-08).

## 5. C. Combined WAL and SSTable damage (`scratch/f08_verification/raw/combined`)

Precedence observed (matches ADR-SST-01 and the F-07 code order): `WAL_CORRUPT` -> SSTable preflight -> `WAL_TAIL_DAMAGED`.

| Case | First start | After repairing the SSTable | Changed files |
|---|---|---|---|
| bloom + torn last WAL frame (clean stop) | `SSTABLE_CORRUPT` | `WAL_TAIL_DAMAGED` (log ends at 203, attested 204) | none, either start |
| missing table + torn tail | `SSTABLE_MISSING` | `WAL_TAIL_DAMAGED` | none |
| data-block flip + torn tail | `WAL_TAIL_DAMAGED` (a data-block flip does not refuse) | same | none |
| bloom + corrupt older WAL segment | `WAL_CORRUPT` | `WAL_CORRUPT` | none |
| corrupt older WAL segment only / torn tail only | `WAL_CORRUPT` / `WAL_TAIL_DAMAGED` | - | none |

One error **does mask the other at startup** (the second shows up only after the first is repaired), but no combination leads to an unsafe recovery: all six first starts and all repaired starts changed zero files, `WAL_CLEAN_STOP` was never consumed, and the acknowledged record is never silently dropped. The mitigation exists: `rubixdb check --data-dir` on the both-damaged directory reports both (`SSTABLE_CORRUPT` and `WAL_TORN_TAIL` plus the INFO lines `WAL_TAIL_WOULD_BE_QUARANTINED` and `WAL_TAIL_START_WOULD_REFUSE`). A wording point, not an F-08 item: the check prints `WARNING WAL_TORN_TAIL ... (expected after a crash)` for a directory that was stopped cleanly, which the INFO line next to it contradicts. **PASS** for ordering and safety; masking is by the ADR's design (`PHASE_ITEM_F08_IMPLEMENTATION.md` section 5). No WAL attestation or recovery ordering was changed.

## 6. D. Error classification at the SQL / API boundary (`raw/error_contract/result.json`)

One bit flipped in a data block (rows 8603-8620 in it), instance serving, pass `damaged`:

| Route | Request touching the block | Response |
|---|---|---|
| SQL `POST /v1/sql` | point lookup of a row in it; `COUNT(*)`; range over it | **500 `STORAGE_ERROR`**, message `a storage error occurred`, no detail, no corruption word |
| SQL | lookup in an intact block; `INSERT` | 200 |
| KV `GET /v1/kv/{key}`, `/exists`, `GET /v1/range` | the same keys | **500 `CORRUPTION`**, message `the engine detected on-disk corruption`, detail `block: checksum mismatch` |

Trace: the engine returns `EngineError::Corruption`; the KV/range routes map it to `CORRUPTION` (`api/src/error.rs:135`); the relational layer wraps it as `RelationalError::Engine(_)`, which `sql/src/error.rs` (`From<RelationalError>`) folds into `SqlError::Storage(String)`, which `api/src/error.rs:313` maps to `STORAGE_ERROR` after logging the detail server-side. This is the **documented** design (`PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md` "Error taxonomy": `Storage` is a lower-layer class with a safe text) and ADR-SST-01 lists it as out of scope, so there is **no defect against the written contract**. The consequence is that a SQL client cannot distinguish a damaged block from any other storage failure except through `/readyz` / `/v1/status`. Changing it means a new SQL error code (public contract) in `sql/` and `api/` (not protected paths, no `src/error.rs` change needed). **OPEN — maintainer decision** (GAP-E).

## 7. E. Specification consistency (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`)

| # | Specification says | Implementation does | Evidence | Class |
|---|---|---|---|---|
| E1 | 3.5: the WAL is never purged by a flush, so a corrupt SSTable "can never cause data loss" | Each successful flush calls `pool.purge_before(meta.max_seq)` | `src/lsm/mod.rs` flush job (`pool.purge_before(meta.max_seq)?`); `a`: 18 rows unreadable and `COUNT(*)` fails after a data-block flip | documentation defect (stale since Phase 5; the section itself says to re-examine on purge) |
| E2 | 3.5: "at worst `open` refuses until an operator removes the offending file, after which the engine opens with one fewer table" | A data-block flip does not refuse the open at all; a missing/damaged live table is refused again because the Manifest lists it, with no override | `a`, `g`, `m6` restart, `C2` | documentation defect |
| E3 | 6: "index/bloom: offset out of file bounds" | `footer: bloom block extends past the file` / `footer: index block extends past the file` | `src/sstable/reader.rs:108,115`; `b3` run | documentation defect (text only) |
| E4 | 6: "index: overlapping blocks" | `index: block_offset is not contiguous with the preceding block (gap or overlap detected)` (also rejects gaps) | `src/sstable/format.rs` `decode_index_block`; `d2` run | documentation defect |
| E5 | 6: "footer: file shorter than FOOTER_SIZE" | same text plus ` (<len> < 72)` | `reader.rs:95` | documentation defect (trivial) |
| E6 | 2.7: `record_count` is the total across all data blocks; 6 lists no `record_count` case | Not validated at open or read; `check` errors on a mismatch; the pass ignores it | `b4` | **unresolved policy decision** (GAP-B) |
| E7 | 3.1-3.3: liveness by directory scan, no Manifest | Manifest-authoritative since Phase 5 | `preflight` | documentation defect (outside 3.5, listed) |
| E8 | `src/sstable/mod.rs:6-16` repeats E1 | unchanged | - | documentation defect in a **protected** file; not edited |

Action taken: an additive, dated **ERRATUM** was inserted after section 3.5 of the specification (original text unchanged, section marked superseded for Phase 5 onward). No format, compatibility rule or protected file was touched. E8 needs an authorisation to edit a comment in `src/sstable/mod.rs`.

## 8. Baseline and regression (no code change, so these are the baseline and the result)

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 |
| focused: `cargo test --lib -p rubixdb sstable_integrity` | 18 passed, 0 failed, 1 ignored (the evidence sweep) |
| `cargo test -p rubixdb-api --test sstable_integrity_reporting` | 4 of 4 |
| `cargo test -p rubixdb-cli --bin rubixdb startup_env` | 10 of 10 |
| `cargo test -p rubixdb --lib compaction_failure` / `-p rubixdb-api --test compaction_failure_reporting` | 5 passed, 1 ignored (documents the pre-fix leak) / 2 of 2 |
| `cargo test --workspace --no-fail-fast` (debug) | 1,470 passed, 2 failed, 32 ignored: `repo_hygiene::no_tracked_credentials_json`, `repo_hygiene::no_tracked_file_contains_a_64_hex_admin_key_literal` (tracked credentials file, recorded in `OPEN_ITEMS.md`); includes the F-07 integration suite 16 of 16, `wal_tests`, `pathological_recovery_matrix`, the crash-consistency suites |
| `cargo test --release --workspace --no-fail-fast` | 1,471 passed, 3 failed, 30 ignored: the same two `repo_hygiene` tests and `m1_3_thousand_writers_throughput` at **26,543 ops/s** in the suite (target 80,000) |
| `cargo test --release --test group_commit thousand_writers_throughput` x3, alone | 91,315 / 89,651 / 107,883 ops/s, all passed |

Both counts equal the F-11 final-closure baseline (1,470 / 2 and 1,471 / 3). The M1.3 failure is not described as clean: it failed in the full release suite and passed three of three alone, which is the known load-dependent behaviour recorded in `OPEN_ITEMS.md`; nothing in this mission changed code that could affect it. Full-suite log summaries: `logs/base_*_summary.txt`. The suites were run on the unchanged tree; the erratum and records were written afterwards, so no test ran over them.

## 9. Reproduction (all from `E:\RubiXDb`; harness in `scratch/f08_verification/scripts`, original in `E:\rubixdb_f08b\scripts`, `F08_EXE` = the release binary)

* Open-time matrix: `F08_RAW=...\raw\open F08_WAIT_PASS=1 python f08_repro.py` (after `python build_template.py`). Mid-run: `python f08_midrun.py m1_running_data_block ... m6_running_delete` (and `m7_compaction_over_damaged_block` after `build_template3.py`). Online check: `python online_check_midrun.py`. Combined: `python combined_damage.py`. Error contract: `python d_error_contract.py`. Env, F-07 interaction, slow pass: `python f08_extra.py env f07_after slow_pass_midrun`. Cancel / override: `python cancel_and_override.py`. Semantic probes: `python semantic_lies.py`.
* Throttle: fixture `gen/` (`f08gen <data_dir> 3 1100 4096 16 835518` on an instance created by one start/stop of the binary; scratch crate with `rubixdb` as a path dependency); then `PROBE_THREADS=0 python throttle_measure.py R64_warm_noprobe 64 5` etc.; cold: `python evict.py 12` first. Raw: `raw/throttle/*.json`.
* Unit / integration: the cargo commands in section 8.

## 10. Gaps and what each needs

| Gap | Status | Needs |
|---|---|---|
| GAP-A: is 64 MiB/s an acceptable default? | **OPEN** (decision) | The throttle is real (PASS). Warm: no cost. Cold: p95 x2, p99 x2.4 on random reads in two samples. Maintainer: keep 64, lower it, or keep it and document `...=0`. Writes/compaction concurrent with the pass NOT TESTED. |
| GAP-B: `record_count` mismatch | **OPEN** (policy) | Decide whether a count mismatch with all blocks intact is `damaged` (the stderr line says "the rows ... cannot be read", which would be false), a separate finding, or a downgrade of `check` to a warning. The ADR defers it; the previous mission left it open on purpose. |
| GAP-C: CRC-consistent semantic damage (zeroed bloom: `check` exit 0, whole table invisible) | **OPEN** (design) | A pass-time bloom/index cross-check (hash every key: CPU cost) is a design decision outside the ADR; random corruption is caught by the CRCs. |
| GAP-D: online `check` all-clear without physical checks | **OPEN** (policy) | Either add the physical SSTable checks to the online path, or state the limit in the output. Not in ADR-SST-01. |
| GAP-E: SQL maps corruption to `STORAGE_ERROR` | **OPEN** (contract) | Decision on a typed SQL corruption code (`sql/`, `api/`; no protected path). |
| GAP-F: stale comment in `src/sstable/mod.rs:6-16` | **OPEN** (authorisation) | Protected path; comment-only edit needs an explicit authorisation. |
| Deleted file under a running instance is reported by neither the pass nor `/readyz` | NOT REQUIRED (ADR limit) | The pass treats a vanished file as "retired by compaction" because it has no engine handle to tell the difference. |

NOT TESTED: power loss; a real disk error or full disk during the pass; a database larger than 3.24 GiB; concurrent writes/compaction with the pass; the standalone `rubixdb-api` binary (reports `disabled`); tables written after the start (not scanned until the next start, by design); Linux; a foreground workload other than point reads.

## 11. What was and was not done

Done: inspection, baseline, re-run of the full open-time, mid-run, combined, error-contract and environment matrices on HEAD, a 3.24 GiB throttle and resource measurement (warm and cold), spec comparison, erratum. Not done and why: no code fix, because every defect-shaped finding is either outside the ADR (policy / contract) or in a protected file, which the Fix Policy and the protected-path rule forbid fixing without a decision or authorisation. ADR-STARTUP-01 and F-11 were not touched. No production-readiness claim is made.

---

## 12. ADDENDUM (second pass, 2026-10-11, tree `35bc86e`; sections 1-11 above are unchanged history)

**Premise check.** The second-pass prompt said the first pass left provisional GAP-B (`metadata_findings`) and GAP-E (SQL `CORRUPTION`) changes in the tree. **They do not exist**: HEAD `35bc86e` is docs-only, `git status --short` is empty, `git diff a95e2f4 -- src api cli sql` is empty, and `metadata_findings` / `metadata_mismatch` occur nowhere in the source. The first pass recommended fixing them as a *next step* and never implemented them; no such instruction was received. Therefore there is no provisional change set to verify, label, ratify or revert, GAP-B and GAP-E remain decision requests (12.5), and nothing was built on them.

**12.1 Reuse of first-pass evidence.** Source is byte-identical to `a95e2f4`, so the first-pass runs of F08-01, -02, -05, -06, -07 (sections 2, 4, 5, 6) and F08-03 (section 3, 3.24 GiB fixture, binary SHA-256 `f655de73...bd05d`) stand; the fixture and generator are re-used, not rebuilt. Added below: F08-02/-03 details, F08-04, section-7 audit.

**12.2 F08-02 additions** (`raw/pass_lifecycle.json`, 3.24 GiB fixture, real binary). Transitions `running -> complete` (rate 256: 21.4 s on a cooler cache, 13.4 s after restart); no torn status in 3 runs of about 30 samples each (no non-monotonic counter, `tables_verified <= tables_total`, `/readyz.sstable_verification == /v1/status state`, `ready:true` in every sample); final `bytes_verified` 3,481,188,624 against 3,481,397,709 bytes of table files (0.006 % under: accounted by average block size, bloom/index/footer excluded). Threads 19 during the pass, 18 after it; handles 113-115 during, 111-113 after. Cancel: a graceful stop 15 s into a 4 MiB/s pass took 0.38 s, exit 0, no `rubixdb` process left; the restart starts a fresh pass (state `running` from 0). Invalid values and 0: section 2 AC-7. **PASS.**

**12.3 F08-03 additions.** Partial first/last intervals bias the whole-pass rate by at most 3.5 %: steady-state (t >= 2 s, excluding the last sample) 64.0 / 255.8 / 939.0 MiB/s warm and 165.1 cold, against whole-pass 63.8 / 247.2 / 924.8 / 160.8. Thread count and handles in 12.2. Foreground **write** latency during the pass: **NOT TESTED** (only point reads were probed). Disk pressure beyond the process' read bytes: not sampled. Everything else in section 3 stands. Default unchanged; no throttle policy added.

**12.4 F08-04 — footer `record_count`.** Code: `src/sstable/format.rs:423` parses it, `src/sstable/reader.rs:173` exposes it; `src/ops/check.rs:952` and `src/ops/sstable_integrity.rs:157,486` read it; `src/lsm/mod.rs:2940` sums it into the compaction output's `entry_count_hint`.

| Entry point | Validates it? | Observed with `record_count` 17210 -> 17211, footer CRC valid (`b4`) |
|---|---|---|
| startup preflight | no | opens |
| engine open / normal reads | no | correct data, 1,082 of 1,082 lookups |
| background pass | no (counts records, never compares) | `complete`, no finding |
| `rubixdb check` | yes | `ERROR SSTABLE_CORRUPT 17210 records readable, footer says 17211`, exit 2 |

Both an inconsistency (two detectors disagree) **and** a detection gap (the running instance reports clean).

**New finding, demonstrated: the field is not inert.** Its one non-diagnostic consumer is compaction's bloom sizing, uncapped in the writer (`BloomFilter::new_for_key_count`), while the reader rejects any bloom above 268,435,456 bytes (`format.rs:352`). Isolated fixture (`scripts/recordcount_consumer.py`; 3-table template; SSTable 1 `record_count` set to 400,000,000 with a valid footer CRC; a 4th flush triggers compaction): the instance opened `ready:true` with `sstable_verification:"complete"`; within 70 s compaction wrote **six 516,099,278-byte tables** (3.1 GB) whose bloom (500,064,500 bytes) the reader refuses (`capacity exceeded: requested 500064500, max 268435456`); `compaction: failed, will retry` repeated while `cycles_completed` stayed 0; the inputs 1-4 were left as Manifest orphans; and the **next start was refused** (`SSTABLE_CORRUPT ... capacity exceeded`; `rubixdb check`: `ERROR` on sstable 5-10, `ORPHAN` on 1-4). The recovery path is a verified backup into a new instance; the inputs remain on disk but no repair is authorised.

Reachability: random corruption cannot trigger it (the footer CRC), so it needs a writer bug or a deliberate rewrite. **By arithmetic only (NOT TESTED at honest scale):** an honest database whose summed `record_count` reaches about 214.7 million (268,435,456 x 8 / 10 bits per key; 10 bits per key is what the observed 500,064,500 implies) would hit the same cliff on its next full-merge compaction. The fix location is `src/sstable/` / `src/lsm/` (protected) or an O(1) footer sanity bound in the preflight (policy; "do not add record_count startup validation" is binding in this mission). **No code changed.**

**12.5 Decision requests** (the maintainer ratifies or rejects; this mission implements none).

| | Current behaviour (exact) | Distinction wanted | Options and trade-off |
|---|---|---|---|
| GAP-B `record_count` | table in 12.4: pass `complete`, `check` exit 2 | one agreed meaning | (1) report a mismatch as a non-`damaged` finding (no startup cost, hazard remains); (2) an O(1) bound in the preflight, `record_count x min_record_bytes <= data region` (no scan, closes the compaction hazard, changes what starts); (3) downgrade `check` to a warning (hazard remains); (4) cap the bloom in the writer (protected engine, needs an ADR). 12.4 shows the field is load-bearing. |
| GAP-E SQL code | SQL: 500 `STORAGE_ERROR`, `a storage error occurred`, no detail; KV, `/exists`, range: 500 `CORRUPTION`, detail `block: checksum mismatch` (`raw/error_contract`). No test pins SQL = `STORAGE_ERROR` for corruption; only the KV mapping is asserted (`api/src/error.rs:421`, `api/tests/sstable_integrity_reporting.rs:377`) | a SQL client can tell a damaged block from other storage failures | (1) map `RelationalError::Engine(Corruption)` to `CORRUPTION` in `sql/src/error.rs` and `api/src/error.rs` (public contract, a new SQL code, no protected path); (2) leave and document; clients use `/readyz`. |

**12.6 Section 7 — `rubixdb check --data-dir` safety boundary** (`raw/check_audit/*.json`, `scripts/check_mutation_audit.py`). `check` was run **directly** on stopped disposable directories (not on copies), with a full before/after inventory by SHA-256, size and mtime.

| ID | Directory condition | Exit | Directory modified? |
|---|---|---|---|
| CM0 | clean | 0 | no |
| CM1 | data-block flip (physical errors, logical pass skipped) | 2 | no |
| CM2 | valid foreign table, never recorded | 1 | **YES: `MANIFEST` changed (the table was adopted)** |
| CM3 | stale `*.sst.tmp` | 0 | **YES: the file was deleted and no finding was printed** |
| CM4 | 3 stray bytes after the last Manifest frame | 1 | **YES: `MANIFEST` changed (tail truncated)** |
| CM5 | torn WAL tail (F-07 fix) | 1 | no |
| CM6 | 23 MB un-flushed WAL tail, far above the 4 MiB memtable bound | 0 | no (no recovery flush occurred) |

`rubixdb check --data-dir` is therefore **not** read-only in general: it adopts an unknown table, removes stale temp files and truncates a torn Manifest tail in the directory it inspects (3 of 7 conditions; a recovery flush did not occur in CM6). This turns the 2026-10-10 code-reading residual into a tested result. The WAL clean-stop attestation was never consumed by `check`. Not fixed: the candidate remedy (widen the copy-on-recovery rule of `open_engine_for_inspection`, `PHASE_ITEM_F07_CHECKPATH_ADR.md`) is not authorised. **OPEN.**

**12.7 Classification after the second pass**

| Item | Status |
|---|---|
| F08-01, F08-02, F08-05 (trace), F08-06, F08-07 | PASS (as the first pass; for damage while running, detection is NOT REQUIRED by the ADR limit and the evidence survives restart) |
| F08-03 | PASS for throttle accuracy and read-latency measurement; default acceptability OPEN (GAP-A); write latency NOT TESTED |
| F08-04 | OPEN (GAP-B decision; a newly demonstrated compaction and restart hazard) |
| F08-05 mapping | OPEN (GAP-E decision) |
| F08-08 | PASS (additive erratum); the stale comment `src/sstable/mod.rs:6-16` is OPEN (protected path) |
| Section 7 | FAIL for "check is read-only" (3 of 7 conditions mutate); remedy OPEN |

**Regression.** No code changed since `a95e2f4`, so the first-pass results are the current ones: fmt 0, clippy `-D warnings` 0, focused 18 + 4 + 10 + 5 + 2, debug workspace 1,470 passed / 2 failed (`repo_hygiene` x2), release 1,471 passed / 3 failed (`repo_hygiene` x2 and M1.3 at 26,543 ops/s in-suite; 91,315 / 89,651 / 107,883 alone). Nothing was re-run because nothing relevant changed; this is stated, not presented as a new pass. Not committed (instruction: no commit unless asked).

---

## 13. GAP-B: O(1) bound on the footer `record_count` (2026-10-11, base `35bc86e`; sections 1-12 above are unchanged history)

**Authorisation.** The maintainer's mission prompt "GAP-B — O(1) SSTable `record_count` Preflight Bound" (2026-10-11), sections 0 and 3: option (2) of 12.5, with protected-path edits limited to `src/sstable/bloom.rs` (the helper) and `src/sstable/reader.rs` (the call site). No other protected file was touched (`git diff --stat -- src/wal src/manifest src/compaction src/error.rs src/lsm` is empty). No separate ADR file was written; this section is the record.

**13.1 Derivation (from the code, not copied).** Writer sizing, `BloomFilter::new_for_key_count`: `num_bits = max(1, saturating(expected_keys * bits_per_key))`, bytes `= ceil(num_bits / 8)`; the product is `u64`, the bytes are cast to `usize`. Reader cap, `format::decode_bloom_block`: `checked_len(ceil(num_bits / 8), 256 * 1024 * 1024)`: it applies to the **bit array only** (not the 8-byte `num_bits`, the 1-byte hash count or the 4-byte checksum) and refuses with `EngineError::CapacityExceeded`, strictly above 268,435,456. `ceil(n / 8) <= C` holds exactly when `n <= 8 * C`, so the largest safe count is `floor(8 * C / bits_per_key)`. At the pinned 10 bits per key (spec 2.5, `DEFAULT_BLOOM_BITS_PER_KEY`): `floor(2,147,483,648 / 10) = 214,748,364`. Proof of tightness: 214,748,364 x 10 = 2,147,483,640 bits = 268,435,455 bytes (fits); 214,748,365 x 10 = 2,147,483,650 bits = 268,435,457 bytes (refused by the reader). `8 * C = 2^31` fits `u64`; `bits_per_key == 0` sizes a 1-bit filter, so every count is safe; the largest `bits_per_key` yields 0.

**13.2 Change.**

| File | Change | Class |
|---|---|---|
| `src/sstable/bloom.rs` | `MAX_BLOOM_BIT_ARRAY_BYTES` (mirrors the reader literal), `bloom_num_bits` (the single sizing formula, now used by `new_for_key_count`), `max_record_count_for_bloom_cap(bits_per_key)`; 4 unit tests | **protected, authorised** |
| `src/sstable/reader.rs` | `SsTable::open`: after the footer and offset checks, before the bloom block is read or allocated, `record_count > bound` returns the existing `EngineError::Corruption` with a detail naming `record_count`, the value, the bound and the 268435456-byte reader limit | **protected, authorised** |
| `src/ops/sstable_integrity_tests.rs` | one new test, existing assertions untouched | product layer, test |
| `tests/sstable_record_count_bound.rs` | new integration test file (6 tests, counting allocator) | test |

The preflight (`src/ops/sstable_integrity.rs`) was **not edited**: it already calls `SsTable::open` and maps its error to `SSTABLE_CORRUPT` carrying the engine text, so the new refusal arrives through the existing path with the directory untouched. Not changed: `FOOTER`/`bloom`/`index` byte formats, the public `SsTable` API, WAL, Manifest, checkpoint, purge, startup order, `rubixdb check`, `/readyz`, `/v1/status`, the 64 MiB/s default. Footer CRC, index, bloom and structural checks still run exactly as before; the new check is an extra, earlier comparison of one integer. `bits_per_key` for the bound is the format-pinned 10; a library caller that wrote a table with fewer bits per key and more than 214,748,364 records would now be refused (no product path does that; the product uses the default).

**13.3 Tests.**

| Command | Result |
|---|---|
| `cargo test -p rubixdb --test sstable_record_count_bound` | 6 passed |
| `cargo test -p rubixdb --lib sstable` (baseline 88 passed, 1 ignored) | 93 passed, 1 ignored |
| `cargo test -p rubixdb --lib sstable_integrity` (baseline 18, 1 ignored) | 19 passed, 1 ignored |
| `cargo test -p rubixdb --lib compaction` (baseline 47, 1 ignored) | 47 passed, 1 ignored |
| `cargo test -p rubixdb-api --test sstable_integrity_reporting` | 4 passed |
| `cargo test -p rubixdb-cli --test f07_tail_damage_integration` | 16 passed |
| `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 / exit 0 |
| `cargo test --workspace --no-fail-fast` (debug) | 1,480 passed, **3 failed**, 32 ignored |

Workspace failures: `repo_hygiene` x2 (baseline, a tracked credentials file) and **`observability_integration::two_real_instances_keep_every_surface_apart_and_a_restarted_one_starts_fresh`**, a failure not in earlier baselines: it asserted that two instances report different start times and both reported `1791713053286` (two servers started in the same millisecond; the test then leaked its two servers, which were stopped by hand). It is unrelated to SSTables (observability start-time bookkeeping; this change only adds a comparison in `SsTable::open`). It passed 6 of 6 on immediate re-runs (`logs/gapb/`). The first failure is kept as the evidence; it is **intermittent**, not "clean". The release workspace suite was **not run** (not requested; the debug workspace was).

Non-vacuity: against the unfixed `HEAD` source the new integration file fails 5 of 6 (the control that a normal table opens passes) and the new preflight test fails (`unwrap_err` on an `Ok`). Mutants of the fix, all killed (6 of 6, `logs/gapb/gapb_mutants.log`): `>` to `>=` (the bound is refused), `>` to `> max+1` (the next integer is accepted), helper divisor `+1`, helper cap x4, sizing loses `max(1)`, and the check moved after the bloom block is decoded.

**13.4 `400,000,000` is rejected before any unsafe allocation or encoding.**

* Allocator proof (`tests/sstable_record_count_bound.rs`): with a counting global allocator, `SsTable::open` of the honest 500-record table makes a largest single allocation under 1 MiB (control); the same table with `record_count = 400,000,000` and a **valid** footer CRC returns `EngineError::Corruption` (detail names `record_count`, `400000000`, `214748364`), with a largest single allocation under 1 MiB, against the 500,000,000 bytes a bloom for that count would be; the file is byte-identical afterwards.
* Order proof: with the bloom block also damaged, the refusal is the `record_count` one (the bloom was never read or decoded); with every data-block byte wrecked, `open` still succeeds for an honest count (it reads no data block) and refuses the inflated count identically.
* Real binary (`rubixdb_gapb.exe`, release, SHA-256 `35c87fea...a59`, `raw/gapb_real_binary.json`, 3-table disposable copy, SHA-256 inventory of 9 files before and after): `400,000,000` and `214,748,365` -> exit 1 in 1.1-1.8 s, `SSTABLE_CORRUPT ... footer: record_count 400000000 exceeds 214748364, the largest count whose bloom filter fits the 268435456 byte reader limit; refusing to open ...`, `directory_unchanged: true`, no new file, no compaction, no 3 GB of output. `214,748,364` and the `b4` value `17211` still open (a mismatch below the bound is not detected, by design of this mission).

**13.5 O(1).** The check compares one already-decoded `u64` footer field with one `const fn` result (a division by a constant); it runs after the 72-byte footer read the open already performed, before the bloom read, with no allocation, no loop, no data-block read, no thread. Startup cost therefore does not depend on data size (the preflight reads each table's footer, bloom and index exactly as before).

**13.6 Remaining limitation, demonstrated: the bound is per table; compaction sums.** `lsm/mod.rs:2940` sums the inputs' counts into the output's bloom size. With `record_count` set to the **bound itself** (214,748,364, accepted) on one of four small tables, compaction on the fixed binary still wrote nine 284,534,733-byte tables whose bloom (268,499,955 bytes, 64,499 over the cap) the reader refuses (`capacity exceeded: requested 268499955, max 268435456`), `cycles_completed` stayed 0, and the next start was refused (`raw/recordcount_consumer_214748364.json`). So the O(1) bound closes the `400,000,000` shape (any single count above 214,748,364) but **not** the summed shape: a database whose inputs' counts add up to more than 214,748,364 at compaction time (honest or not) still produces a table the reader refuses. This is the case 12.4 derived by arithmetic; it is now also reproduced. The remedy is a clamp of the hint at its one call site (`src/lsm/mod.rs`, `entry_count_hint.min(max_record_count_for_bloom_cap(bits_per_key))`, same helper) or a writer cap; both are outside this mission's authorised files.

**13.7 Still open.** Full `record_count` mismatch detection (a count below the bound that disagrees with the stored records: opens, serves, `check` exit 2); the pass does not report it. GAP-A (default rate), GAP-C (CRC-consistent semantic damage), GAP-D (online check all-clear), GAP-E (SQL code), GAP-F (stale comment `src/sstable/mod.rs:6-16`, protected) and the `rubixdb check --data-dir` mutation finding are untouched. F-08 is not complete; no production-readiness claim.

---

## 14. GAP-B follow-up: the summed compaction hint (2026-10-11, base `35bc86e` + the section-13 changes; sections 1-13 above are unchanged history)

**Status: the fix is designed, validated in a scratch copy, and NOT applied to the repository.** The shared helper `max_record_count_for_bloom_cap` is unreachable from `src/lsm`: `mod bloom;` is private in `src/sstable/mod.rs` (only `BloomFilter` is re-exported). Using the helper, as the mission requires ("so the sizing rule is not duplicated"), needs a one-line `pub use` in `src/sstable/mod.rs`, which the mission excludes ("no change to `src/sstable/`"; "if the smallest correct change requires touching `src/compaction/` or `src/sstable/` beyond reading, stop and report"). So the mission stopped at that rule. `git diff HEAD -- src/lsm src/compaction src/sstable/mod.rs` is empty. The complete patch, its tests and the evidence are in `scratch/f08_verification/proposed_summed_hint/`.

**14.1 What `entry_count_hint` controls (proved from the code, before choosing a remedy).** The hint has one consumer in the writer, `write_from_sorted_records`: `BloomFilter::new_for_key_count(entry_count_hint.max(1), bits_per_key)` (`src/sstable/writer.rs:150`). It does not control record inclusion (`for item in records` writes every record), the persisted `record_count` (`total_records`, counted in that loop and written to the footer), or key insertion (`bloom.insert(&key)` runs for every record, unconditionally). Capacity cannot make construction fail or omit a key: `bit_positions` takes `% num_bits` with `num_bits >= 1`, `bits.len() == ceil(num_bits / 8)`, and insert and query use the same positions, so every inserted key has all its bits set whatever the size; a smaller filter only collides more. The reader uses the bloom in exactly two places, `get_versioned` (`reader.rs:233`) and `contains` (`reader.rs:302`), both of which answer "absent" only when a bit is clear, which cannot happen for an inserted key; `range_scan_raw` never consults it and there is no separate block-scan fallback. So an undersized bloom changes the false-positive rate, never a read result. In addition, the chosen remedy never undersizes below the output's need (14.4). The hint feeds nothing else (`grep entry_count_hint`: `lsm/mod.rs:2940,2963`, `sstable/writer.rs:80,90,113,132,150`, tests).

**14.2 Section 2.1: the "nine tables" are not a leak; they are nine committed outputs** (fresh fixture, `scripts/nine_tables.py`, `raw/nine_tables_214748364.json`, binary of the previous mission, table 1 `record_count` = 214,748,364, 24,000 rows to force a fourth flush, sampled every 5 s for 60 s).

| Observation | Value |
|---|---|
| attempts | one compaction per attempt, about every 5-7 s; `failures_total` 1 (first sample) -> 8 (last), `consecutive_failures` the same, `cycles_completed` 0 |
| per attempt | one complete output of 284,534,733 bytes was written, renamed (so `TmpFileGuard` was disarmed, no `.sst.tmp` leaked) **and committed to the Manifest (`AddSstable`, then `RemoveSstable` for each input)**; only then did `SsTable::open(output)` fail with `capacity exceeded: requested 268499955, max 268435456`, before the in-memory splice |
| files over time | 6, 6, 7, 8, 8, 9, 10, 10, 11, 12, 12, 13 (inputs 1-4 plus up to nine outputs) |
| in-memory view | `live_sstable_count` stayed 4 (stale), so every retry compacted the same inputs again |
| classification | `last_failure.kind: "other"`: `CapacityExceeded` falls in "any other error", i.e. **transient**; `blocked: false` throughout; stderr: `compaction: failed, will retry on the next trigger` repeated, no table named |
| end state | `rubixdb check`: `ERROR SSTABLE_CORRUPT` on sstable 5-13, `WARNING SSTABLE_ORPHAN` on 1-4, exit 2; the next start refused |

So the failure mode is **commit before verify** (the output is opened only after the Manifest commit) combined with a **transient** classification: an unreadable output is committed again every ~5 s, about 285 MB each, with no block and no cap, until something else stops it (disk full). It is not the ADR-COMPACTION-LEAK-01 shape (that guard covers `.sst.tmp`). The clamp removes this trigger; the general ordering and the transient classification are **not** changed by it (open finding, 14.7).

**14.3 Section 2.2: the observability failure is pre-existing and intermittent.** `observability_integration::two_real_instances_keep_every_surface_apart_and_a_restarted_one_starts_fresh` asserts the two instances report different start times. Both spawn at once (`spawn_owner` twice back to back) and the assertion fails when both report the same millisecond.

| Tree | Build | Runs | Failures | Signature |
|---|---|---|---|---|
| baseline: `git archive` of `35bc86e` (identical code to `a95e2f4` for every non-doc file), separate target dir | debug | 40 sequential | **1** (run 35) | `observability_integration.rs:403`, `left: Number(1791714347198)`, `right: Number(1791714347198)` |
| current tree (section-13 changes) | debug | 40 sequential, plus 6 earlier re-runs | 0 of 46; plus 1 of 1 in the section-13 workspace run | the same line and kind (`1791713053286` on both sides) |

Both failures leave two leaked servers with identical creation timestamps, as seen. The rates (1/40 baseline; 1/47 on the current tree counting the workspace run) are not distinguishable. The section-13 change does not touch code the test exercises: the test's instances are fresh and have no SSTable, so `SsTable::open` never runs, and nothing it adds concerns timestamps. **Classification: pre-existing intermittent** (reproduced on unmodified code with the same signature). It is not caused by section 13. Caveat: the baseline and current runs used separate build directories on the same machine, not simultaneous runs.

**14.4 Remedy: A (clamp) plus a pre-commit output guard.** A alone closes the demonstrated trigger but not the following: with the per-table open bound of section 13, an output whose real survivor count exceeds 214,748,364 has a footer count that `SsTable::open` refuses, and it is only opened after the Manifest commit, so the same commit-before-verify failure occurs with the cause moved from the hint to the real count. Remedy B (refuse whenever the *sum* exceeds the cap) is safe but blocks compaction for a database whose inputs carry many overwritten versions (the sum counts versions and tombstones that the merge drops). Chosen, in `compact_once_impl` only:

1. `entry_count_hint = saturating_sum(inputs' record_count).min(cap)` where `cap = max_record_count_for_bloom_cap(max(bloom_bits_per_key, 10))` (never above what `open` accepts). The sum saturates (a wrapped sum could become a small hint); the clamp is documented in code as intentional. With the guard below, an output that gets this far holds at most `cap` records, so a bloom sized for `cap` keys is **not** undersized for it: the false-positive rate is the design rate or better.
2. After the output is written and before the Manifest commit: if its real `record_count > cap`, remove the output file (nothing references it) and return `EngineError::Unsupported` (an existing permanent-candidate class, so the worker blocks after its 3-attempt budget instead of retrying forever). No new variant, no format change.

**14.5 Validation in the scratch copy (`E:\rubixdb_f08b\nv_fix`, current tree + patch; not the repository).**

| Check | Result |
|---|---|
| unit tests of the pure functions (cap, saturating sum, boundary, `u64::MAX`) | 4 passed |
| `tests/compaction_summed_hint.rs` (real engine, background worker): normal compaction (bloom framing `13 + ceil(total x 10 / 8)` unchanged); summed hint exactly at the cap; cap + 1; survivors above a scaled cap (`bloom_bits_per_key` 3,000,000 -> cap 715, 1,428 survivors): refused before the commit, worker `blocked`, `cycles_completed` 0, no output file left, **Manifest bytes unchanged**, all keys still readable | 4 passed (61 s; the three big cases write 256 MiB blooms) |
| non-vacuity | against the unfixed call site the cap + 1 case FAILS (the at-cap case and the normal case pass: controls) |
| mutants, 6 of 6 killed | clamp removed (integration + 2 unit); clamp applied per input before the sum (integration + 2 unit); wrapping sum (unit); guard `<=` -> `<` (unit); guard never refuses (integration + unit); guard call removed from the compaction path (integration) |
| real binary, scratch build, SHA-256 `fe1ac6db...564db`, same fixture as 14.2 | `cycles_completed` 1, `failures_total` 0, `blocked` false, 1 live table, 1 file in `sstables/`, `rubixdb check` exit 0 with 0 errors and 0 warnings (the previous build: nine unreadable tables) |
| scratch regression | `--lib compaction` 47 passed + 1 ignored; `--lib lsm` 127 + 1; `--lib sstable` 93 + 1; `sstable_record_count_bound` 6; `sstable_integrity_reporting` 4; scratch `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings` exit 0 (after moving the helpers above the doc comment, which clippy required) |

The scratch workspace test run and the release workspace run were not done. The guard's wiring is tested end to end only at the scaled cap; at the real cap (214.7 million survivors) it is **NOT TESTED**.

**14.6 Authorisation needed to apply it** (the maintainer; the smallest set): (1) the one-line re-export `pub use bloom::{max_record_count_for_bloom_cap, BloomFilter};` in `src/sstable/mod.rs`; (2) the `src/lsm/mod.rs` edit (about 45 lines including three tiny pure functions and their unit tests). Reproducible with `scripts/patch_lsm_scratch.py <repo>` followed by `cargo fmt`, which is byte-identical to the validated scratch copy (verified); the new test file `tests/compaction_summed_hint.rs` goes beside it.

**14.7 Findings this follow-up adds, not fixed.** (a) Commit-before-verify in `compact_once_impl`: the output is opened after `AddSstable`/`RemoveSstable` are committed; any other open failure of the output (not only this one) commits an unreadable table and, being classified transient, repeats it without a block. (b) `EngineError::CapacityExceeded` is classified transient by `CompactionFailureKind::classify`. Both are engine behaviour outside this mission. (c) Format limit made visible: with a single full-merge output and a 256 MiB bloom cap, a database whose compaction survivors exceed 214,748,364 records can no longer be compacted (with the guard it blocks safely; read amplification grows, as ADR-COMPACTION-LEAK-01 already accepts). Remaining from sections 12-13: counts below the bound that disagree with the stored records are undetected at start; GAP-A, C, D, E, F and the `rubixdb check --data-dir` mutation are untouched.

---

## 15. GAP-B summed hint: the validated fix APPLIED (2026-10-11; base `35bc86e` + the section-13 changes; sections 1-14 above are unchanged history)

**Authorisation.** The maintainer's mission "RubixDB GAP-B — Apply the Validated Aggregate Compaction Fix" (2026-10-11): (1) the re-export in `src/sstable/mod.rs`, (2) the clamp and the pre-commit guard in `src/lsm/mod.rs`, (3) `tests/compaction_summed_hint.rs`, (4) the records. `src/sstable/reader.rs` was not touched in this mission (its diff is the section-13 change only). Nothing was committed or pushed.

**15.1 Independent review before applying.** The scratch patch was re-read line by line, not trusted. Confirmed from the code: the aggregate is `fold(0, saturating_add)` and only then `.min(cap)` (a wrapped sum cannot become a small hint; the unit tests include `u64::MAX + 2`); the guard (`lsm/mod.rs:2992`) sits after `write_from_sorted_records` returns and **before** the first `Manifest` write (`AddSstable`, line 3016), so a refused output is never committed; `EngineError::Unsupported` is classified permanent-candidate by `CompactionFailureKind::classify` (`is_permanent()` true), so the worker blocks after its existing budget; the writer still inserts every key and counts every record whatever the hint is, so the persisted `record_count` and the bloom's contents are unchanged. **One deviation from the scratch patch, found in review:** the guard removes a file that already has its published `.sst` name; the writer makes its rename durable with `fsync_dir`, so the removal now does the same (`let _ = wal::fsync_dir(sstables_dir);`, best effort, three lines), otherwise an OS crash in that window could bring an unreadable table back as an unrecorded one. On Windows that helper is a no-op; the Unix effect is **NOT TESTED**. The result is otherwise byte-identical to the validated scratch copy (diffed).

**15.2 Changed files.**

| File | Change | Class |
|---|---|---|
| `src/sstable/mod.rs` | `pub use bloom::{max_record_count_for_bloom_cap, BloomFilter};` (one line) | **protected, authorised** |
| `src/lsm/mod.rs` | three pure functions (`compaction_record_cap`, `capped_bloom_hint`, `compaction_output_fits`), the capped hint at the former `entry_count_hint` line, the pre-commit guard, 4 unit tests | **engine, authorised** |
| `tests/compaction_summed_hint.rs` | new, 4 integration tests | test |
| `PROGRESS.md`, `OPEN_ITEMS.md`, `CHANGELOG.md`, this document, `scratch/f08_verification/` | records and evidence | docs |

Earlier uncommitted section-13 changes (`bloom.rs`, `reader.rs`, `sstable_integrity_tests.rs`, `tests/sstable_record_count_bound.rs`) were preserved. Not changed: `reader.rs` in this mission, the SSTable format, the reader's cap, WAL, Manifest, checkpoint, purge, the failure classification, the worker.

**15.3 Verification (repository, exit codes 0 unless stated).**

| Command | Result |
|---|---|
| `cargo test -p rubixdb --lib summed_hint` | 4 passed |
| `cargo test -p rubixdb --test compaction_summed_hint` | 4 passed (61 s) |
| `cargo test -p rubixdb --test sstable_record_count_bound` | 6 passed |
| `cargo test -p rubixdb --lib sstable` | 93 passed, 1 ignored |
| `cargo test -p rubixdb --lib sstable_integrity` | 19 passed, 1 ignored |
| `cargo test -p rubixdb --lib compaction` | 47 passed, 1 ignored |
| `cargo test -p rubixdb --lib lsm` | 127 passed, 1 ignored |
| `cargo test -p rubixdb-api --test sstable_integrity_reporting` | 4 passed |
| `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 / exit 0 |

Non-vacuity and mutants are those of section 14.5 (unfixed call site fails the cap + 1 case; 6 of 6 mutants killed); they ran on the scratch copy and were not re-run on the repository, whose code differs by the three fsync lines only. The scratch real-binary result is superseded by the repository build below. **Not run:** the debug and release workspace suites (not required by this mission); `f07_tail_damage_integration` (untouched code path).

**15.4 The nine-table scenario on a build of the repository** (release, SHA-256 `242671371aae...6004d`, built into `E:\rubixdb_f08b\apply_target` because `target\release\rubixdb.exe` was in use by another running console; disposable 3-table copy, table 1 `record_count` = 214,748,364, 24,000 rows to force the fourth flush, `scripts/nine_tables_port.py`, `scripts/verify_output.py`). The disposable instance was pinned to port 3302 because a separate `rubixdb gui` (not started by this session) holds port 302 on this machine; it was not touched (my first attempt reached it with requests that returned 401 and wrote nothing).

| Check | Before (previous build) | After (this build) |
|---|---|---|
| compaction | 9 outputs committed in 60 s, `failures_total` 8, `cycles_completed` 0, `blocked` false | `cycles_completed` 1, `failures_total` 0, `blocked` false, 40 s of samples unchanged |
| files in `sstables/` | 13 (4 inputs + 9 unreadable outputs) | 1 (284,470,233 bytes) |
| stderr | `compaction: failed, will retry ...` repeated | none |
| output footer | - | `record_count` 68,810 (the real count, not the hint), `bloom_length` **268,435,468 = 13 + 268,435,455**, exactly the clamped bound |
| `rubixdb check` (stopped) | exit 2, 9 errors | exit 0, 0 errors, 0 warnings |
| restart | refused | ready in 0.6 s |
| reads | - | `COUNT(*)` 84,000 (= 60,000 + 24,000); 28,001 sampled present keys all correct (0 wrong); 2,499 absent keys all return no rows |
| `check` after the restart | - | exit 0, 0 errors, 0 warnings |

(A first run of my verification script reported 2,000 "unexpected" rows among its absent keys; that was a wrong expectation in the script, ids 50000-51999 exist in the 60,000-row fixture. The script was corrected and rerun: `raw/apply_verify_output.json` is the first run, `raw/apply_verify_output2.json` the corrected one.) The zero false negatives over 28,001 present keys against a bloom sized for the cap is the behavioural evidence that the clamped bloom is correct; the argument that it must be is in 14.1.

**15.5 The real-cap guard is NOT TESTED.** The guard (an output with more than 214,748,364 real records is refused before the commit) has been exercised only through the scaled test in `tests/compaction_summed_hint.rs` (`bloom_bits_per_key` 3,000,000, cap 715, 1,428 survivors): same code path, a different cap. That is **not** equivalent to the production bound and is not presented as such. A test at 214.7 million survivors was not run.

**15.6 Remaining.** Counts below the per-table bound that disagree with the stored records are still not detected at start or by the verification pass (`rubixdb check` reports them). A database whose compaction survivors exceed 214,748,364 records now blocks compaction safely (permanent-candidate failure, nothing committed) instead of committing an unreadable table; read amplification then grows. The ordering "output opened after the Manifest commit" and the transient classification of `CapacityExceeded` are unchanged and remain open for any other cause of an unreadable output. The `fsync_dir` line is untested on Unix. Untouched: GAP-A, C, D, E, F, the `rubixdb check --data-dir` mutation finding, the intermittent observability test. F-08 is not complete; no production-readiness claim.
