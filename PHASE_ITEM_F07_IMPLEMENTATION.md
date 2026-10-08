# PHASE ITEM F-07 — IMPLEMENTATION: ADR-WAL-01 (P4), a damaged WAL tail must not open silently

**Date:** 2026-10-08. **Mission:** F-07 only, implementation of the maintainer-approved ADR-WAL-01 (P4) exactly as written in `PHASE_ITEM_F07_ADR.md` (now ACCEPTED). Discovery and the evidence for the problem: `PHASE_ITEM_F07_DISCOVERY.md`. **Not** a power-loss result, **not** a statement about whole-product readiness (not declared).

## 1. Objective, as the mission states it

A cleanly stopped database must never silently lose an acknowledged WAL tail. An unattested crash tail may still be truncated according to the existing WAL engine contract, but the removed evidence is quarantined and reported. Existing corruption refusals stay refusals. The override is an explicit operator choice and never bypasses true `WAL_CORRUPT`. A refused startup does not modify the data directory. The WAL specification and the engine's recovery classification are preserved.

## 2. Identity

| Item | Value |
|---|---|
| Base | branch `master`, tree clean, HEAD `a3a7f9661008…` (`F-07: WAL tail damage -- discovery and ADR-WAL-01 (PROPOSED); no code changed`) |
| `docs/PROJECT_STATE.md`, `missions/ACTIVE.md` | both exist (ACTIVE.md: "No active mission"; the mission text of this session is the authority) |
| Binary under test | release build of the working tree, `E:\rubixdb_f07\rubixdb_f07_new.exe`, SHA-256 `b6b2599b6307b7175d44cafa7c5012dd143e5ba2c355c8fb8ac4a780addbf29b`, reports `git_revision a3a7f9661008-dirty` (the uncommitted F-07 change on top of the base) |
| Baseline binary | `E:\rubixdb_f07\rubixdb_f07_base.exe` (caf12be, built before the change), SHA-256 `8299dffa2c5892b6c0ac5743771b4ade92335f16fb4b4ced00776b9ef7f98d74` |
| Host | Windows 10 Home, local disk, loopback; one instance at a time on its API port |

## 3. What changed

| File | Change |
|---|---|
| `src/ops/wal_tail.rs` (new) | The whole feature: override parser, `WAL_CLEAN_STOP` encode / strict parse / read / write, the sequence rule, the quarantine (write, validate, reuse, list), `apply_tail_policy`, the report types. |
| `src/ops/format.rs` | `startup_guard` is unchanged in behaviour (its WAL preflight moved verbatim into a private helper, messages byte-identical); new `startup_guard_with_tail_policy` and `StartupGuard`. |
| `src/ops/check.rs` | Read-only: `WAL_CLEAN_STOP` and `wal-quarantine` are known entries; new informational finding `WAL_TAIL_QUARANTINED`. |
| `src/ops/mod.rs` | `pub mod wal_tail`, the test module, two error codes (`WAL_TAIL_DAMAGED`, `WAL_TAIL_QUARANTINE_FAILED`). |
| `cli/src/host.rs` | Uses the new guard and reports its result; after the engine has fully stopped, writes the attestation (a failure goes to stderr and never fails the shutdown). `EmbeddedServer`'s small identity moved into a boxed struct so the type does not grow (a clippy size lint on `main.rs`'s enum would otherwise fire). |
| `cli/src/startup_env.rs` | `RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL` parsed strictly with the `RUBIXDB_LOCAL_*` conventions and validated in `load()` before anything is created. |
| `api/src/main.rs` | The standalone binary uses the new guard (quarantine, reporting, honours an existing attestation); it writes no attestation. |
| `src/ops/wal_tail_tests.rs`, `cli/tests/f07_tail_damage_integration.rs` (new) | 23 library tests plus an `#[ignore]` timing harness; 13 real-binary tests (12 scenario tests and a SHA-256 self-test). |

## 4. What did not change

* **Protected paths:** `git diff --stat -- src/wal/ src/manifest/ src/sstable/ src/compaction/ src/error.rs` is **empty** (section 6). No comment, import, accessor, helper or test inside them.
* The WAL format, the manifest format, the engine's recovery classification (`recovery.rs`) and its truncation (`mod.rs`) are untouched; the engine still truncates exactly as before.
* `src/ops/open.rs` is **unchanged**; `startup_guard` behaves as before for its remaining callers; no existing test assertion was changed; `WAL_CORRUPT` messages and behaviour are byte-identical; `WAL_TORN_TAIL` keeps its severity and exit-code effect; `Cargo.toml` / `Cargo.lock` unchanged; no new dependency, no `unsafe`.

## 5. Guard sharing with `src/ops/open.rs`: **not safe, not shared**

`open_engine_for_ops` (called by `rubixdb check`'s logical pass, by `restore`, and by `ops_cmd`) calls `startup_guard`. The tail policy writes files (quarantine) and deletes one (the attestation) and adds a refusal. Sharing it would (a) make `rubixdb check` — documented "INSPECTION (read only)" — write a quarantine and consume the attestation, and (b) add a new refusal to `check` and `restore`. Both change existing behaviour, so the startup-only path is separate: `startup_guard_with_tail_policy` is called only by `rubixdb gui` / the embedded host (`cli/src/host.rs`) and the standalone `rubixdb-api` (`api/src/main.rs`). **`src/ops/open.rs` was not changed; `rubixdb check` behaviour did not change** (apart from the two read-only additions the mission asks for: the known entries and the informational listing). Test: `the_old_guard_is_unchanged_for_its_other_callers`.

Consequence, stated plainly: because `check`'s logical pass still opens the engine through the old guard, `rubixdb check --data-dir` on a damaged stopped directory still truncates the tail without quarantining it (the separate finding already recorded in `OPEN_ITEMS.md`, 2026-10-08). The attestation protection survives that: `check` neither deletes the attestation nor adds records, so the next real start still sees "recorded N, log ends at M<N" and refuses.

## 6. Protected-path verification

`git diff --stat -- src/wal/ src/manifest/ src/sstable/ src/compaction/ src/error.rs` — empty (the output, empty, is also in the report returned with this document). `git diff --name-only` lists only: `PHASE_ITEM_F07_ADR.md`, `api/src/main.rs`, `cli/src/host.rs`, `cli/src/startup_env.rs`, `src/ops/check.rs`, `src/ops/format.rs`, `src/ops/mod.rs` plus the new files and the documentation entries.

## 7. Semantics as implemented

**Attestation `<data_dir>/WAL_CLEAN_STOP`** — canonical text, five `key=value` lines, each ended by `\n`: `rubixdb-wal-clean-stop=1`, `segment=<id>`, `length=<bytes>`, `last_seq=<seq>`, `crc32c=<8 lowercase hex>` (CRC32C of the four preceding lines, newlines included). Written by `EmbeddedServer::shutdown()` only after the server thread is joined and the runtime dropped (the engine, hence its WAL lock, is gone): a read-only `wal::replay_streaming` of the stopped directory must report no corrupted segment, no torn tail, an existing newest segment whose length equals `last_valid_position.offset`, and an internally consistent summary — a still-held WAL lock makes the replay fail, so nothing is written. Temp file, write, fsync, atomic rename; any failure removes the temp file, prints to stderr and leaves shutdown successful. Real stops (`rubixdb instance stop`) wait for the instance lock, which `shutdown()` releases last, so the file exists when `instance stop` returns.

**Parser** — rejects: wrong/unknown version, missing / duplicate / unexpected / reordered keys, lines without `=`, non-canonical integers (sign, padding, leading zeros, overflow), segment 0, length below a 24-byte header, wrong CRC or non-lowercase / wrong-width hex, missing final newline, trailing text or blank lines, CRLF, non-ASCII, oversize. A missing, unreadable or invalid file is **UNATTESTED**: never a refusal by itself; a stderr note is printed when the file is present but unusable, or absent while the WAL holds records.

**Evaluation (before the engine opens)** — after the unchanged `WAL_CORRUPT` preflight (those refusals take precedence), `current_reached_seq = max(last valid WAL seq, manifest checkpoint seq)` (the manifest is read only when the log is behind the attestation); **refuse iff `current_reached_seq < attested last_seq`**: `WAL_TAIL_DAMAGED`, exit 1, with the ADR's exact wording (segment, both sequence numbers, the exact missing count, "The directory has not been modified."). A refused start writes nothing.

**Attestation removal** — after the guard has passed and before the engine opens, best effort; a stale file cannot refuse a later start because the rule compares sequences (a log that has grown or been purged below the checkpoint always reaches it).

**Quarantine** — when the replay reports `truncated`, the bytes `[last_valid_position.offset, file length)` of the newest segment are copied unchanged to `<data_dir>/wal-quarantine/wal-<segment>.<offset>.<unix_ms>.tail` (44-byte header: magic `RBXWTAIL`, version, segment, offset, length, CRC32C of the bytes, CRC32C of the header; then the exact bytes) via `*.tmp`, fsync, atomic rename, before the engine truncates. A partial file never validates (size, header CRC and payload CRC are all checked) and `.tmp` files do not match `*.tail`. Repeated start: an existing complete entry with the same segment, offset, length and CRC is reused (no duplicate); a complete entry is never overwritten (a name collision picks the next free millisecond). If the copy fails for any reason (I/O error, permission, directory, disk full, fsync, rename) the start is refused (`WAL_TAIL_QUARANTINE_FAILED`, the WAL untouched) unless the override is set.

**Reporting** — stderr (`rubixdb: WAL tail truncated at open: segment S, offset O, N byte(s) removed, last good sequence Q; the removed bytes were preserved in <path>`); security log events `wal.tail_quarantined` (`object` = `segment=S offset=O bytes=N last_seq=Q`) and `wal.tail_override`; `rubixdb check` (stopped instance) lists each preserved tail. No event or line carries preserved bytes, credentials or row data.

**Override `RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL`** — unset / empty = off, exactly `1` = on, anything else stops startup naming the variable before anything is created (`true`, `TRUE`, `yes`, `on`, `01`, `1.0`, padded forms and the rest are refused). It bypasses only the `WAL_TAIL_DAMAGED` refusal (and an unwritable quarantine, see section 9); it still quarantines, reports and truncates; it never bypasses `WAL_CORRUPT`, header, middle-frame, format or identity refusals, nor the 64-byte zero tail. `wal.tail_override` is logged only when it changed an outcome.

**Unaffected cases** — kill, power loss, first start, legacy directory, standalone `rubixdb-api`, missing / unreadable / unknown-version attestation: no attestation refusal; a torn tail is quarantined, reported and truncated by the engine as before; corruption keeps its `WAL_CORRUPT` refusal; a clean WAL starts normally.


## 9. Deviations from the ADR and from the mission text, and clarifications

None changes a decision; each is stated so nothing is silent.

1. **Override versus an unwritable quarantine (mission section 16 versus section 20).** Section 16 says an unwritable quarantine refuses the start "unless the override is active"; section 20 says the override "still requires quarantine". The ADR's Design says the same as section 16. Implemented as section 16 / the ADR: with the override on, a quarantine failure does not block the start; it proceeds loudly (stderr "could NOT be preserved", `wal.tail_override` logged because the override changed the outcome, nothing reported as quarantined). Without the override the start is refused (`WAL_TAIL_QUARANTINE_FAILED`, WAL untouched). If the maintainer prefers section 20's reading, the change is one condition in `apply_tail_policy`.
2. **Severity of the `check` finding.** The mission asks for a listing finding without a severity. It is **Info**, so no exit code of `rubixdb check` changes and `WAL_TORN_TAIL` stays a warning. It is produced by the physical pass, so it appears in the offline / stopped `rubixdb check` (the real-binary test asserts it), not in the online check of a running instance (which runs only the logical pass, `api/src/routes/admin.rs`, outside the permitted files).
3. **Security-log fields.** The record has a fixed set of fields and `api/src/security_log.rs` is outside the permitted files, so events use the existing fields: `code` = `wal.tail_quarantined` / `wal.tail_override`, `object_kind` = `wal`, `object` = `segment=S offset=O bytes=N last_seq=Q` (override: `segment=S attested_seq=A reached_seq=R`, or `quarantine_failed`). Never bytes, credentials or row data.
4. **Attestation eligibility is the literal rule.** "Last segment length == `last_valid_position.offset`": a newest segment that holds only its header after earlier segments with records is not attested (the summary's position names the earlier segment). That is the conservative direction (unattested = today's behaviour plus reporting). Test: `the_attestation_is_not_written_for_a_corrupt_torn_or_inconsistent_directory`.
5. **Manifest read.** The checkpoint is read only when an attestation exists and the log is behind it; an unreadable manifest then refuses the start with its own message (the engine would fail on it anyway).
6. **Unattested note.** Printed when a file is present but unusable, or absent while the WAL holds records; not for a brand-new empty directory.
7. **`EmbeddedServer` layout.** Adding the data directory made `main.rs`'s `ConnectionSource` enum exceed clippy's `large_enum_variant` threshold (a new `-D warnings` failure caused by this change), and `main.rs` is not a permitted file; the identity fields were moved into a boxed `ServerMeta` in `host.rs`, which shrinks the type below the old size.
8. **A test was wrong, not the code.** The first draft of `sha256_matches_the_published_vectors` (in the new integration test file, a helper for the directory-digest proof) carried a mistyped expected value for the 56-byte NIST vector. The implementation's output was cross-checked against Python `hashlib` (identical) and the expected value corrected; a second two-block vector was added. No existing test was touched.
9. **Mission section 24 lists the scenario fields** "binary revision, binary hash, exit code, stderr, rows, WAL bytes, attestation, quarantine, security log, check result, directory digest". The evidence rows (`E:\rubixdb_f07\raw\f07_integration_run.txt`, 23 `F07_EVIDENCE` lines) carry them per scenario; the revision of the integration-test binary is `a3a7f9661008-dirty` (debug build, SHA-256 prefix `af2e153947f5`); the performance binary is the release build `b6b2599b…`.

## 10. Test evidence

### 10.1 Library tests (`src/ops/wal_tail_tests.rs`, 23 tests, all pass in debug and release)

Attestation canonical text and round trip; strict parser rejects every listed malformation (missing lines, malformed integers, invalid and mis-cased CRC, duplicate / extra / reordered keys, unknown version, malformed key=value, trailing garbage, CRLF, non-ASCII, oversize); a missing / unreadable / malformed / unknown-version / bad-CRC file is unattested and does not refuse; written from the stopped state with a valid CRC; not written for torn, corrupted, inconsistent or lock-held directories, and a failed write leaves nothing complete-looking; the sequence rule (behind / reaches / exceeds / checkpoint reaches / `max(wal, checkpoint)`), including the missing count; refusal with the exact ADR text, the exact gap, the directory byte-identical (xxh64 of every file) and a second identical refusal; the count is in sequences not rows; a stale attestation never refuses; the manifest checkpoint counts as reached (a real engine directory with flushed SSTables: attested = checkpoint passes, checkpoint + 3 refuses with "3 acknowledged record(s) are missing"); existing `WAL_CORRUPT` refusals take precedence with the old guard's exact message, with or without the override; the old guard is unchanged for its other callers; unattested torn tail is quarantined and reported; the override bypasses only the attestation refusal; security notes carry no bytes; quarantine payload byte-for-byte, header and CRC fields; partial / damaged files never validate; every injected failure stage (create dir, create temp, write, sync, rename; generic and disk-full) is an error and leaves no `.tail` or temp file; an unwritable quarantine refuses unless the override is set; repeated start reuses the entry; a complete entry is never overwritten; `check` recognises the new entries, lists the quarantine, and its error / warning counters are unchanged.

**The tests can fail.** Four deliberate mutants of `wal_tail.rs` were each killed (source restored byte-identical afterwards): sequence rule `<` -> `<=` (4 tests failed), refusal disabled (3), quarantine copying from `offset + 1` (9), manifest checkpoint ignored (2).

### 10.2 Real-binary matrix (`cli/tests/f07_tail_damage_integration.rs`, 13 tests, all pass)

Real `rubixdb gui` process; real graceful stop (`rubixdb instance stop`) or process kill; fresh data per scenario; each scenario inserts three single rows and one **four-row statement that is a single WAL sequence number**, every insert acknowledged with HTTP 200. Matrix as run (full rows incl. stderr text, digests and check results are in the evidence file):

| scenario | mutation | stop | opened / exit | acked rows missing | WAL bytes before -> end | attestation after stop | quarantine files at end | wal.* security events | exe SHA-256 (first 12) |
|---|---|---|---|---|---|---|---|---|---|
| clean_stop_tail_damage | A1LastBody | Clean | NO, exit 1 | not reachable: start refused | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| clean_stop_tail_damage | A2LastCrc | Clean | NO, exit 1 | not reachable: start refused | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| clean_stop_tail_damage | A4LastLenUp | Clean | NO, exit 1 | not reachable: start refused | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| clean_stop_tail_damage | A5Torn10 | Clean | NO, exit 1 | not reachable: start refused | 744 -> 734 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| kill_unattested_tail_damage | A1LastBody | Kill | yes | 4,5,6,7 | 744 -> 591 | - | 1 | 1 | af2e153947f5 |
| kill_unattested_tail_damage | A2LastCrc | Kill | yes | 4,5,6,7 | 744 -> 591 | - | 1 | 1 | af2e153947f5 |
| kill_unattested_tail_damage | A4LastLenUp | Kill | yes | 4,5,6,7 | 744 -> 591 | - | 1 | 1 | af2e153947f5 |
| kill_unattested_tail_damage | A5Torn10 | Kill | yes | 4,5,6,7 | 744 -> 591 | - | 1 | 1 | af2e153947f5 |
| invalid_override | A1LastBody | Clean | NO (startup refused) | - | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| wal_corrupt_regression | A3LastLenDown | Clean | NO, exit 1 | - | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| wal_corrupt_regression | B1MidBody | Clean | NO, exit 1 | - | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| wal_corrupt_regression | B2MidCrc | Clean | NO, exit 1 | - | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| wal_corrupt_regression | B3Catalog | Clean | NO, exit 1 | - | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| wal_corrupt_regression | C1HdrMagic | Clean | NO, exit 1 | - | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| wal_corrupt_regression | C2HdrVersion | Clean | NO, exit 1 | - | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| wal_corrupt_regression | C3HdrSegId | Clean | NO, exit 1 | - | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| wal_corrupt_regression | Z3Zero64 | Clean | NO, exit 1 | - | 744 -> 808 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| zero_tail_after_clean_stop | Z1Zero5 | Clean | yes | none | 744 -> 744 | seg 1 len 744 seq 6 | 1 | 1 | af2e153947f5 |
| zero_tail_after_clean_stop | Z2Zero8 | Clean | yes | none | 744 -> 744 | seg 1 len 744 seq 6 | 1 | 1 | af2e153947f5 |
| control | none | Clean | yes | none | 744 -> 744 | seg 1 len 744 seq 6 | 0 | 0 | af2e153947f5 |
| control | none | Kill | yes | none | 744 -> 744 | - | 0 | 0 | af2e153947f5 |
| override_clean_stop_tail_damage | A1LastBody | Clean | yes | 4,5,6,7 | 744 -> 591 | seg 1 len 744 seq 6 | 1 | 2 | af2e153947f5 |
| acknowledged_sequence_proof | A1LastBody | Clean | refused, then yes with override | 4,5,6,7 | 744 -> 591 | seg 1 len 744 seq 6 | 1 | 2 | af2e153947f5 |


Coverage against the mission's list: A1, A2, A4, A5 after a clean stop -> `WAL_TAIL_DAMAGED`, exit 1, directory unchanged twice; the same four after a kill -> quarantined, reported on all three channels, record still lost; A1 with the override; A1 with `bogus` and eleven other invalid values; B1, B2, B3, C1, C2, C3, Z3 (and A3, length lowered by one) with and without the override -> identical `WAL_CORRUPT`, no `wal.tail_override` event, directory unchanged; S0 clean and kill controls; Z1 / Z2 (5 / 8 zero bytes after an intact attested end) -> opens, nothing lost, junk quarantined; attestation lifecycle (written after a real stop, consumed at start, absent after a kill, failure to write reported while the shutdown still exits 0); an unwritable quarantine (a regular file where the directory must be) -> refused, WAL untouched, and with the override opens with "could NOT be preserved".

### 10.3 Acknowledged-sequence proof (A1, clean stop)

The last statement acknowledged with HTTP 200 inserted rows 4-7 in one statement; its WAL frame carries **sequence 6**; the attestation written by the real graceful stop records `last_seq=6`, `length=744`, segment 1. After one bit flip in that frame, the start is refused with `the log ends at sequence 5, but the last clean shutdown recorded 6 (segment 1, 744 bytes); 1 acknowledged record(s) are missing from the end of the WAL ...` — **one sequence number, four rows**, so the count comes from sequences, not rows. With the override the database opens, rows 1-3 are present, rows 4-7 are gone, and the quarantine entry (`wal-1.591.<ms>.tail`) holds exactly the 153 removed bytes (asserted byte-for-byte against the frame read before the damage). Catalog records are ordinary sequences (seq 1-2 here) and do not disturb the calculation (the same run).

### 10.4 Directory immutability

For every refused startup the SHA-256 of **every file** under `<instance>/data` (WAL, MANIFEST, SSTable directory, `DATA_FORMAT`, the attestation, and anything else present) is computed before and after, plus `credentials.json` and `instance.json` beside it; sets must be identical. Done for both attempts of each A1 / A2 / A4 / A5 clean-stop refusal (and the second restart repeats the same refusal), both runs of each corruption scenario (without and with the override), all twelve invalid-override values, and the unwritable-quarantine refusal. All identical; no quarantine file exists after any refused start.

### 10.5 Regression

| step | command | passed | failed | ignored | exit |
|---|---|---|---|---|---|
| 01_fmt | `cargo fmt --all -- --check` | - | - | - | 0 |
| 02_clippy | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | - | - | - | 0 |
| 03_lib_debug | `cargo test --lib` | 632 | 0 | 2 | 0 |
| 04_lib_release | `cargo test --release --lib` | 632 | 0 | 2 | 0 |
| 05_wal_tests | `cargo test --test wal_tests` | 12 | 0 | 0 | 0 |
| 06_pathological | `cargo test --test pathological_recovery_matrix` | 9 | 0 | 0 | 0 |
| 07_crash_cons | `cargo test --test crash_consistency --features test-util` | 2 | 0 | 0 | 0 |
| 08_gc_crash | `cargo test --test group_commit --features test-util crash_consistency` | 2 | 0 | 0 | 0 |
| 09_f07_cli | `cargo test -p rubixdb-cli --test f07_tail_damage_integration -- --test-threads=1` | 13 | 0 | 0 | 0 |
| 10_ws_debug | `cargo test --workspace --no-fail-fast` | 1395 | 2 | 29 | 101 |
| 11_ws_release | `cargo test --release --workspace --no-fail-fast` | 1395 | 4 | 27 | 101 |


Focused suites: `wal_tests` 12/12, `pathological_recovery_matrix` 9/9, `crash_consistency --features test-util` 2/2, `group_commit` `crash_consistency` 2/2, `f07_tail_damage_integration` 13/13. Library tests: 632 passed in debug and in release (609 before this change + 23 new; 2 ignored: the pre-existing one and the timing harness).

**Failures in the workspace runs, by exact name**

| Run | Test | Class |
|---|---|---|
| debug and release | `repo_hygiene::no_tracked_credentials_json` | **known, pre-existing** (tracked throwaway credential file, `OPEN_ITEMS.md`, 2026-10-04, SG-1) |
| debug and release | `repo_hygiene::no_tracked_file_contains_a_64_hex_admin_key_literal` | **known, pre-existing** |
| release | `group_commit::m1_3_thousand_writers_throughput::thousand_writers_throughput` | **known, pre-existing, intermittent** (WAL throughput gate M1.3) |
| release only | `rubixdb-api` `observability::a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind` | **NEW to this report; classified intermittent / environmental, pre-existing, not introduced** (below) |

**Classification of the one failure outside the known set.** The test panics with "the statement never showed as executing (it finished before the disconnect): use a heavier statement" — a race between a fast statement and the observer, which only the release build's speed makes likely; it passed in the debug workspace run. It lives in `api/tests/observability.rs`; the `rubixdb-api` library and every file on the path it exercises (SQL sessions, statement execution) have **no diff** in this mission (the api diff is the binary's `main.rs`). Repeated 30 times per case in release: **unmodified base tree** (built from `git archive`, no F-07 change): 3 of 30 fail with the per-package build and 3 of 30 with the workspace-unified build; **F-07 tree**: 5 of 30 and 7 of 30 respectively. One earlier batch of 30 runs of the F-07 tree's workspace-built binary failed 30 of 30 while the machine was still busy after the regression, and the identical file failed 7 of 30 minutes later on an idle machine — so its failure rate depends on host state, not on the code. Conclusion: pre-existing intermittent timing sensitivity of the test in release builds, not an F-07 regression; recorded in `OPEN_ITEMS.md` with the reproduction. (The previous mission's intermittent failures in the same file were different tests; same file, same kind of cause.) No test was modified.

## 11. Performance (F-07 paths only; no hot-path claim)

Hot append and read paths: unchanged — no code of this change runs there (it runs at start and at stop). Measured: `E:\rubixdb_f07\raw\perf\` (`unit_timings_4kib.txt`, `unit_timings_tiny.txt`, `perf_blackbox.json`, `stop_attribution.json`); library harness `f07_path_timings` (release, 15 runs per row, medians), temp directory on the same drive as the product tests; black-box on a fresh copy of the 20,000-row template (790 KB WAL), release binaries, interleaved.

| Path | WAL | Old `startup_guard` | New guard, unattested | New guard, attested | `write_attestation` (stop) | Quarantine write | Guard incl. torn-tail quarantine |
|---|---|---|---|---|---|---|---|
| 1 MiB, 249 records | 1.0 MB | 3.07 ms (min 2.13) | 2.28 ms | 3.08 ms | 10.6 ms | 1 KiB: 5.4 ms | 7.8 ms |
| 16 MiB, 3,994 records | 16.5 MB | 35.1 ms | 34.8 ms | 35.0 ms | 42.5 ms | 1 KiB: 5.3 ms; 1 MiB: 12.0 ms | 40.8 ms |
| 64 MiB, 15,978 records | 66.0 MB | 139.2 ms | 138.8 ms | 139.3 ms | 143.7 ms (max 268) | 1 KiB: 6.0 ms; 1 MiB: 14.2 ms | 127.2 ms |
| worst case for replay: 8-byte values, 149,796 records | 6.3 MB | 702.8 ms | 691.0 ms | 693.5 ms | 698.7 ms | 1 KiB: 5.8 ms; 1 MiB: 14.7 ms | 701.5 ms |
| worst case: 599,186 records | 25.2 MB | 2,766 ms | 2,757 ms | 2,752 ms | 2,760 ms | 1 KiB: 6.4 ms; 1 MiB: 14.3 ms | 2,759 ms |

* **Startup guard:** the new guard costs the same as the old one (the single WAL replay is shared); the attested variant adds one small read and one file delete (about 0.3-0.8 ms at 1 MiB, within noise at larger sizes). **Black-box start-to-ready: 520.0 ms (base) vs 520.6 ms (new)**, median of 20 interleaved runs each, min-max 504-534 vs 506-862 ms.
* **Graceful shutdown:** gains **one extra read-only replay of the stopped WAL plus a small fsynced file write.** Black-box median stop time on the template: 216 ms (base) vs 304 ms (new), 20 interleaved runs each; a second 3-way run (base / new / new with the attestation write forced to fail after the replay) gave 193 / 264 / 258 ms, so about **+65 ms is the replay and about +6-10 ms is the write** for this 20,000-row WAL. Replay cost is per record, not per byte (about 4.6 microseconds per record here): the worst synthetic case, a 25 MB segment of 600,000 tiny records, costs about **2.8 s** extra at every stop (and the same already costs 2.8 s at every start, from the existing guard). `rubixdb instance stop` waits up to 120 s for the instance lock, which the host releases only after the attestation is written; a WAL with tens of such segments would approach that bound. All 40 stops in the black-box runs were graceful, exit 0, and all 20 new-binary runs wrote the attestation.
* **Quarantine write:** about 5-6 ms for a 1 KiB tail and 12-15 ms for 1 MiB (fsync-dominated); only on a start that finds a torn tail.

## 12. Known limitations (stated, not hidden)

* **Power loss is NOT TESTED**, here or anywhere in this project. Every "kill" result is a process kill. Nothing in this work is a power-loss certification, and which tail shapes a real power cut leaves on this filesystem is unverified.
* **A kill still loses the damaged record.** With no graceful-stop attestation, an acknowledged record that was damaged on disk and then followed by a kill (or power loss) before the next graceful stop is truncated at the next start exactly as before — quarantined and reported, no longer silent or destructive, but not prevented. The real-binary test shows rows 4-7 missing after a kill for A1, A2, A4 and A5.
* **A directory from before this change, the standalone `rubixdb-api` binary, and any stop that could not attest** run unattested (reporting and quarantine only).
* **`rubixdb check` (stopped instance) still truncates a damaged tail without quarantining it** — its logical pass opens the engine through the unchanged shared guard (section 5; already recorded in `OPEN_ITEMS.md`). Running it does not defeat the attestation refusal.
* **The online `rubixdb check` of a running instance does not list quarantined tails.**
* **Shutdown cost scales with the number of records in the retained WAL** (section 11).
* **Zero-filled tails of 9+ bytes** remain refused as `WAL_CORRUPT` (the separate finding already recorded); the override does not and must not change that.
* **A release-build intermittent test** (section 10.5) is unrelated but now recorded.
* The attestation is an integrity aid against accidental damage, not a security control.

## 13. Git

Final protected-path check and the commit hash are in the report returned with this document and in `PROGRESS.md`.
