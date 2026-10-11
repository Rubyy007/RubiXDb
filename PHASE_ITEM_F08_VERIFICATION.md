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
