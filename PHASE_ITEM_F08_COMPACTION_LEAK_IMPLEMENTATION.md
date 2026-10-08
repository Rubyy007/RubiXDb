# F-08 compaction retry leak — implementation of ADR-COMPACTION-LEAK-01 (P3)

**Date:** 2026-10-08. **Base tree:** `bb46d88`. **Status:** implemented and tested; committed locally, not pushed. Engine change authorised by the maintainer's approval of ADR-COMPACTION-LEAK-01 (P3). Nothing outside the ADR's authorised files was changed.

Raw evidence: `E:\rubixdb_f08_compaction\raw\impl\` (`before\`, `after\`, `case_A\result.json`, `unit_tests.txt`, `mutation_check.txt`); scripts: `E:\rubixdb_f08_compaction\scripts\impl\`; regression logs: `E:\rubixdb_f08_compaction\logs_regress_impl\`.

## 1. Decision implemented (ADR Decision line, verbatim)

> Make the SSTable writer remove its own temporary file on every failure path; record every compaction failure (kind, count, last error) in `CompactionMetrics` and show it, with a derived state, on the existing compaction surfaces and in the security log; and classify failures by structured error variant so that a worker that has failed `max_flush_retries` (3) consecutive times with a permanent-class error (`Corruption`, `Unsupported`, or a panic) stops attempting until the process restarts, leaving `StorageState` and ADR-WE-SP-001's model untouched.

## 2. Reproduction on the unmodified tree (before)

Same fixture and damage as the discovery (case A: three-table fixture, one bit flipped in data block 478/956 of SSTable 1). Binary built from the unmodified tree (`rubixdb_impl_before.exe`).

| t (s) | `.sst.tmp` files | tmp bytes | `sstables/` bytes | compaction state / failures reported |
|---|---|---|---|---|
| 0 | 1 | 1,991,320 | 18,112,427 | none (`cycles_completed: 0`, no failure field) |
| 30 | 6 | 11,947,920 | 28,069,027 | none |
| 60 | 12 | 23,895,840 | 40,016,947 | none |
| 120 | 24 | 47,791,680 | 63,912,787 | none |

* 23 inter-attempt gaps, 5.025–5.067 s, mean 5.052 s. Each tmp file is 1,991,320 bytes (the discovery: 5.048 s mean, 1,991,320 bytes, 1/6/12/24 files at 0/30/60/120 s). **No material divergence from the discovery.**
* A graceful stop left all 24 files (47,791,680 bytes). stderr: one `compaction: failed, will retry on the next trigger: corruption: block: checksum mismatch` per attempt. `/readyz` ready, `storage_state` Healthy.

## 3. After the change (same case, same fixture, `rubixdb_impl_after.exe`)

| t (s) | `.sst.tmp` files | `sstables/` bytes | state | failures_total | consecutive | blocked |
|---|---|---|---|---|---|---|
| 0 | 0 | 16,121,107 | failing | 1 | 1 | false |
| 30 | 0 | 16,121,107 | blocked | 3 | 3 | true |
| 60 | 0 | 16,121,107 | blocked | 3 | 3 | true |
| 120 | 0 | 16,121,107 | blocked | 3 | 3 | true |

* Failures at 0 s, 5.05 s, 10.12 s (engine timestamps `last_failure.at_unix_ms`); the third is the budget, so the worker is blocked at ~10.1 s and makes no further attempt. Zero tmp files at every 50 ms watcher poll except the transient file of an attempt in flight, which the writer removes (the watcher saw ids 5 and 6 only, 1,820,514 and 287,426 bytes, both gone). A graceful stop left 0 files.
* stderr: `compaction: failed (attempt 1 of 3), will retry: …`, `(attempt 2 of 3)`, then `compaction: blocked after 3 consecutive permanent failures (corruption: block: checksum mismatch); restore the damaged table and restart`.
* At t = 120 s all four surfaces agree (`/v1/compaction/status`, `/v1/compaction/metrics`, `/v1/admin/status.compaction`, `/v1/metrics/system.compaction`): `state: blocked`, `failures_total: 3`, `consecutive_failures: 3`, `blocked: true`, `last_failure {at_unix_ms, kind: corruption, message}`. `/readyz` 200 ready, `storage_state` Healthy (unchanged, decision D5).
* I/O amplification: before, one failing attempt every 5 s (≈ 0.4 MB/s in case A, up to the size of the database in case C); after, three attempts in total, then none until restart.

## 4. What changed, by file

| File | Change | Mechanism |
|---|---|---|
| `src/sstable/writer.rs` | `TmpFileGuard` | Created right after the tmp file is created, declared before the file handle (so the handle closes first on Windows); `Drop` removes the path unless disarmed; disarmed immediately after the successful `fs::rename`. Covers every `?` return and a panic unwinding through the writer. Removal is best-effort; the sweep at open stays as the backstop for a kill. |
| `src/lsm/mod.rs` | classification, state machine, worker gate, metrics | `CompactionFailureKind { Corruption, Unsupported, Panic, Io, Other }` classified by `EngineError` variant, never by text (Corruption/Unsupported/Panic are permanent-candidate; Io incl. ENOSPC and Other are transient). `CompactionHealth { Healthy, Failing, Blocked }` in an `AtomicU8` (unknown value reads as Blocked). `record_compaction_failure` applies the budget (`max_flush_retries`, 3 consecutive permanent failures → Blocked; a transient failure or a success resets the permanent streak). A success never lifts Blocked. The worker skips attempts while Blocked (still drains messages and honours shutdown). `CompactionState { idle, running, failing, blocked }` derived by `LsmEngine::compaction_state()`. New `CompactionMetrics` fields `failures_total`, `consecutive_failures`, `blocked`, `last_failure`. |
| `src/lsm/tests.rs` | tests | See section 5. |
| `api/src/routes/compaction.rs`, `admin.rs`, `metrics_system.rs` | additive JSON fields | `state`, `failures_total`, `consecutive_failures`, `blocked`, `last_failure {at_unix_ms, kind, message}` on `/v1/compaction/status`, `/v1/compaction/metrics`, `/v1/admin/status.compaction`, `/v1/metrics/system.compaction`. No existing field changed. |
| `api/src/observability/sampler.rs` | security-log events | `compaction.failing` and `compaction.blocked`, emitted by the sampler tick on state change only (never per retry). The object field carries only `kind`, `consecutive`, `failures_total`. |
| `api/tests/compaction_failure_reporting.rs` | new | See section 5. |

Not touched: `src/wal/`, `src/manifest/`, `src/error.rs`, `src/compaction/` (`git diff --stat` empty), `StorageState`, `/readyz`, ADR-WE-SP-001, `Cargo.toml`, `Cargo.lock`, `docs/PROJECT_STATE.md`.

Only change relative to the ADR text: none in policy. (Implementation detail: the worker receives the budget as a `failure_budget` parameter taken from `LsmConfig::max_flush_retries` at the spawn site.)

## 5. Tests

Engine (`src/lsm/tests.rs`, `compaction_tests::compaction_failure_tests`):

* `documented_prefix_bug_failed_compaction_leaves_tmp_files_and_reports_nothing` — `#[ignore]`: the documentation counterpart; passes only on a tree without the fix (verified before the fix).
* `a_failed_compaction_attempt_leaves_no_partial_output_behind` — acceptance; **fails before the fix** (tmp files accumulate), passes after.
* `a_permanently_unreadable_input_is_reported_and_the_worker_stops_after_its_budget`.
* `a_restart_is_the_defined_exit_from_blocked`.
* `a_transient_failure_is_counted_never_blocks_and_a_success_ends_the_streak`.
* `the_classification_and_the_state_machine_follow_the_adr`.

API (`api/tests/compaction_failure_reporting.rs`): `a_healthy_engine_reports_idle_and_zero_failures_on_every_compaction_surface`; `a_blocked_compaction_is_visible_on_every_surface_and_changes_nothing_else` (the four surfaces, `/readyz` and `storage_state` unchanged, no tmp files, exactly one `compaction.failing` and one `compaction.blocked` security event, through the real `SecurityLogLayer`).

Mutation check (`raw\impl\after\mutation_check.txt`): 5 of 5 mutants killed — M1 guard never removes (killed by the acceptance and budget tests), M2 worker ignores Blocked, M3 Io classified permanent, M4 a success lifts Blocked, M5 budget ignored. Sources restored byte-identical.

## 6. Full regression

| Step | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | PASS |
| `cargo test --lib` (debug) | PASS |
| `cargo test --release --lib` | PASS |
| `wal_tests` | PASS 12/12 |
| `pathological_recovery_matrix` | PASS 9/9 |
| `crash_consistency --features test-util` | PASS 2/2 |
| `group_commit --features test-util crash_consistency` | PASS 2/2 |
| F-07 integration (`f07_tail_damage_integration`, `--test-threads=1`) | PASS 13/13 |
| `cargo test --workspace --no-fail-fast` (debug) | 1402 passed, 2 failed — both known: `repo_hygiene::no_tracked_credentials_json`, `repo_hygiene::no_tracked_file_contains_a_64_hex_admin_key_literal` |
| `cargo test --release --workspace --no-fail-fast` | 1402 passed, 4 failed — all known: the two `repo_hygiene` tests, `group_commit::m1_3_thousand_writers_throughput::thousand_writers_throughput`, `observability::a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind` (release-only) |

Every failure is one of the four known pre-existing failures; no new failure. The new tests pass in both profiles.

## 7. Answers to the brief's questions

* **Does the fix also cover the flush path?** The writer fix does: the flush path calls the same `write_from_sorted_records`, so a failed flush attempt now removes its tmp file as well. That is by code reading and by the shared guard; **no flush-failure (e.g. ENOSPC) test was added or run** — not tested. The failure classification, metrics, state and block apply to compaction only.
* **Does the error line now name the table?** No. The stderr line and `last_failure.message` carry the engine's message (`corruption: block: checksum mismatch`), which still names no table. Naming the damaged table is ADR-SST-01's subject (F-08 itself), not this item.

## 8. Limits and not tested

* Disk-full (ENOSPC) during compaction: classified transient by variant, covered by the classification unit test with a synthetic `Io` error; not exercised against a real full disk. Power loss: not tested.
* `LsmEngine::compact_once` called by hand (outside the worker) is not recorded by the worker's failure arm; it benefits from the writer cleanup only.
* Blocked is in-memory: it is left only by a process restart, as the ADR defines. After a restart with the damaged table still present the worker fails three times again and blocks again (~10 s).
* The `rubixdb status` CLI text line was not extended (the JSON endpoints are).
* A table count that grows while blocked (read amplification, retained tombstones) is the ADR's accepted consequence of P3.
* Interaction with F-07/F-08 attestation, ADR-SST-01 and everything else listed out of scope were not touched.
