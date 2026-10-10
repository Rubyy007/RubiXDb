# ADR-STARTUP-01 — RATIFICATION PACKAGE (for the maintainer's decision)

Prepared 2026-10-10 by the F-11 closure-disposition mission. This file asks for a decision; it does not make one. Nothing here edits `PHASE_ITEM_F11_ADR.md` or `docs/PROJECT_STATE.md`. Evidence: `PHASE_ITEM_F11_CLOSURE.md` (HEAD `09082c23601f7d0b75b659d12acf7133f7dc1b01` when written), `PHASE_ITEM_F11_ADR.md`.

## 1. The decision to ratify (one sentence)

When a startup bound has fired and the server thread has not ended within `RUBIXDB_STARTUP_GRACE_MS`, `rubixdb gui` and the plain CLI's become-the-owner path end the process with `process::exit(1)` (a process termination, not a shutdown), enforced by a timed wait on the calling thread, instead of waiting for the engine open indefinitely.

## 2. In scope of the ratification

* The CLI layer only (`cli/src/host.rs`, `EmbeddedServer::start`).
* Failure model: **process termination during startup** (the product's own `process::exit(1)`), and the same state reached by `TerminateProcess`.
* The Windows host of the evidence (Windows 10 build 19045, rustc 1.98.1).
* The lifetime contract of ADR section 3, decision 1: an engine open that outlasts the 30 s bound ends at 30 s + grace after the server thread starts; a server start that outlasts the 15 s `/healthz` bound after readiness ends at (readiness, at most 30 s) + 15 s + up to 2 s (the last probe's own timeout) + grace (measured 22.0 s with a 5 s grace).
* The test seam (`startup-probe` feature, 15 named pause points, absent from the product binary) and its 15 feature-gated calls in `src/lsm/mod.rs`, `src/ops/format.rs`, `src/ops/wal_tail.rs` (none a protected path).

Also in the tree since this package was first written, and referred to by the amended ADR section 5 (bullets 2 and 3): the stamp-before-open and interrupted-stamp recognition (`PHASE_ITEM_F11_OI03_ADR.md`, `src/ops/format.rs`, `cli/src/host.rs`), the stale quarantine temp sweep and the `check` wording (`PHASE_ITEM_F11_OI02_ADR.md`, `src/ops/wal_tail.rs`, `src/ops/check.rs`) and a 15th probe point. The amended ADR records their outcome (FIXED); their designs are those two mini-ADRs (status IMPLEMENTED), each with its own evidence, and are **not** the decision ratified in section 1.

## 3. Explicitly NOT in scope

* Power loss or OS crash during any startup phase (NOT TESTED).
* The work before the server thread exists: config parsing, instance lock, listener bind, runtime construction (outside both bounds; not claimed bounded).
* Cooperative cancellation of the engine open (alternative B below).
* Platforms other than Windows; the standalone `rubixdb-api` binary (unsupported, no startup bound).
* A WAL above 8.0 MB; many SSTables; large catalogs; a genuinely slow disk; a kill inside the body of a protected function at finer-than-call grain (`create_new_segment_file`, the `set_len` calls, `Manifest::append_sync`) and the watchdog firing inside an engine recovery flush or index recovery at a named point: all NOT TESTED, sampled at most by the random-instant sweep.
* Whole-product production readiness.

## 4. Alternatives rejected (ADR section 4 lists six, A-F; the three the maintainer asked about first)

| | Alternative | One-line reason |
|---|---|---|
| A | Keep waiting (pre-F-11 join, notice only) | The original defect: the process outlives its bound by the open's duration, silently, port bound and nothing answering. |
| B | Cooperative cancel token through `LsmEngine::open` / WAL / Manifest | Needs protected-path changes (`src/wal`, `src/manifest`), its own ADR, an explicit engine-change authorisation and a partially-open cleanup path that does not exist; not authorised. Remains the remedy if a bound independent of the process is ever required. |
| C | Abandon the thread and return the error | Leaves engine, locks and a bound listener alive in a process that reported "failed to start"; it can still write after the failure; ends in `process::exit` in this binary anyway. |

Also rejected in the ADR: D `abort()` instead of `exit(1)`; E shorter bounds (default 60 s and range 5-3600 s unchanged); F keep the separate watchdog thread (its "waiting without a bound" fallback made the bound conditional).

## 5. Consequences the maintainer accepts by ratifying

Updated by the F-11 real-closure mission. Two of the three consequences this section used to ask the maintainer to accept were **fixed** instead; one was not and is the only tracked consequence left.

| Consequence | Status | Record |
|---|---|---|
| A stray `wal-quarantine/*.tail.tmp` left forever after an interruption between the quarantine temp write and its rename | **FIXED** (`OI-F11-02`): the next start removes it after every refusal is decided; `check` names it as a stale temp; none survived any of the 51 phase restarts; before the fix 3 of 3 runs of the interrupting phase left one | `PHASE_ITEM_F11_OI02_ADR.md`; `src/ops/wal_tail.rs`; tests `the_guard_sweeps_stale_quarantine_temp_files_and_nothing_else` and 3 more, phase `quarantine_tmp_swept` |
| A first start ended between the engine open and `stamp_if_fresh` leaves a directory that is classified legacy and never stamped | **FIXED** (`OI-F11-03`): a fresh directory is stamped before the engine creates anything; this build's own interrupted stamp is recognised and completed; a legacy directory of an earlier release is still never touched | `PHASE_ITEM_F11_OI03_ADR.md`; `src/ops/format.rs`, `cli/src/host.rs`; 4 unit tests + 3 real-binary tests |
| A graceful stop arriving during the start's own recovery flush aborts the flush, leaves the published table for the next start's orphan adoption, and the engine declines the attestation (explained on stderr, data intact) | **NOT FIXED** (`OI-F11-04`, P2): the working fix reverses a rule pinned by an approved F-07 test; it needs the maintainer's decision; measured prototype 15 of 15 attested | `PHASE_ITEM_F11_OI04_ADR.md` |

ADR section 5 also records two further consequences that ratification accepts: after a hard exit the next start is *unattested* (as after any kill; the guard decides its refusal before it consumes the attestation); and `fail_after_bound` can end the process from a library-like module (`rubixdb-cli` is a binary crate; only `gui` and the become-the-owner path call it). **ADR section 5, bullets 2 and 3 (stray temp file, unstamped first start) describe the behaviour before the OI-F11-02 / OI-F11-03 fixes and are superseded by those two ADRs**; the ADR text was not edited (the status line and the AC-2 row are the only parts of it that this process changes).

## 5b. What ratification does not do

Ratifying ADR-STARTUP-01 accepts the timeout policy and the consequences remaining in section 5. It does not close any OI-F11-* row; does not remove F-11 from the blocker list in `docs/PROJECT_STATE.md`; and does not imply whole-product production readiness. Each is a separate disposition on its own evidence.

## 5c. Ratification is against the amended ADR

`PHASE_ITEM_F11_ADR.md` section 5 bullets 2 and 3 now read as historical, superseded by `PHASE_ITEM_F11_OI02_ADR.md` and `PHASE_ITEM_F11_OI03_ADR.md`. The maintainer is ratifying the ADR as amended, not the pre-amendment text. The two tracked consequences this package still asks the maintainer to accept are: (a) after a hard exit the next start is unattested (as after any kill, unchanged from before F-11); (b) `fail_after_bound` can end the process from a library-like module (`rubixdb-cli` is a binary crate; only `gui` and the become-the-owner path call it). Neither is a defect; both are properties of the retained design. (`OI-F11-04` is not among them: it is NOT FIXED, stays open and is not accepted by ratifying; see section 5b.)

## 5d. Items that stay open and are not decided by this ratification

| Item | Status recorded in `OPEN_ITEMS.md` | What a maintainer decision would be |
|---|---|---|
| `OI-F11-04` attestation declined after an aborted recovery flush | **OPEN, NOT FIXED** (P2) | A policy question separate from ADR-STARTUP-01, for the owner of ADR-WAL-01 (F-07): *should the clean-stop attestation be written when the newest WAL segment holds only its header and the last valid record ends exactly at the end of the preceding segment?* Today the rule says no, and an approved F-07 unit test (`the_attestation_is_not_written_for_a_corrupt_torn_or_inconsistent_directory`, the header-only case) pins that. A scratch prototype that says yes attested 15 of 15 where the shipped rule attested 6 of 15 (`PHASE_ITEM_F11_OI04_ADR.md`); nothing was applied. Nothing in this package changes the rule or the test. The decision request, with the one-line answer needed, is `PHASE_ITEM_F07_OI04_ADR.md` (authorisation gate not met on 2026-10-10; nothing applied). |
| `OI-F11-05` the interruption suite is manual and feature-gated | **OPEN, owner NOT ASSIGNED** (P2) | The repository records no authorised owner; none is assigned here. The maintainer names an owner and a trigger (the row proposes one: before any release and after a change to the files it lists). |
| `OI-F11-01` | intentionally unused (windows-sys sub-task, answered in closure 13.1) | none |

## 6. Evidence index (closure section 8; raw logs in `E:\rubixdb_f11c\logs\`)

| ID | Status | File | Count | Raw log |
|---|---|---|---|---|
| AC-1 contract explicit | PASS | `cli/src/startup_env.rs` unit tests; `cli/tests/f11_startup_grace_integration.rs` | 6 of 6 | `f11_grace_existing.log` |
| AC-2 hard bound at the named points (narrowed, text below) | PASS | `cli/tests/f11_startup_interruption_integration.rs` | 51 of 51 phase runs (17 scenarios x 3), exit 35.03-35.08 s (21.99-22.02 s `/healthz` path); default grace 90.07 s; owner path 35.05 s | `recert_matrix.log` (owner path: its own test in the same file, `addendum_owner_path.log`) |
| AC-3 no conditional fallback | PASS | `cli/src/host.rs` (`grace_expired`), unit test; `git grep "waiting without a bound" -- cli src` empty | 1 unit test | workspace logs |
| AC-4 restart integrity | PASS | interruption file | 51 of 51 (2,500 / 4,000 / 7,540 rows) | `recert_matrix.log` |
| AC-5 never reported as shutdown | PASS | interruption file | 51 of 51 | `recert_matrix.log` |
| AC-6 normal paths unchanged | PASS | interruption file, grace file, workspace suites | controls + 6 of 6 | `cert_final_all.log`, `f11_grace_existing.log` |
| AC-7 deadline race | PASS | interruption file | 24 runs (12 joined / 12 hard exit) | `recert_matrix.log` |
| AC-8 random-instant kills | PASS | interruption file | 200 kills, 97 during startup | `recert_matrix.log` |
| AC-9 product binary free of the probe | PASS | `grep -c -a "RUBIXDB_TEST_STARTUP_PAUSE" target/release/rubixdb.exe` | 0 (probe build: 1); product SHA-256 `f3592a1fb9503bd9bdab2cbad8b31266c6332ee1ade8a461a06c508cf3449121` at `33a91da` | command in closure section 8 |
| AC-10 no protected path | PASS | `git diff aad45d0..HEAD --stat -- src/wal src/manifest src/sstable src/compaction src/error.rs` | empty | command |
| AC-11 documentation consistent | PASS | ADR, closure, `gui --help`, PROGRESS / CHANGELOG / OPEN_ITEMS | n/a | files |

**AC-2 as ratified (narrowed):** A stuck startup ends the process by itself within the bound plus the grace (plus the documented `/healthz` granularity) at each of the 14 named probe points of section 6 and at the 15th, `quarantine_tmp_swept`, added by `PHASE_ITEM_F11_OI02_ADR.md` (17 scenarios; two replay an 8.0 MB WAL), through `rubixdb gui` and through the plain CLI's become-the-owner path; a process still alive past that limit fails the test. This is a claim about those named points and the documented timing paths only. Interruption inside the body of a protected function at finer-than-call grain, inside an engine recovery flush or index recovery at a named point, a WAL above 8.0 MB, and the pre-thread work (config parsing, instance lock, listener bind, runtime construction) remain NOT TESTED (closure section 11).

Regression and baseline comparison: closure section 9, the section 13 addendum and section 14.

### 6.1 Non-vacuity of the tests added or strengthened by the real-closure mission

**9 of 29 added or strengthened test functions were observed to fail on the unfixed `92f8774` tree. 6 more were run there and passed by design (controls, a coverage ratchet, a test of a function that did not exist) and each of those 6 was shown to fail under a deliberate mutation of the fixed code. 14 were not run on `92f8774`: 13 pre-existing phase tests whose added after-restart assertions have nothing to catch in their scenario, and the random-kill sweep.** (`92f8774` = the tree before any of the fixes; the new test files were copied into it unchanged, with definition-only shims where a test names a helper that did not exist; HEAD `33a91da`'s code with the same tests is the passing side, section 14.3 of the closure.)

Observed to FAIL on `92f8774` (non-vacuity checked) - 9:

| Test | Failure on `92f8774` |
|---|---|
| `src/ops/format.rs` `a_lone_interrupted_stamp_is_still_a_fresh_directory_and_gets_stamped` | `format.rs:308` `left: Legacy` |
| `src/ops/format.rs` `data_beside_this_builds_own_stamp_temp_is_stamped_not_left_legacy` | `format.rs:334` `left: Legacy` |
| `src/ops/format.rs` `stamping_creates_a_missing_directory_and_never_overwrites_a_marker` | `format.rs:377` `IO ... The system cannot find the path specified` |
| `src/ops/wal_tail_tests.rs` `the_guard_sweeps_stale_quarantine_temp_files_and_nothing_else` | `wal_tail_tests.rs:1221` `left: []` |
| `src/ops/wal_tail_tests.rs` `check_names_a_stale_quarantine_temp_and_its_exit_counters_do_not_change` | `wal_tail_tests.rs:1293` old text "unrecognised or incomplete quarantine entry" |
| `cli/tests/f11_startup_interruption_integration.rs` `data_left_beside_this_builds_stamp_temp_is_stamped_at_the_next_start` | `:1608` `marker: false, marker_tmp: true` after the restart |
| same file `interrupted_after_stale_quarantine_temp_files_were_swept` (the new phase) | `:1320` the probe point was never reached in 60 s |
| same file `interrupted_between_the_format_marker_temp_and_its_rename` (strengthened: stamp before engine files; stamped after restart) | `:1329` the engine's WAL file exists at the stamp pause; with that clause skipped in the scratch copy: `:985` "the directory must be stamped after the restart" (`nv_B6_old_skip_S1.log`) |
| same file `interrupted_between_the_quarantine_copy_and_its_rename` (strengthened: no stale temp after a restart) | `:1398` `wal-1.2652920.<ms>.tail.tmp` survived the restart |

Run on `92f8774` and PASSED, by design; shown non-vacuous by mutation of the fixed code - 6:

| Test | Why it passes on `92f8774` | Mutation (scratch copy of HEAD) and result |
|---|---|---|
| `format.rs` `data_beside_a_temp_file_this_build_did_not_write_stays_legacy_and_unmodified` (control) | the unfixed code never stamped a legacy directory | B1 (naive fix: every legacy directory counts as an interrupted stamp): FAILS |
| `cli/tests/...` `data_beside_a_foreign_temp_file_is_not_stamped_and_nothing_is_lost` (control) | same | B1: FAILS |
| `wal_tail_tests.rs` `a_refused_start_does_not_sweep_and_leaves_the_directory_unmodified` (control) | the unfixed code never swept | B2 (sweep runs before the refusals): FAILS |
| `wal_tail_tests.rs` `only_the_exact_quarantine_temp_name_is_a_stale_temp_file` | pure predicate; it did not exist, run only with a shim holding HEAD's text, so passing there proves nothing | B3 (predicate accepts every `wal-*` name): FAILS |
| `cli/tests/...` `interrupted_after_the_quarantine_rename_before_the_attestation_is_consumed` (strengthened: a complete entry present at the pause survives the restart) | the unfixed code had no sweep to lose it | B3: FAILS |
| `cli/tests/...` `every_phase_has_a_test_and_every_probe_point_is_covered` (ratchet, 17 scenarios) | the unfixed point set (14) is covered by the 17 scenarios | B5 (the `quarantine_tmp_swept` scenario removed from the table): FAILS, "no scenario for quarantine_tmp_swept" |

Additionally mutation B4 (`stamp_if_fresh` overwrites an existing marker) fails `stamping_creates_a_missing_directory_and_never_overwrites_a_marker`'s second half.

NOT run on `92f8774` - 14, with the reason:

* 13 pre-existing phase tests, each strengthened by the same two after-restart assertions (no stale temp survives; a complete entry present at the pause is not lost), which **cannot fail on `92f8774` in these scenarios** because there is no stale temp and, with one exception, no complete entry for them to catch: `interrupted_in_the_guards_wal_preflight_replay`, `..._of_a_large_wal`, `interrupted_inside_the_engines_wal_replay`, `..._of_a_large_wal`, `interrupted_after_the_attestation_is_consumed_before_the_engine_opens`, `interrupted_after_wal_replay_before_the_wal_is_opened`, `interrupted_after_the_wal_tail_was_truncated`, `interrupted_after_the_manifest_tail_was_truncated`, `interrupted_after_the_stale_sst_tmp_was_swept`, `interrupted_before_the_manifest_adopts_an_orphan_table`, `interrupted_after_the_manifest_adopted_an_orphan_table`, `interrupted_at_the_end_of_engine_open`, `interrupted_after_ready_was_signalled_before_serving`. In `interrupted_after_the_wal_tail_was_truncated` (verified by running it under mutation B3: it PASSES) and, by the same sequence, `interrupted_after_wal_replay_before_the_wal_is_opened` (NOT run under B3), a complete entry exists at the pause in the unmutated code, but the mutated sweep removes it at the first start, before the pause, so the kept-entry assertion cannot catch that mutation there. The strengthened clauses are regression guards in these 13, not detectors of OI-F11-02 / OI-F11-03; the tests' own earlier assertions are unchanged.
* `random_instant_kills_during_startup_never_damage_the_database` (strengthened through `restart_and_verify_fresh`, the 40 empty-directory kills): probabilistic (a kill must land in the stamp window), so not run on `92f8774`; the same helper clause is the one that failed in the format-marker phase above.

Evidence: `nv_unit_old_panics.txt`, `nv_integration_old.log`, `nv_B6_old_skip_S1.log`, `nv_mutations.txt` in `E:\rubixdb_f11c\logs\`; scratch trees `E:\rubixdb_f11c\nv_old` (`92f8774` + HEAD's tests + shims, built by `make_nv_old.py`) and `nv_head` (mutations, `mutations.py`). Nothing in the repository was changed to obtain them.

### 6.2 The baseline-test policy, the named regression-guard exception, and what the mutation evidence does and does not show

**Policy.** A test added for a fix must fail on the unfixed baseline (`92f8774`). That is the check in the strict sense and 9 tests meet it (6.1, first table). Three kinds of test legitimately **pass** on the baseline and are not defects for doing so: (a) a **control** that pins behaviour the fix must leave unchanged (a legacy directory stays unstamped; a refused start does not sweep); (b) a **coverage ratchet** that counts scenarios and probe points; (c) a test of a function that did not exist on the baseline. For these the evidence is not a baseline failure but a **mutation of the fixed code** that the test must catch. The six tests in 6.1's second table are of these kinds and each failed under a named mutation (B1, B2, B3, B5; `nv_mutations.txt`).

**Named exception: the 13 inert regression-guard clauses.** 13 pre-existing phase tests received two added after-restart assertions (no stale quarantine temp survives; a complete entry present at the pause is not lost). In those 13 scenarios the unfixed code produces nothing for the assertions to catch, so they could not fail on `92f8774`, and **they are not claimed to detect OI-F11-02 or OI-F11-03**: they are regression guards against a future change that would leave a stale temp or lose an entry in those scenarios. They are listed by name in 6.1.

**Verification status of the 13, stated plainly.** The repository records the 13 names and the reason they are inert (6.1; `OPEN_ITEMS.md` 2026-10-10 note; `PROGRESS.md`). The repository records **no mutation result for 12 of them**. For one, `interrupted_after_the_wal_tail_was_truncated`, the sweep-everything mutation B3 was run and the test PASSED, which confirms that its added clause is inert there. `nv_mutations.txt` records B3's real-binary run of four selected tests, two FAILED (named) and two passed; that the passing two include this test follows from the selection, and a separate manual run that printed it by name was not logged. That is the whole of the repository's mutation evidence about the 13. The statement that the 13 are "mutation-validated" is therefore **NOT supported by the repository and is not made here**; what is recorded is "inert, not run on the baseline". The assertions are not weakened and not removed.

**What the mutation evidence establishes:** for each of the six tests in 6.1's second table, a specific, plausible wrong implementation (B1 stamp every legacy directory; B2 sweep before the refusals; B3 sweep every `wal-*` name; B5 drop a scenario) makes the test fail, so the test is not satisfied by any implementation. **What it does not establish:** that the fixes are correct beyond the tests' assertions; that the mutations cover every way the code could be wrong; that tests not mutated (the 13, the sweep) would catch any particular regression; or that vacuity risk is eliminated. It is evidence of sensitivity to four named faults, nothing more.

### 6.3 Canonical counts used in this package, and where older documents differ

| Figure | Value | Evidence |
|---|---|---|
| Named pause points | **15** | `Point::ALL: [Point; 15]` at `src/startup_probe.rs:55`; 15 `startup_probe::hit(` call sites in `src/` and `cli/` outside the module |
| Phase scenarios | **17** | 17 `Phase {` entries in `cli/tests/f11_startup_interruption_integration.rs`; `assert_eq!(PHASES.len(), 17)` in `every_phase_has_a_test_and_every_probe_point_is_covered` |
| Phase runs under `F11_REPS=3` | **51** (17 x 3) | `recert_matrix.log` (`summarize.py`: "phase runs: 51 across 17 phases") |
| Earlier matrix, before `quarantine_tmp_swept` existed | **48** (16 scenarios x 3, 14 points) | `cert_final_all.log` ("phase runs: 48 across 16 phases") |

The closure (sections 6, 8, 9, 13), `PROGRESS.md` and `OPEN_ITEMS.md` entries dated before the `OI-F11-02` fix state 14 points / 16 scenarios / 48 runs. Those are **historical and accurate for the matrix they describe** (HEAD `f7f4843`, `cert_final_all.log`); they are superseded as current-state figures by this table and by closure section 14.3 (51 runs). The ADR's section 6 sentence "14 points" is likewise the original count (see the note at the start of ADR section 2).

## 7. The exact lines for the maintainer

**Add to `PHASE_ITEM_F11_ADR.md`** — replace, in line 3, everything from `**Status:** PROPOSED` through `... and F-11's closure document says so.` with:

```
**Status:** ACCEPTED -- ratified <YYYY-MM-DD> by <name>, <role>; scope exactly as in PHASE_ITEM_F11_RATIFICATION.md section 2-3; ADR section 5 as amended (bullets 2 and 3 historical), per package section 5c.
```

**Remove from `PHASE_ITEM_F11_CLOSURE.md` section 1** (line 9) the first three sentences, i.e. exactly:

```
One condition is stated, not resolved: **ADR-STARTUP-01 is PROPOSED.** The mission text asks for the ADR before the hard-exit policy is retained as certified behaviour and it is written, but this mission has no maintainer ratification to record. Until the maintainer ratifies it, "certified" in the sense of `CLAUDE.md` applies to the evidence, not to the policy decision.
```

(keep the sentence that follows it, "Not declared: whole-product production readiness ..."), and remove the last bullet of section 11, `* The maintainer's ratification of ADR-STARTUP-01.` (line 182). Do not rewrite sections 1-14 otherwise: append the ratification as a dated line after section 14.

Whether to remove "F-11" from the blocker list in `docs/PROJECT_STATE.md` (line 37) is a separate decision; ratification does not do it by itself.

This amendment (ADR section 5 bullets 2 and 3 marked historical, section 5c added here) is part of what is being ratified and must be noted in the ratification line.

## 8. If the maintainer does NOT ratify

* F-11 stays a blocker in `docs/PROJECT_STATE.md`.
* The evidence stands (it is evidence of behaviour, not of approval).
* The policy is **in effect, unratified**: the code behaves as documented, and the next session must not treat the hard exit as certified behaviour, must not cite F-11 as closed, and must not remove F-11 from the blocker list.
* Alternative B (cooperative cancellation) becomes the open design question and needs its own ADR and an engine-change authorisation.

Decision: ____ RATIFY ____ REJECT ____ AMEND (state):

Name / role / date: ______________________________
