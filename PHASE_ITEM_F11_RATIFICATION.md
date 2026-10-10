# ADR-STARTUP-01 — RATIFICATION PACKAGE (for the maintainer's decision)

Prepared 2026-10-10 by the F-11 closure-disposition mission. This file asks for a decision; it does not make one. Nothing here edits `PHASE_ITEM_F11_ADR.md` or `docs/PROJECT_STATE.md`. Evidence: `PHASE_ITEM_F11_CLOSURE.md` (HEAD `09082c23601f7d0b75b659d12acf7133f7dc1b01` when written), `PHASE_ITEM_F11_ADR.md`.

## 1. The decision to ratify (one sentence)

When a startup bound has fired and the server thread has not ended within `RUBIXDB_STARTUP_GRACE_MS`, `rubixdb gui` and the plain CLI's become-the-owner path end the process with `process::exit(1)` (a process termination, not a shutdown), enforced by a timed wait on the calling thread, instead of waiting for the engine open indefinitely.

## 2. In scope of the ratification

* The CLI layer only (`cli/src/host.rs`, `EmbeddedServer::start`).
* Failure model: **process termination during startup** (the product's own `process::exit(1)`), and the same state reached by `TerminateProcess`.
* The Windows host of the evidence (Windows 10 build 19045, rustc 1.98.1).
* The lifetime contract of ADR section 3, decision 1: an engine open that outlasts the 30 s bound ends at 30 s + grace after the server thread starts; a server start that outlasts the 15 s `/healthz` bound after readiness ends at (readiness, at most 30 s) + 15 s + up to 2 s (the last probe's own timeout) + grace (measured 22.0 s with a 5 s grace).
* The test seam (`startup-probe` feature, 14 named pause points, absent from the product binary) and its 14 feature-gated calls in `src/lsm/mod.rs`, `src/ops/format.rs`, `src/ops/wal_tail.rs` (none a protected path).

Also in the tree since this package was first written, **not part of the decision ratified here** (each has its own record and evidence): the stamp-before-open and interrupted-stamp recognition (`PHASE_ITEM_F11_OI03_ADR.md`, `src/ops/format.rs`, `cli/src/host.rs`), the stale quarantine temp sweep and the `check` wording (`PHASE_ITEM_F11_OI02_ADR.md`, `src/ops/wal_tail.rs`, `src/ops/check.rs`) and a 15th probe point.

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

## 7. The exact lines for the maintainer

**Add to `PHASE_ITEM_F11_ADR.md`** — replace, in line 3, everything from `**Status:** PROPOSED` through `... and F-11's closure document says so.` with:

```
**Status:** ACCEPTED -- ratified <YYYY-MM-DD> by <name>, <role>; scope exactly as in PHASE_ITEM_F11_RATIFICATION.md section 2-3.
```

**Remove from `PHASE_ITEM_F11_CLOSURE.md` section 1** (line 9) the first three sentences, i.e. exactly:

```
One condition is stated, not resolved: **ADR-STARTUP-01 is PROPOSED.** The mission text asks for the ADR before the hard-exit policy is retained as certified behaviour and it is written, but this mission has no maintainer ratification to record. Until the maintainer ratifies it, "certified" in the sense of `CLAUDE.md` applies to the evidence, not to the policy decision.
```

(keep the sentence that follows it, "Not declared: whole-product production readiness ..."), and remove the last bullet of section 11, `* The maintainer's ratification of ADR-STARTUP-01.` (line 182). Do not rewrite sections 1-12 otherwise: append the ratification as a dated line in section 13.

Whether to remove "F-11" from the blocker list in `docs/PROJECT_STATE.md` (line 37) is a separate decision; ratification does not do it by itself.

## 8. If the maintainer does NOT ratify

* F-11 stays a blocker in `docs/PROJECT_STATE.md`.
* The evidence stands (it is evidence of behaviour, not of approval).
* The policy is **in effect, unratified**: the code behaves as documented, and the next session must not treat the hard exit as certified behaviour, must not cite F-11 as closed, and must not remove F-11 from the blocker list.
* Alternative B (cooperative cancellation) becomes the open design question and needs its own ADR and an engine-change authorisation.

Decision: ____ RATIFY ____ REJECT ____ AMEND (state):

Name / role / date: ______________________________
