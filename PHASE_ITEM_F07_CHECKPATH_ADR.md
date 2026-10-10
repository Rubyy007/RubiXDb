# ADR-F07-CHECKPATH — `rubixdb check` must not truncate a damaged WAL tail in place (F-07 residual (c))

**Date:** 2026-10-10. **Status:** IMPLEMENTED (product layer; no protected path). Closes residual (c) of the 2026-10-08 F-07 `OPEN_ITEMS.md` line. Does not touch ADR-STARTUP-01, the attestation eligibility rule or OI-F11-04.

## 1. Context (source-cited; reproduced on HEAD `4fdfd7f`)
* `cli/src/ops_cmd.rs` `check`: the physical pass (`check_physical`, read-only) reports `WAL_TORN_TAIL` as a warning; the logical pass then calls `open_engine_for_ops(&data_dir)` (`src/ops/open.rs`), which runs `startup_guard` (unchanged old guard: format + `WAL_CORRUPT` preflight only) and `LsmEngine::open`. Opening performs ordinary crash recovery, i.e. `FileWal::open_for_recovery` `set_len`s the torn tail **in the inspected directory**.
* The startup guard `startup_guard_with_tail_policy` (`src/ops/wal_tail.rs`) additionally copies the bytes to `wal-quarantine/` (temp, fsync, rename), reports them (stderr, security log) and evaluates the attestation. `PHASE_ITEM_F07_IMPLEMENTATION.md` section 5 left `open_engine_for_ops` alone on purpose: sharing the guard would make `check` ("INSPECTION (read only)") write a quarantine and consume the attestation, and add a refusal to `check` and `restore`. It recorded the consequence: `check` on a damaged stopped directory truncates the tail without quarantining it.
* Reproduction (`E:\rubixdb_f11c\logs\f07chk_repro_head.txt`): 42-frame WAL, one CRC bit of the last frame flipped, no attestation. `rubixdb check --data-dir DIR`: exit 1 (warning), WAL 2731 -> 2673 bytes (58 bytes, the last acknowledged record, gone), no quarantine directory, only the generic warning. The startup path on identical bytes: 58 bytes preserved in `wal-quarantine/wal-1.2673.<ms>.tail`.

## 2. Shapes
* **A (chosen).** `check` stays read-only: it *reports* what a start would do and never does it. Two parts: (1) `plan_tail` (`wal_tail.rs`), a read-only counterpart of `apply_tail_policy` that uses the very same `torn_tail_location` and attestation rule (`missing_records`), surfaced by `check_physical` as two **informational** findings (`WAL_TAIL_WOULD_BE_QUARANTINED`: bytes, offset, segment, last good sequence; `WAL_TAIL_START_WOULD_REFUSE`: attested vs reached) so exit codes and warning/error counters do not change; (2) the logical pass, which must open an engine, opens it on a **private temporary copy** of the directory whenever the plan found a torn tail (`open_engine_for_inspection`, `open.rs`); a directory without a torn tail is opened in place exactly as before. The copy is removed when the value is dropped. If the copy cannot be made (for example no space) the check fails closed with an error and the directory is untouched.
* **B (rejected).** Make `check` quarantine like a start. Changes the documented read-only contract (an inspection command would write quarantine files) and does not by itself stop the in-place truncation by the logical pass. A was preferred.
* **C (rejected).** Skip the logical pass when a tail is torn. Loses the data check exactly when the operator most wants it.
* Not shared: the startup guard is unchanged in behaviour (its step 2 now calls the extracted `torn_tail_location`; existing tests unchanged). `restore` keeps `open_engine_for_ops`. `cli/src/ops_cmd.rs` (not a protected path) changed in one call site; it was not on the mission's file list and is named here.

## 3. Failure model and limits
Covers a stopped directory (offline check). A running instance is checked through its API and is untouched. Cost: the copy needs free space in the temp directory equal to the data directory, only in the damaged-tail case. **Not changed, still true:** `check`'s logical pass can still modify the inspected directory in place for other recovery work (a torn Manifest tail, stale `*.sst.tmp`, an orphan table adopted, a recovery flush of a replayed memtable over its bound); only the WAL tail is covered here (recorded as a separate open row). Power loss: NOT TESTED.

## 4. Acceptance criteria
| ID | Criterion | Status |
|---|---|---|
| C1 | the defect reproduces on the unfixed binary (directory changes, no quarantine) | PASS (repro log; test fails on `4fdfd7f`) |
| C2 | `check` of a damaged stopped directory leaves it byte-identical (WAL torn by body flip, CRC flip, cut) | PASS (`check_of_a_damaged_stopped_directory_...`, 3 mutations) |
| C3 | the report names the exact cut a start then makes (offset, bytes), and the start quarantines exactly those bytes | PASS (same test) |
| C4 | an attested damaged directory: `check` names the refusal with the same sequences, leaves attestation and WAL intact, the start still refuses | PASS (`check_of_an_attested_damaged_directory_...`) |
| C5 | valid, and corrupt (`WAL_CORRUPT`) directories are distinguished: no cut named, corrupt keeps exit 2 and is untouched | PASS (`check_distinguishes_valid_corrupt_and_damaged_directories`) |
| C6 | the startup path (quarantine-and-truncate, refusals, override) behaves as before | PASS (the 13 existing F-07 integration tests and the existing unit tests, unchanged) |
| C7 | the private copy is removed; a valid directory is opened in place without a copy | PASS (unit tests) |
