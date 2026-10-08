# F-08 — a damaged SSTable: implementation of ADR-SST-01 (P4)

**Date:** 2026-10-08. **Base tree:** `5910dd3` (clean `git status --short` at the start; `git log -3`: `5910dd3` F-08 compaction-leak implementation / `bb46d88` compaction-leak discovery + ADR / `13dc534` F-08 discovery + ADR-SST-01). **Status:** implemented, tested, committed locally, not pushed. Nothing under `src/wal/`, `src/manifest/`, `src/error.rs`, `src/compaction/`, `src/sstable/` or `src/lsm/` changed (`git diff --stat` empty for all six) - ADR-SST-01 authorises no engine change and none was needed. The previous mission's compaction-leak change (`TmpFileGuard`, compaction failure state) is untouched and its suite still passes.

Raw evidence: `E:\rubixdb_f08\raw\impl\` (`before\`, `after\`, `before_after_matrix.txt`, `endpoint_diff.json`, `sweep_result.tsv`, `mutation_check.txt`, `mutation_check2.txt`); scripts: `E:\rubixdb_f08\scripts\impl\`; regression logs: `E:\rubixdb_f08\logs_regress_impl\`.

| Binary | SHA-256 |
|---|---|
| before (`cargo build --release --locked -p rubixdb-cli` at `5910dd3`) | `f59f063e672fa384dd6f7edc8d3f09faf07ede5838a8f93dd8708fc4b7a0ed47` |
| after (same command, this change) | `be205bb0fffa67e63f84c113c5752055a0bbcd3ba976d1031b4a8dc53a8d4bb4` |

## 1. The ADR's Decision (verbatim) and what was implemented

> In the product layer only: (1) extend the startup-only guard with a read-only SSTable preflight that runs before the F-07 attestation is consumed and before the engine opens - every Manifest-live table must exist, match the Manifest's recorded size and sequence range, and pass the same footer / bloom / index validation the engine runs, otherwise the start is refused (exit 1, directory unmodified, no override); and (2) after the server is serving, verify every data block of the tables that were live at start in a throttled, cancellable background pass whose state and findings are reported through additive fields of `/readyz` and `/v1/status`, a security-log event and stderr, without ever changing `ready`.

Policy P4 was implemented as approved. The four acceptance criteria of the brief are each named by the Decision: a startup-only preflight before the attestation is consumed; a Manifest-authoritative comparison (size and sequence range); a throttled background verification; additive reporting on `/readyz` and `/v1/status`. The brief's "reject if" tests all passed on reading. **One thing the ADR does not supply and the brief asked about: the throttled default (64 MiB/s) is an unvalidated proposal** - see section 9.

## 2. Reproducer on the CURRENT tree, before any change (real numbers)

Same fixture as the discovery (`runs\template`: 40,000 rows, 2 live SSTables of ~4.03 MB with 956 data blocks each). Binary `f59f063e...`. `raw\impl\before\`, `raw\impl\before_after_matrix.txt`. **The numbers match the discovery; no material divergence** (open-time: 17 scenarios, same 11 refusals, same four opens; mid-run: same six outcomes). The one difference from the discovery is expected and not a divergence: the compaction case m7 now follows ADR-COMPACTION-LEAK-01 (3 attempts, `blocked`, no leaked `.sst.tmp`).

| Category | Scenario | Current behaviour (before) |
|---|---|---|
| (a) bit flipped in a middle data block | `a_data_block_bitflip`, `k_data_block_other_table` | **opens**, exit n/a, `/readyz` `{ready:true, storage_state:Healthy, index_recovery:complete}` (no integrity field exists anywhere); the 18 rows of the block fail every read (HTTP 500), the other 1,082 / 1,081 sampled lookups are correct, `COUNT(*)` 500, `INSERT` 200; `WAL_CLEAN_STOP` consumed (changed_files `WAL_CLEAN_STOP`, `wal-...3.log`) |
| (b) valid-but-wrong file (Manifest size / sequence range not compared) | `h_swapped_valid_file` | **opens** `ready:true`; 1,082 of 1,082 lookups fail, `COUNT(*)`, `INSERT` HTTP 404 `NOT_FOUND` (the catalog rows are gone); `WAL_CLEAN_STOP` consumed |
| (c) footer / bloom / index damaged AFTER open | `m2`, `m3`, `m4` (and `m6` file deleted) | invisible while running: 18 of 18 and 1,082 of 1,082 lookups correct, `/readyz` unchanged; the next start is refused (`footer: bad magic` / `bloom: checksum mismatch` / `index: checksum mismatch` / `recorded live but ... is missing`) |
| (d) mid-run detection of any of these | `m1` (data block), `m5` (truncate) | by the first read of that block only (m1: 18 errors, rest correct, `COUNT(*)` 500; m5: 230 of 1,082 lookups fail); nothing on any health surface; the engine-side refusal after restart (m5) |
| open-time refusals (11 scenarios) | b1-b3, c, d1, d2, e1, e2, f, g, j | exit **1** with `engine open failed: ... <path>: <detail>`; the only file changed is **`WAL_CLEAN_STOP`** (consumed by the F-07 guard before the engine's SSTable checks) - 11 of 11 `directory_unchanged=False` |
| `b4` footer `record_count` lie, `i` valid foreign table | b4, i | open and serve correct data (by design: informational field; the engine adopts a valid foreign table) |

## 3. What changed, by file

| File | Change | Mechanism |
|---|---|---|
| `src/ops/sstable_integrity.rs` (new) | `preflight(dir)`; `SstableIntegrity` state; `run_verification`; `parse_verify_mib_per_sec` | **Preflight** (read-only): `manifest::replay_readonly`; for every live id, in order: file exists (`SSTABLE_MISSING`, engine's text), length equals the recorded `file_size` unless the record is `0` = unknown (`SSTABLE_MISMATCH`), `SsTable::open` (the engine's own function: footer, bloom, index; `SSTABLE_CORRUPT` carrying the engine's message verbatim), footer `min_seq`/`max_seq` equal the recorded ones (`SSTABLE_MISMATCH`); then every other published `*.sst` that is neither live nor ever recorded (the ones the engine adopts) must open. `*.sst.tmp` and removed-but-undeleted tables are left to the engine; an unreadable Manifest refuses with the replay's message. **Verification pass:** one thread, per table its own `SsTable::open` handle and `range_scan_raw` over every block (the scan `rubixdb check` performs), a bytes-per-second pace against an absolute clock (accounted as blocks read x the file's average block size; consulted every 256 KiB), cancellable at the next block; a table absent at open is skipped as retired; the first error of a table is recorded as `{id, sstables/<file>, records_before_failure, records_total}`; nothing is persisted. |
| `src/ops/format.rs` | `startup_guard_with_tail_policy` calls `preflight` after the unchanged `WAL_CORRUPT` preflight and **before** `apply_tail_policy`; `StartupGuard.sstables` carries the validated tables | A start refused by the preflight has not consumed `WAL_CLEAN_STOP`. The old `startup_guard` (used by `rubixdb check`, `restore`) is unchanged. |
| `src/ops/mod.rs` | module registration, three refusal codes | `SSTABLE_CORRUPT`, `SSTABLE_MISSING`, `SSTABLE_MISMATCH` |
| `cli/src/startup_env.rs` | `RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC` | the strict `RUBIXDB_LOCAL_*` parser: digits only; unset/empty = 64; `0` = disabled; max 1024; anything else stops startup naming the variable before anything is created (also part of `load()`) |
| `cli/src/host.rs` | starts the pass after readiness, cancels it on shutdown, joins it before the engine stops | same pattern as the post-start index recovery; one stderr line per damaged table |
| `api/src/sstable_integrity.rs` (new), `api/src/lib.rs`, `api/src/state.rs` | `spawn_sstable_verification`; `AppState.sstable_integrity` | the state is `running` before the spawn returns; the `sstable.damaged` security event (`object` = `table=<id> records_before=<n>`, outcome `failed`, object_kind `sstable`) is emitted from the pass thread |
| `api/src/routes/health.rs`, `status.rs` | `/readyz.sstable_verification`, `/v1/status.sstable_integrity` | additive; `ready` is still `crate::observability::sampler::ready()` (constant `true`, decision D5) |

The standalone `rubixdb-api` binary (`api/src/main.rs`) is unchanged: it shares the guard, so it gets the preflight, and reports `sstable_verification: disabled` (it does not start the pass). The ADR names the host only.

## 4. After the change, same fixture (binary `be205bb0...`)

| Category | After |
|---|---|
| (a) data block flip | still **opens** (the ADR keeps the instance serving): `/readyz` `{ready:true, storage_state:Healthy, index_recovery:complete, sstable_verification:"damaged"}`; `/v1/status.sstable_integrity` `{state:"damaged", tables_total:2, tables_verified:2, bytes_verified:6044788, damaged:[{id:1, path:"sstables/00000000000000000001.sst", records_before_failure:8612, records_total:17210}]}`; one stderr line; one `sstable.damaged` event (`table=1 records_before=8612`) in `security.log`. Reads unchanged: 18 errors in the block, 1,082 of 1,082 other lookups correct. The pass took 15 ms of wall time for 8 MB at the default 64 MiB/s. `k` (second table): `damaged:[{id:2, 5724, 17200}]`. |
| (b) valid-but-wrong file | **refused**, exit 1, `directory_unchanged=True`: `engine open refused: SSTABLE_MISMATCH: sstable <path>: the file is 4030165 bytes but the Manifest recorded 4030612; refusing to open: ...`. |
| (c) footer / bloom / index damaged after open | still not detected while running (stated limit of the ADR: held in memory). The next start is refused by the preflight, directory unmodified (m2-m4: `SSTABLE_CORRUPT`; m5 truncate: `SSTABLE_MISMATCH`; m6 deleted: `SSTABLE_MISSING`). |
| (d) mid-run detection | data-block damage is found by the pass if it has not yet been read: `slow_pass_midrun` (budget 1 MiB/s, one bit flipped in block 318 of the not-yet-scanned SSTable 2, 0.01 s after ready): `/readyz` was `running` at the flip and `damaged` 4.77 s later, `/v1/status` named table 2 (5724 of 17200 records), the 18 rows failed typed while 190 of 190 sampled lookups were correct, restart opened and the pass reported the same finding again. With the default 64 MiB/s the whole 8 MB pass finishes in ~0.13 s, so in the matrix's `m1` (damage injected seconds after ready) the pass had already passed: only the first read finds it, exactly the ADR's "damaged after the pass passed it" limit. |
| 11 open-time refusals | all still **exit 1**, now `engine open refused: SSTABLE_CORRUPT\|SSTABLE_MISSING\|SSTABLE_MISMATCH: <the engine's text, path included>; refusing to open: ... The directory has not been modified.` and **11 of 11 `directory_unchanged=True`** (before: 11 of 11 False). A second start gives the same refusal (unit test). |
| b4, i | unchanged (open; `i` is adopted by the engine, `MANIFEST` is the extra changed file, as before) |
| healthy control S0 | opens in 0.512 s (before 0.512 s; 5-scenario range 0.502-0.534 before, 0.504-0.528 after, at 50 ms polling), `sstable_verification` `complete` in 15 ms, 8,060,036 bytes verified |

**Endpoint diff against the pre-change binary** (healthy fixture, `endpoint_diff.json`): `/readyz` gained `sstable_verification`; `/v1/status` gained `sstable_integrity`; `/healthz`, `/v1/admin/status`, `/v1/compaction/status`, `/v1/instance`, `/v1/metrics/system` gained and lost nothing; no existing field changed value other than volatile ones (clocks, uptime, memory) and `disk.volume_free_bytes` (environmental). `/readyz.ready` is `true` in every scenario, including `damaged`.

## 5. The F-07 interaction

Real binary, `f07_interaction_before` / `f07_interaction_after` (`raw\impl\after`): damage the bloom of SSTable 1 → start → repair the table → damage the last WAL record → start.

* **Before:** the SSTable-refused start consumed `WAL_CLEAN_STOP` (changed_files `["WAL_CLEAN_STOP"]`, file gone). After the repair and the WAL damage the next start was **unattested**: it opened, quarantined and truncated the tail (`WAL tail truncated ... last good sequence 203`), `COUNT(*)` 39,800 - one acknowledged record lost, with only a stderr remark.
* **After:** the SSTable-refused start changed nothing (`changed_files []`, `WAL_CLEAN_STOP` SHA-256 identical, `274d628e...`); after the same repair and WAL damage the next start was refused `WAL_TAIL_DAMAGED: the log ends at sequence 203, but the last clean shutdown recorded 204 ...`, directory unchanged. Unit test: `a_refused_sstable_start_leaves_wal_clean_stop_untouched_so_the_next_start_is_still_attested`.

**Precedence (an ADR/brief wording point).** `WAL_CORRUPT` keeps its precedence (its preflight runs first; unit test `wal_corrupt_keeps_its_precedence...`). `WAL_TAIL_DAMAGED` is decided inside `apply_tail_policy`, which is also the first mutation; the ADR places the SSTable preflight *before* it ("so every refusal is decided before the first mutation"). So when a damaged table and a damaged WAL tail coexist, the **SSTable refusal is reported first** and nothing is changed; after the table is repaired the WAL refusal follows (shown above). I followed the ADR, as the brief requires. Making `WAL_TAIL_DAMAGED` win would need `apply_tail_policy` split into decide / execute (`src/ops/wal_tail.rs`, not on the ADR's list).

## 6. Tests

* `src/ops/sstable_integrity_tests.rs` (18 run + 1 ignored evidence tool): healthy directory + guard; **every open-time scenario** (footer magic, footer CRC, index offset, bloom, index body, index structure, format-version bit flip, format-version 2, truncated, missing live file, junk foreign table) → refused with the right code, names the file, `The directory has not been modified.`, directory (including `WAL_CLEAN_STOP`) digest-identical, second start same refusal, **and the engine refuses the same directory**; the engine's text carried verbatim; valid-but-wrong file by size and by sequence range (and the engine accepts it); recorded size 0 is unknown; unrecorded valid table left to the engine, unrecorded damaged one refused, `.sst.tmp` ignored; unreadable Manifest; F-07 attestation interaction; `WAL_CORRUPT` precedence and SSTable-before-tail-policy; an open/close/open cycle with writes (4 rounds) never refuses; directories written through flush, **compaction** and **adoption** pass; clean pass completes; a data-block finding (counts, path, others still verified, no key in the stderr/security text); damage found while the pass is running is reported before the pass ends; the throttle (5,981,322 bytes at 12 MiB/s: expected 0.475 s, measured 0.476 s; unthrottled 0.030 s); cancel stops at the next block (not even the rest of the table); a table retired / damaged after the preflight; the budget parser. `sweep_existing_directories` is `#[ignore]` (the evidence tool of section 8).
* `api/tests/sstable_integrity_reporting.rs` (4): no pass → `disabled` with exactly the 3 old `/readyz` fields + 1; clean pass → `complete` on both surfaces; a damaged block → `damaged` on both surfaces with `ready:true`/`Healthy`, one stderr line, **exactly one** `sstable.damaged` security event through the real `SecurityLogLayer`, reads fail typed (`CORRUPTION`) for the damaged block and succeed for the rest; budget 0 starts nothing.
* `cli/src/startup_env.rs` (1 unit test): default 64, 0, 1024, leading zeros, 13 bad values each naming the variable.
* Real binary: `raw\impl\after\extra_summary.txt` - the setting unset/`0`/`""` start; `abc`, `1025`, `-1`, `" 5"` exit 1 naming the variable with the directory untouched.
* **Mutation check** (`mutation_check.txt`, `mutation_check2.txt`): 10 mutants of the new code - M1 preflight after the tail policy, M2 size not compared, M3 sequence range not compared, M4 size 0 compared, M5 unrecorded tables not opened, M6 no throttle, M7 cancel ignored, M8 damaged pass reported complete, M9 a finding does not set `damaged`, M10 a missing file not refused. First pass: M7 and M9 **survived**; the two tests were strengthened (the cancel test now bounds the bytes read after the cancel; the mid-run test now requires `damaged` before the pass ends); second pass: M1 (did not compile as a one-line edit in the first pass), M7, M9 killed. **10 of 10 killed.** Sources restored byte-identical.

## 7. Regression

| Step | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | PASS |
| `cargo test --lib` debug / release | PASS / PASS |
| `wal_tests` | 12/12 |
| `pathological_recovery_matrix` | 9/9 |
| `crash_consistency --features test-util` | 2/2 |
| `group_commit --features test-util crash_consistency` | 2/2 |
| F-07 integration (`-p rubixdb-cli --test f07_tail_damage_integration -- --test-threads=1`) | **13/13**, unchanged |
| F-08 compaction-leak suite (`compaction_failure_tests` x5 + `compaction_failure_reporting` x2) | pass in both workspace runs |
| `cargo test --workspace --no-fail-fast` debug | 1425 passed, 2 failed: `repo_hygiene::no_tracked_credentials_json`, `repo_hygiene::no_tracked_file_contains_a_64_hex_admin_key_literal` (known) |
| `cargo test --release --workspace --no-fail-fast` | 1425 passed, 4 failed: the two `repo_hygiene` tests, `group_commit::m1_3_thousand_writers_throughput::thousand_writers_throughput`, `api observability::a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind` (all four known; none new) |
| `git diff --stat -- src/wal/ src/manifest/ src/error.rs src/compaction/ src/sstable/ src/lsm/` | **empty** |

## 8. The migration check the ADR required

ADR "Migration": run the check over existing output directories before enabling the refusal. `sweep_existing_directories` ran the preflight, `check_physical` and an engine open (on a copy) over **485** data directories left on this machine by earlier runs (SQL/LSM test dirs, the physical-corruption campaign, the long-endurance and final-endurance data, e2e data). Result (`sweep_result.tsv`): 476 pass the preflight, have 0 `check` errors and open; 5 pass the preflight, have `check` errors (data-block / WAL damage the physical campaign injected on purpose) and open; **4 are refused by the preflight - all four are deliberately damaged physical-campaign case directories (missing table, damaged Manifest, size mismatch, unsupported version), and the engine refuses the same four; 0 directories were refused by the preflight that the engine opens and `check` reports clean, and 0 pass the preflight that the engine refuses.** This covers engine-written directories through flush, compaction and adoption; it does not include crash-cycle example outputs (those are deleted by their runs) and does not test power loss.

## 9. The background pass's default is NOT measured

`RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC` defaults to **64**, the ADR's proposal; the ADR states the effect on foreground latency "is not measured and is the first thing to measure", and it is **still not measured**: the available fixtures (8 MB) finish a pass in 0.13 s, and the volume has ~2.5 GB free (97.8 % used), too little for a meaningful multi-GiB run with a latency probe. What *was* measured: the throttle is accurate (expected 0.475 s vs 0.476 s for 5.98 MB at 12 MiB/s; the setting is honoured by the real binary: `slow_pass_midrun` at 1 MiB/s reached its finding after 5.1 MiB read in ~4.8 s) and the preflight did not change measured start time (0.50-0.53 s before and after at 50 ms polling). The default is therefore an **unvalidated default**; `0` disables the pass. "Low-priority thread" is realised as the byte-rate throttle only: no OS thread priority is set (that needs `unsafe` or a new dependency, both excluded).

## 10. Limits, deviations and things I did not do

* **One sentence on deviations from the brief / ADR:** the refusal wording differs from today's `engine open failed: ...` (the ADR's proposed `engine open refused: <CODE>: ...; refusing to open: ...` carrying the engine's text verbatim), the brief's "same message" cannot hold literally for the same reason, and an SSTable refusal is reported ahead of `WAL_TAIL_DAMAGED` (section 5); the policy is otherwise exactly the ADR's.
* A truncated table is now reported as `SSTABLE_MISMATCH` (the length no longer equals the Manifest's record - check 2 precedes the open, ADR order) instead of `footer: bad magic`.
* The pass reports the first failing read of each table; a footer `record_count` that disagrees with the readable records (`b4`) is **not** a finding (the ADR names only the first `Err`); `rubixdb check` still reports it. Left open.
* Footer / bloom / index damage and a deleted / replaced file under a running instance are found only at the next start (ADR limit). A table deleted while running is skipped by the pass as "retired".
* The pass verifies only the tables live at start; tables written afterwards are not scanned until the next start.
* `docs/PROJECT_STATE.md` was **not** edited (the ADR's scope does not cover it; F-08 remains listed there until a doc-only commit changes it, as for F-07). The compaction-retry leak this ADR also mentions was fixed by the previous mission and is not touched.
* Not tested: power loss; a real disk error; a database of more than a few MB under load; a cold-cache scan rate; the standalone `rubixdb-api` binary (it shares the guard; not run).
