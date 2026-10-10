# ADR-F11-OI02 — sweep stale quarantine temp files at start, name them in `check` (OI-F11-02)

**Date:** 2026-10-10. **Status:** IMPLEMENTED by the F-11 real-closure mission (product layer, `src/ops/wal_tail.rs`, `src/ops/check.rs`; not a protected path). An addition to ADR-WAL-01's quarantine; nothing it decides is changed.

## 1. Context
`quarantine_tail_with` (`wal_tail.rs`) writes `wal-<segment>.<offset>.<ms>.tail.tmp`, `fsync`s, renames to `.tail`. A start interrupted in between leaves the temp file; the next start picks a fresh name, so the leftover was never removed (3 of 3 runs). Correction to the 2026-10-10 closure text: `rubixdb check` **did** list it (Info, "unrecognised or incomplete quarantine entry ... not a valid preserved tail", `check.rs`), only generically; the closure said it did not mention it because the last output line was all that was read.

## 2. Decision
* `apply_tail_policy` removes every regular file in `wal-quarantine/` named exactly `wal-<digits>.<digits>.<digits>.tail.tmp` (`is_quarantine_tmp_name`), **after every refusal has been decided and after this start's own quarantine write finished**, before the attestation is consumed. A refused start still leaves the directory unmodified (unit-tested). Never touches `*.tail`, any other name, or a directory of that name. A failed removal is reported on stderr, never fatal, retried at the next start. One stderr line per start (`removed N stale quarantine temp file(s) ...`) only when something was removed.
* Safe because a temp file is never read as an entry (`find_existing` and `inspect_quarantine_file` accept only a complete `*.tail`), its only writer is the owning process's own quarantine write (finished before the sweep), and the instance lock forbids a second start of the same directory; were one to race, its rename would fail and its quarantine would fail closed (the start is refused), never succeed silently.
* `check` (read-only, Info, exit counters unchanged) now says `stale quarantine temp ... removed at the next start` for such a name.
* Probe point `quarantine_tmp_swept` (hit only when a file was removed).

## 3. Alternatives rejected
Sweep at the start of `apply_tail_policy` (before the decisions): rejected, a refused start promises "The directory has not been modified". Sweep in `check`: rejected, `check` is documented read-only. Sweep by age: rejected, a clock is not needed and not trusted.

## 4. Evidence
Unit (`src/ops/wal_tail_tests.rs`): `only_the_exact_quarantine_temp_name_is_a_stale_temp_file`, `the_guard_sweeps_stale_quarantine_temp_files_and_nothing_else` (a complete entry, an unrelated file, a near-miss name and a same-named directory survive; idempotent), `a_refused_start_does_not_sweep_and_leaves_the_directory_unmodified`, `check_names_a_stale_quarantine_temp_and_its_exit_counters_do_not_change`. Real binary: phase `quarantine_tmp_swept` (fixture S: a stale temp present at the start; at the pause it is gone and this start's own entry exists), and after **every** phase restart the suite now asserts that no `*.tail.tmp` survives and no complete entry present at the pause was lost (`interrupted_between_the_quarantine_copy_and_its_rename`: the temp left by the interruption is removed by the restart). Non-vacuity: against the unchanged `92f8774` code the temp survives the restart and the new phase never reaches its point (`fix_negative_old_code.log`).

## 5. Status
| Criterion | Status |
|---|---|
| a stale `*.tail.tmp` is removed by the next start | PASS |
| refused starts, `*.tail`, other files unchanged | PASS |
| `check` names it, exit code unchanged | PASS |
| sweep cannot delete a file a live writer needs | PASS by the argument above for the supported single-owner case; two concurrent starters of one directory: NOT TESTED (fail-closed by construction) |
