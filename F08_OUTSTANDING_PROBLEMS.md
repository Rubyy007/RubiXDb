# F-08 SSTable Integrity: every problem still to be handled

Compiled 2026-10-11 from the F-08 verification, the GAP-B work, and the
records (`PHASE_ITEM_F08_VERIFICATION.md` sections 1-15, `OPEN_ITEMS.md`,
`PROGRESS.md`, `CHANGELOG.md`). This file only collects. It decides nothing and
changes no code. F-08 is **not complete**, and nothing here is a
production-readiness claim.

Status words: **OPEN** = needs a fix or a decision. **NOT TESTED** = no
evidence either way. **ACCEPTED LIMIT** = known and documented, no fix planned.

---

## A. Open engineering problems (the named gaps)

| ID | Problem | Why it matters | What is needed | Status |
|---|---|---|---|---|
| A-1 (GAP-A) | The verification pass defaults to 64 MiB/s and that default is unvalidated. Measured: 63.8 MiB/s at the 64 setting, 247 at 256, 926 warm / 161 cold at the 1024 cap. CPU is about 5 % of one core at 64. Cold-cache foreground p95/p99 rises about 2x / 2.4x (only n=2). | The pass is a real throttle. The default may be too slow for large data, or too costly for foreground latency. The cold-cache number rests on two runs only. | A maintainer decision on the default, plus more runs if the cold-cache cost matters. | OPEN |
| A-2 (GAP-C) | Semantic bloom damage is invisible. A bloom block zeroed with a recomputed CRC passes every check and hides the table's keys. | Reads of present keys would silently return "absent". This is the one case where a bloom problem causes wrong answers. | A maintainer decision: whether a semantic check (for example, comparing the bloom against the records) is wanted, and at what cost. | OPEN |
| A-3 (GAP-D) | The online `check` returns an all-clear without doing the physical checks. | An operator can read "healthy" when nothing physical was examined. | Decision: make the online check say what it did not check, or make it check. | OPEN |
| A-4 (GAP-E) | The SQL route returns 500 `STORAGE_ERROR` for a corrupt table. The KV and range routes return 500 `CORRUPTION`. | Clients cannot tell corruption from other storage errors on the SQL path. | Decision on a typed code for SQL. No new error codes were authorised so far. | OPEN |
| A-5 (GAP-F) | A stale comment in a protected path (the spec's section 3.5 claim is out of date: the WAL is purged after a flush through `pool.purge_before`). | A reader of the code is misled. An additive erratum to the spec is already committed (`35bc86e`); the protected comment itself is not changed. | Authorisation to edit the protected comment. | OPEN |
| A-6 | `rubixdb check --data-dir` is **not read-only**. It adopts an unrecorded table, silently deletes a stale `.sst.tmp`, and truncates a torn Manifest tail. | A command people expect to be an inspection changes the data directory. | Decision: make it truly read-only, or rename it and document it as repairing. | OPEN |
| A-7 | Damage that happens while the server is running is found only at the next start. | This is a limit stated in ADR-SST-01. A damaged table can serve wrong or failed reads until a restart. | Accept it, or add a continuing check. | ACCEPTED LIMIT (needs ratification) |
| A-8 | A `record_count` that is below the per-table bound but disagrees with the stored records is not detected at start or by the verification pass. `rubixdb check` does report it. | A wrong count passes silently. | Decision on a mismatch policy (the earlier mission said not to add startup validation). | OPEN |

## B. Compaction problems found during the GAP-B work

| ID | Problem | Status |
|---|---|---|
| B-1 | **Commit before verify.** `compact_once_impl` writes the output, commits it to the Manifest (`AddSstable` and `RemoveSstable` for each input), and only then opens the output with `SsTable::open`. Any open failure leaves a committed, unreadable table. The GAP-B fix guards only the record-count cause. | OPEN. Protected-adjacent, needs authorisation. |
| B-2 | **`CapacityExceeded` is classified transient** (`other`), so for any other cause of an unreadable output the worker never reaches `blocked` and retries forever. In the nine-table case this wrote about 285 MB roughly every 5-7 seconds, nine outputs in 60 seconds. | OPEN. |
| B-3 | **Format limit.** A database whose compaction survivors exceed 214,748,364 records cannot be compacted into one table, because the bloom cap is 256 MiB at the pinned 10 bits per key. Compaction now blocks safely, and read amplification then grows. | ACCEPTED LIMIT (needs ratification) |

## C. NOT TESTED (no evidence either way)

| ID | Item |
|---|---|
| C-1 | The compaction guard at the **real** bound (more than 214,748,364 survivors). Only a scaled-cap test ran (`bloom_bits_per_key` 3,000,000, cap 715, 1,428 survivors). It uses the same code path with a different cap, so it is **not** equivalent evidence. |
| C-2 | The best-effort `fsync_dir` after the guard's file removal, on Unix. On Windows that helper does nothing. |
| C-3 | The debug and release **workspace** suites were not run after the apply mission. `f07_tail_damage_integration` was not run either. |
| C-4 | The mutants (6 of 6 killed) and the non-vacuity check ran on the scratch copy, not on the repository. The repository differs from the scratch copy only by the three `fsync_dir` lines. |
| C-5 | Power-loss behaviour. It stays NOT TESTED unless a real test is run. |

## D. Test and tooling problems

| ID | Problem | Status |
|---|---|---|
| D-1 | `observability_integration::two_real_instances_keep_every_surface_apart_and_a_restarted_one_starts_fresh` fails intermittently. It asserts that two instances report different start times, and both spawn at once, so they can report the same millisecond. Rates: 1 of 40 on unmodified code, and 1 of 47 on the current tree. Classified pre-existing. A failure leaves two leaked servers that lock `target\debug\rubixdb.exe`. | OPEN. Not caused by GAP-B. Needs a fix to the test or the instance start time. |
| D-2 | The evidence harness depends on `E:\rubixdb_closure\recon.py`, which is **outside the repository**. | OPEN. Evidence cannot be fully re-run from a clean clone. |
| D-3 | Fixtures, extra binaries and logs live outside the repository in `E:\rubixdb_f08b\`. Only scripts, raw JSON and logs are in `scratch/f08_verification/`. | OPEN. |
| D-4 | `repo_hygiene` fails on any 64-hex string or credentials file in tracked files, so evidence has 64-hex strings elided (shown as `abc...def`). Exact hashes cannot be re-checked from the repository. | ACCEPTED LIMIT |

## E. State of the working tree and the records

| ID | Problem |
|---|---|
| E-1 | **Nothing from the GAP-B work is committed.** Modified: `CHANGELOG.md`, `OPEN_ITEMS.md`, `PHASE_ITEM_F08_VERIFICATION.md`, `PROGRESS.md`, `missions/ACTIVE.md`, `src/lsm/mod.rs`, `src/ops/sstable_integrity_tests.rs`, `src/sstable/bloom.rs`, `src/sstable/mod.rs`, `src/sstable/reader.rs`. New: `tests/compaction_summed_hint.rs`, `tests/sstable_record_count_bound.rs`, and the `scratch/f08_verification/` additions. A maintainer must decide when to commit. |
| E-2 | **Protected-path edits are uncommitted and need ratification:** `src/sstable/bloom.rs` and `src/sstable/reader.rs` (authorised for the per-table bound), and `src/sstable/mod.rs` (a one-line re-export, authorised for the apply mission). `src/lsm/mod.rs` is an engine edit, also authorised. |
| E-3 | `missions/ACTIVE.md` is modified (uncommitted). The maintainer should confirm its text matches the missions actually run. |
| E-4 | Unrelated work by the user is in the tree and was not touched: modified `frontend/src/api/client.ts`, `queries.ts`, `types.ts`, and untracked `frontend/src/ide/`. Anyone committing must keep these separate from the GAP-B commit. |
| E-5 | F-08 has not been marked complete, and `docs/PROJECT_STATE.md` was not touched. |

## F. Environment problems

| ID | Problem |
|---|---|
| F-1 | A `rubixdb gui` (pid 21004, not started by this work) holds port 302 and the file `target\release\rubixdb.exe`. A release build into `target\release` fails with "Access is denied", and a disposable instance must use another port (3302 was used). Only one server fits on port 302. |
| F-2 | Windows locks a running exe. A leaked test server blocks the next `cargo test`. |
| F-3 | Git Bash `sleep` chains and `strings` are not available. Scripts must be written as files (heredocs with apostrophes or triple quotes failed). |
| F-4 | Files are CRLF in git (autocrlf). Scripts normalise on read. |

## G. Older items still owed (from earlier F-08 / F-11 missions)

| ID | Item |
|---|---|
| G-1 | ADR-STARTUP-01 ratification. |
| G-2 | OI-F11-05 owner. |
| G-3 | The F-11 blocker is untouched and must stay untouched unless a mission says otherwise. |
| G-4 | F-08 pass 1 classified the work PARTIALLY VERIFIED. That classification stands until the items above are closed. |

## H. Paused work (not an engine problem)

| ID | Item |
|---|---|
| H-1 | The Master UI design prompt (7 screens at 1440 / 1024 / 390) and the Worksheet & Notebook SQL IDE design prompt (7 screens at 1440 plus mobile) are **not delivered**. A private canvas exists at `claude.ai/artifact/QJD8MLdgjhxfzzdrCaBiqN` but it is empty, and it must be re-read before any edit. A stylesheet was drafted in the scratchpad and never published. |
| H-2 | The design prompts must be reconciled with the real endpoints before use. Known mismatches: readiness is `/readyz`, not `/v1/readyz`, and the events endpoint takes only a `limit`. |
| H-3 | The repository already contains a SQL IDE in `frontend/src/ide/` and recent frontend commits. The design work should be checked against it before restarting, so the two do not diverge. |

---

## What is true right now

- The per-table `record_count` bound and the compaction summed-hint clamp with its pre-commit guard are **implemented and verified at the narrow level** (all targeted suites, `cargo fmt --check` and `clippy -D warnings` exit 0, nine-table scenario clean on a repository build).
- The guard at the real bound and the Unix `fsync_dir` are **NOT TESTED**.
- Everything else in sections A, B, D, E and G is open and awaits a maintainer decision or authorisation.
- **Verdict: PASS WITH OPEN ITEMS.** Not production ready. Nothing is committed or pushed.
