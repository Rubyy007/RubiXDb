# D1a decision gate — F-D1a-1 and F-D0-1/2/3/4/5

Written 2026-10-13. **STEP 1 of the D1a-gate + D1b slice.** Base `dde8ae9`, nothing committed. This document records, per finding: the measured evidence, the options, the maintainer's decision, the ADR, whether a fix landed, the regression test and the contract change.

## 0.0 Resolution (2026-10-13) — read this first

The maintainer replied "Approve all recommended fixes, then proceed to D1b" (2026-10-13). Every row of the decision table below is therefore **decided as recommended**: F-D1a-1 option (ii), F-D0-1/2 (zero-filled tail = torn tail, including the two F-07-pinned assertions), F-D0-3 (rollback), F-D0-5 (floor at checkpoint + 1); F-D0-4 **accepted now, to be fixed inside the D2 manifest ADR**. All five fixes are in the repository (uncommitted) — see §10 for what changed and what was measured. The four ADRs are marked ACCEPTED/IMPLEMENTED; `docs/DURABILITY_CONTRACT.md` §12 is in force; gaps G1–G4 and G13 are closed. **F-D1a-2 stays open** (not part of the approval). The remainder of this file is the gate document as written before the decision; where it says "pending" or "not landed" it is superseded by this paragraph and §10.

## 0. Status in one paragraph (as written before the decision — SUPERSEDED by §0.0)

**No decision has been supplied, so no fix has landed and D1b (STEP 2) has not started.** For every finding there is now (a) a PROPOSED ADR, (b) a regression test that is **red on the current tree and green on a scratch prototype** (deterministic for F-D1a-1), (c) PROPOSED contract text with the "if accepted" wording alongside, and (d) a prototype diff (`docs/adr/prototypes/D1a-gate-fixes.diff`) built and measured **outside the repository**. The only repository changes this step are: the `lsm.before_apply` seam (2 behaviour-neutral statements, a registered fault point), the regression tests, the ADRs, this document, the proposed contract §12 and `OPEN_ITEMS.md` lines. The D1a suite was re-run on the repository after the seam was added (nothing was fixed there): class A 34/34, class B single writer 164/164 (3 more rows: the new point), relational 31/31, **concurrent 28/31 - 3 FAIL, every one with the F-D1a-1 signature**. On the prototype the same suite is class A 34/34, B single 164/164, **concurrent 31/31**, relational 31/31. The concurrent column therefore still shows F-D1a-1 on the repository: it is neither gone nor accepted.

**Contract coherence.** Today's contract (sections 1–11 of `docs/DURABILITY_CONTRACT.md`) is internally coherent *with its gaps stated* (G1–G4, G13). It would become incoherent only if D1b/D1c fixed expected states while F-D1a-1 is undecided, because the meaning of "acknowledged" under concurrency would differ between the contract and the engine. That is why STEP 2 waits.

**Two things learned while doing this step that change the picture** (both in the ADRs):

1. **Fixing F-D0-1 contradicts two tests that a previous mission (F-07 / ADR-WAL-01) pinned deliberately** — `ops::wal_tail_tests::existing_corruption_refusals_take_precedence_and_are_unchanged` and `cli/tests/f07_tail_damage_integration.rs::existing_wal_corrupt_refusals_are_unchanged_and_the_override_never_bypasses_them` (the `Z3Zero64` entry): a 64-byte zero WAL tail must be `WAL_CORRUPT`. They are the only existing tests in the whole workspace that fail on the prototype. Changing those two assertions needs your explicit authorisation (ADR-TORN-TAIL-ZERO-01 §5).
2. **F-D1a-2 (new, measured):** `snapshot_seq()` can exceed the applied prefix, so a snapshot — and a logical backup — can omit a durable commit. Not part of any fix here; needed before D3 (ADR-CHECKPOINT-CONTIGUITY-01 §5).

## 1. Decision table (one sentence per consequence)

"Recommendation" is mine; the **Maintainer decision** column is empty on purpose.

| # | Finding | Recommendation | Maintainer decision | Consequence if **fixed** | Consequence if **accepted** | Consequence if **excluded** |
|---|---|---|---|---|---|---|
| 1 | **F-D1a-1** acknowledged write lost under concurrent writers, also across a graceful stop | **Fix, option (ii)** (contiguous-applied watermark at freeze) | Approved 2026-10-13: fix, option (ii) | Every acknowledged commit survives a restart whatever the apply order, at the price of a small tracker and a possible checkpoint/WAL-purge stall if a durable seq is never applied. | The mission's "acknowledged commits are recovered" claim is withdrawn for any multi-writer use (the product's HTTP server and every SQL transaction). | Not supportable: reproduced deterministically and in 24 of 90 unseamed runs on a pristine export of the certified commit. |
| 2 | **F-D0-1** zero-filled WAL tail ≥ 16 B condemns the segment | **Fix** (zero run to EOF = torn) | Approved 2026-10-13: fix, incl. the two F-07-pinned assertions | A recoverable zero-tail directory starts (tail quarantined) instead of being refused, at the price of editing one F-07-pinned test assertion and narrowing clause C-TORN-1(b). | A zero-filled tail of 9+ bytes keeps making the directory unopenable (product: refused; bare engine: segment silently dropped). | Only with Tier 3/4 evidence that the shape cannot occur; none available, and D1b's simulator will produce it. |
| 3 | **F-D0-2** zero-filled MANIFEST tail ≥ 8 B refuses open | **Fix** (same rule) | Approved 2026-10-13: fix | The MANIFEST tail is truncated and reported like the WAL's. | A zero-filled MANIFEST tail of 8+ bytes keeps making the directory unopenable. | Same as #2. |
| 4 | **F-D0-3** no rollback of a failed MANIFEST write | **Fix** (truncate on failure; poison if rollback fails) | Approved 2026-10-13: fix | A transient write error can no longer turn a recoverable directory into an unopenable one. | A transient MANIFEST write error can leave a directory that needs an operator. | Only if an OS write error after a partial write is shown impossible; not shown. |
| 5 | **F-D0-4** MANIFEST shared lock is a no-op | **Accept now; fix inside the D2 manifest ADR** (see §6) | Approved 2026-10-13: accept now; fix in the D2 manifest ADR | The read-only replay would honour the engine's lock — and thereby start failing for readers of a running instance, which needs a design decision. | Readers of the MANIFEST are not excluded from a live writer; nothing assumes otherwise today, and the comment is corrected. | n/a (it is a fact, not a hypothesis). |
| 6 | **F-D0-5** seq restarts below the checkpoint when WAL content is missing | **Fix (floor)**; decide separately whether to also refuse | Approved 2026-10-13: fix (floor); refuse-to-start question stays open | New commits after a loss of WAL content can never be numbered below the checkpoint; the missing unflushed records stay missing. | After a loss of WAL content, later acknowledged commits are silently lost at the next restart. | Only with evidence that WAL content cannot go missing; none. |

## 2. F-D1a-1 — concurrent-writer acknowledged-write loss

| Field | Content |
|---|---|
| Finding | A MemTable freeze that overtakes a lower seq's apply makes the flushed table's `max_seq` — the manifest checkpoint — cover a seq present in no table; recovery skips it (`seq <= checkpoint`). |
| Measured evidence | **Deterministic:** `tests/pending_fixes.rs` `f_d1a_1_put_…`, `f_d1a_1_batch_…` FAIL on the current tree — A = seq 1 acknowledged, B = seq 2, manifest checkpoint 2, A absent from the crash image and after a graceful reopen. **Statistical (30 runs/cell, 4 writers):** current tree 7/30, 7/30, 10/30 (24/90); prototype 0/90. **Pristine `dde8ae9` export:** 5/15 (D1a appendix C). 1–2 writers: 0/15. |
| Options | (i) apply in seq order; **(ii) contiguous-applied watermark at freeze (recommended)**; (iii) replay from `min(unflushed)` (collapses into (ii)); (iv) hold the freeze (unbounded-memory failure mode) — full table with R1–R13 and costs in `ADR-CHECKPOINT-CONTIGUITY-01.md` §3. |
| Maintainer decision | **Approved 2026-10-13: fix, option (ii)** |
| ADR | `docs/adr/ADR-CHECKPOINT-CONTIGUITY-01.md` — **ACCEPTED, IMPLEMENTED** |
| Fix landed | **Yes** (repository, uncommitted; §10) |
| Regression test | `f_d1a_1_put_…` and `f_d1a_1_batch_…`: **10 of 10 invocations FAIL on the current tree, 10 of 10 PASS on the prototype** (each invocation runs both tests); guard `guard_f_d1a_1_…` green on both |
| Contract clause | C-CKPT-1, C-CKPT-2 (in force, `DURABILITY_CONTRACT.md` §12); replaces the §1 checkpoint definition |
| Prototype results | whole `fault_points` suite: class A 34/34, class B single 164/164, **concurrent 31/31**, relational 31/31. Engine lib: 686/687 (the one failure is #2's pinned test). Cost (3 alternating repetitions): no difference outside run-to-run spread — raw numbers in §8. |
| Residual risks | stall of the watermark if a durable seq is never applied (an `await_retry_budget` timeout) → WAL not purged, tracker set grows; duplicates after recovery are benign for point reads and scans but not proven for every consumer; F-D1a-2 (above) is not fixed by it. |

## 3. F-D0-1 and F-D0-2 — zero-filled tail

| Field | WAL (F-D0-1) | MANIFEST (F-D0-2) |
|---|---|---|
| Measured evidence | 4 and 8 zero bytes torn; 16, 64, 4096 → whole segment corrupt, 0 of 3 acknowledged trusted; valid frame + 4096 zeros → 0 of 4 | 4 torn; 8, 16, 64 → open fails "empty body (missing edit_type)" |
| Current-tree regression test | `f_d0_1_zero_filled_wal_tail_of_any_length_is_a_torn_tail`, `f_d0_1_valid_unacknowledged_frame_then_zeros_keeps_the_valid_frame` — **FAIL** | `f_d0_2_zero_filled_manifest_tail_of_any_length_is_a_torn_tail` — **FAIL** |
| Guards (must stay green) | `guard_f_d0_1_damage_followed_by_valid_frames_is_still_corruption`, `guard_f_d0_1_all_ff_tail_never_yields_extra_records` | `guard_f_d0_2_corrupt_middle_frame_is_still_an_error` |
| Options | fix / accept / exclude | same |
| Maintainer decision | **Approved 2026-10-13: fix** | **Approved 2026-10-13: fix** |
| ADR | `docs/adr/ADR-TORN-TAIL-ZERO-01.md` — **ACCEPTED, IMPLEMENTED** (covers both) | same |
| Fix landed | **Yes** | **Yes** |
| Contract clause | C-TORN-1(b) narrowed to "zero-filled to end of file, of any length" — **stale-garbage tails no longer promised torn** | same |
| Needs your authorisation beyond "fix" | **two existing assertions** (§0.1): the 64-byte zero tail in `ops::wal_tail_tests::existing_corruption_refusals_take_precedence_and_are_unchanged` and the `Z3Zero64` entry of `cli/tests/f07_tail_damage_integration.rs::existing_wal_corrupt_refusals_…` | none |

## 4. F-D0-3 — failed MANIFEST append

Measured at the real call site (`manifest.write=…:shortwrite=7`): 7 stray bytes, the flush retry appends after them, the MANIFEST (826 B) is refused by the product start (`checksum mismatch at offset 74 (not the trailing frame…)`, directory unmodified). Test `f_d0_3_failed_short_manifest_write_is_rolled_back_and_the_retry_leaves_a_readable_file`: **FAIL** now ("left 48 right 41"), green on the prototype. ADR `docs/adr/ADR-MANIFEST-APPEND-ROLLBACK-01.md` — **ACCEPTED, IMPLEMENTED**. Contract C-MANIFEST-1 (in force). Decision: approved 2026-10-13; fix landed **Yes**. Premise (a partial-write error from the OS) **not shown** reachable here.

## 5. F-D0-5 — sequence restart below the checkpoint

Measured: checkpoint 185, WAL deleted, first new `put` acknowledged at seq 1, lost after a clean reopen. Test `f_d0_5_seq_after_recovery_is_above_the_checkpoint_even_when_the_wal_content_is_missing`: **FAIL** now, green on the prototype. ADR `docs/adr/ADR-WAL-SEQ-FLOOR-01.md` — **ACCEPTED, IMPLEMENTED**; decided together with #1 (the tracker is initialised from the same quantities). Contract C-SEQ-1 (= R6, in force). Decision: approved 2026-10-13; fix landed **Yes**. Premise (WAL content going missing) **not shown** reachable.

## 6. F-D0-4 — MANIFEST shared lock is a no-op

* **Confirmed** again from source: `manifest::replay_readonly` takes `acquire_shared_lock_if_present(<data>/LOCK)` (`src/manifest/mod.rs:44`); the lock file is `<data>/wal/LOCK` (`src/wal/mod.rs`); `<data>/LOCK` is never created, so the lock is a silent no-op. **Confirmed present in `OPEN_ITEMS.md`** with its own line (2026-10-13, next to F-D0-1/2/3/5), and the stale module comment is listed in the D0 discrepancy table.
* **Options.** (a) Fix by pointing the shared lock at `<data>/wal/LOCK`: correct to the module comment, but then a read-only replay of a *running* instance fails (the engine holds the exclusive lock), which changes `ops::check`, `ops::sstable_integrity` and `ops::wal_tail` for in-process readers — a design decision, not a typo fix. (b) Accept: document that MANIFEST readers are not excluded from a live writer (true today, harmless today: the engine's own exclusivity comes from the WAL lock and the instance lock). (c) Exclude: n/a.
* **Recommendation:** accept now (no behaviour change; correct the comment when the file is next edited) and make the reader/writer exclusion an explicit part of the **D2 manifest ADR**, where the open path is rewritten anyway.
* ADR: none now. Decision: **accept now, fix inside the D2 manifest ADR (approved 2026-10-13)**. Fix landed **No** (by decision). Regression test: none (a test of the current no-op would pin a defect). Contract: no gap number or clause; recorded in `OPEN_ITEMS.md` and in the D0 discrepancy table.

## 7. What the repository contains after this step

| Change | Kind | Protected? |
|---|---|---|
| `src/faults.rs` + `src/lsm/mod.rs`: fault point `lsm.before_apply` (2 statements); ADR-FAULTPOINTS-01 §12.1 row; scenario list | test infrastructure, behaviour-neutral | no (`src/lsm`) |
| `tests/pending_fixes.rs` + `Cargo.toml` `[[test]]` (`required-features = ["fault-points"]`) | 7 red regression tests, 4 green guards, 1 ignored observation | — |
| `docs/adr/ADR-CHECKPOINT-CONTIGUITY-01.md`, `ADR-TORN-TAIL-ZERO-01.md`, `ADR-MANIFEST-APPEND-ROLLBACK-01.md`, `ADR-WAL-SEQ-FLOOR-01.md`, `docs/adr/prototypes/D1a-gate-fixes.diff` | PROPOSED | — |
| `docs/DURABILITY_CONTRACT.md` §12, this file, `OPEN_ITEMS.md`, `PROGRESS.md`, `CHANGELOG.md`, `missions/ACTIVE.md` | docs | — |
| **No** production behaviour change; **no** protected file edited in this step. | | |

The 7 red tests make `cargo test --features fault-points --test pending_fixes` fail by design until decisions land; they are not part of the default `cargo test --workspace` (they need the feature) and are not marked ignored.

## 8. Raw measurements

Write-path benchmark (`benchfix`, scratch crate outside the repository; product config GroupCommit 5 ms / 256 KiB, 100-byte values, 8 s per cell, release build, this SATA machine; BEFORE = current tree, AFTER = prototype; alternating):

| writers / MemTable | rep 1 before → after | rep 2 | rep 3 |
|---|---|---|---|
| 1 / 4 MiB | 314 → 304 | 257 → 290 | 277 → 284 |
| 4 / 4 MiB | 592 → 568 | 550 → 581 | 568 → 566 |
| 8 / 4 MiB | 1161 → 922 | 1074 → 1205 | 1144 → 1087 |
| 4 / 64 KiB | 516 → 505 | 543 → 535 | 541 → 504 |

(ops/s.) p50: single writer 2.8–3.2 ms both; 4 writers 6.4–7.2 ms both. p99/p999/max vary run to run in both columns. **n = 3 per cell cannot resolve a regression below roughly 5–10 %.**

## 9. The prototype against the whole existing suite

`cargo test --workspace --no-fail-fast` in the scratch copy with all five fixes applied (2026-10-13 21:21:52 → 21:52:04 IST, 30 m 12 s, `CARGO_TARGET_DIR`/`TEMP` on D:), compared binary by binary with the clean D1a-final run on the repository (55 binaries, 2,178 passed / 2 failed / 32 ignored):

| binary | repository (clean) | prototype | explanation |
|---|---|---|---|
| `rubixdb` lib | 687 / 0 / 4 | 686 / **1** / 4 | `ops::wal_tail_tests::existing_corruption_refusals_take_precedence_and_are_unchanged` — pins the 64-byte zero tail as `WAL_CORRUPT` (F-D0-1) |
| `f07_tail_damage_integration` | 16 / 0 / 0 | 15 / **1** / 0 | `existing_wal_corrupt_refusals_are_unchanged_and_the_override_never_bypasses_them` — `Z3Zero64` (F-D0-1) |
| `repo_hygiene` | 2 passed / **2 failed** | 4 / 0 | **artefact, not a fix**: the scratch copy has no `.git`, so the two "tracked file" checks have nothing to find. They remain the pre-existing failures in the repository and are untouched. |
| the other 52 binaries | identical | identical | — |

With only the `src/wal/recovery.rs` hunk reverted in the prototype, both F-D0-1 tests pass (run separately), so **the F-D1a-1, F-D0-2, F-D0-3 and F-D0-5 hunks affect no existing test** beyond the one `LsmEngine` literal that gains a field. The totals (2,178 / 2 / 32) coincide only because one failure moved from `repo_hygiene` to the two tests above.


## 10. Resolution record (2026-10-13, after the maintainer's approval)

| Finding | Fix (repository, uncommitted) | Protected path touched | Test now |
|---|---|---|---|
| F-D1a-1 | `ApplyTracker` (contiguous-applied watermark) captured under the write lock at freeze; checkpoint, WAL marker and `purge_before` use it; tracker initialised at open | `src/lsm/mod.rs` (certified Write/Read Engine code, not on the protected list) + 1 line in `src/lsm/tests.rs` | `f_d1a_1_put_*`, `f_d1a_1_batch_*`, guard: pass; 10 of 10 invocations (0 of 10 before) |
| F-D0-1 | all-zero header + all-zero remainder to end of segment = torn tail | `src/wal/recovery.rs` (**protected**; ADR-TORN-TAIL-ZERO-01) | `f_d0_1_*` (2), guards (2): pass |
| F-D0-2 | same rule in the MANIFEST (`rest_is_zero`) | `src/manifest/recovery.rs` (**protected**) | `f_d0_2_*`, guard: pass |
| F-D0-3 | `Manifest { len, poisoned }`; rollback by `set_len` + seek + `sync_all`; poison if the rollback fails | `src/manifest/mod.rs` (**protected**; ADR-MANIFEST-APPEND-ROLLBACK-01) | `f_d0_3_*`: pass |
| F-D0-5 | `FileWal::ensure_next_seq_at_least(checkpoint + 1)` called in `LsmEngine::open`, with a warning | `src/wal/mod.rs` (**protected**; ADR-WAL-SEQ-FLOOR-01), `src/lsm/mod.rs` | `f_d0_5_*`: pass |
| F-D0-4 | none, by decision (accepted; fix inside the D2 manifest ADR) | — | — |
| pinned tests | the two 64-byte-zero-tail assertions changed from "WAL_CORRUPT" to "torn tail" (authorised) | tests only: `src/ops/wal_tail_tests.rs`, `cli/tests/f07_tail_damage_integration.rs` | pass |

Also in the repository from this step: the `lsm.before_apply` fault point (behaviour-neutral seam, registered in ADR-FAULTPOINTS-01 §12.1).

Measured after landing: `cargo test --features fault-points --test pending_fixes`: 11 passed, 0 failed, 1 ignored (`observe_f_d1a_2_*`, the F-D1a-2 observation); D1a suite concurrent column 31/31 over 6 runs; 4-writer hunts 0 lost in 300 runs. The default `cargo test --workspace` result against the D0 and D1a counts is in `docs/D0_BASELINE.md` (D1b appendix). **No performance claim is made**: §8's n = 3 cannot resolve a difference below roughly 5–10 %.

**Open after the decision:** F-D1a-2 (`snapshot_seq()`), the refuse-to-start question of ADR-WAL-SEQ-FLOOR-01 §5, F-D0-4 (D2). D1b added two class C findings, F-D1b-1 and F-D1b-2 (`docs/PERSISTENCE_MODEL.md` §7, contract gaps G14 and G6): they are not part of this approval.
