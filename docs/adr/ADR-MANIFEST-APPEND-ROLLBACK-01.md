# ADR-MANIFEST-APPEND-ROLLBACK-01 — A failed MANIFEST append is rolled back

| | |
|---|---|
| Status | **ACCEPTED and IMPLEMENTED 2026-10-13** — maintainer approval 2026-10-13 ("Approve all recommended fixes, then proceed to D1b"): a failed MANIFEST append is rolled back (truncate to the pre-append length and sync) or the manifest is poisoned. The proposal text below is kept as written; the implementation record is the last section. |
| Date | 2026-10-13 |
| Finding | **F-D0-3** — docs/PERSISTENCE_MAP.md §3.6, §9 |
| Touches (if fix) | `src/manifest/mod.rs` (+30 lines) — **protected** |
| Prototype | `docs/adr/prototypes/D1a-gate-fixes.diff` (hunks `src/manifest/mod.rs`) |
| Test | `tests/pending_fixes.rs::f_d0_3_failed_short_manifest_write_is_rolled_back_and_the_retry_leaves_a_readable_file` (red today / green on the prototype) |

## 1. Context and evidence

`Manifest::append_sync` is `write_all(frame)` then `sync_all()` and has **no rollback** of a failed or short write, unlike `SegmentIo::append` (`wal/file_io.rs:91-117`: truncate to the pre-append length, `sync_all`, poison on failure) `[R]`. The flush thread retries a failed flush by appending again (`lsm/mod.rs`, bounded retries then every 2 s), so a retry after a *partial* write appends the full frame **after** the stray prefix.

Observed at the **real call site** `[M]` (D1a, class A, `manifest.write=nth=3:shortwrite=7`; `target/fault-points-results/observation_f_d0_3.txt`, and now `tests/pending_fixes.rs`): the write returned `WriteZero` after 7 bytes; the retry succeeded; the MANIFEST grew to 826 bytes and the product start sequence refused: `corruption: manifest: checksum mismatch at offset 74 (not the trailing frame -- real corruption, not a torn write); refusing to open. The directory has not been modified.` Fail-closed — but a *transient* error turned a recoverable database into one that needs repair. In the regression test the stray bytes are visible directly: after the failed append the file is 48 bytes, not 41.

**Not shown:** that an OS `write` can return `Err` after a partial write on this machine (ENOSPC mid-write is the plausible route; real disk-full is NOT TESTED). The consequence is measured; the premise is not.

## 2. Options

* **Fix** — on a failed `write_all`, `set_len(len_before)`, seek back, `sync_all`; if that rollback fails, poison the manifest (no further appends). Track `len` (the valid prefix; set from `valid_length` at open). *Recommended.*
* **Accept** — document "a failed MANIFEST append may leave the directory needing manual repair".
* **Exclude** — only if the premise is ruled out by Tier 3/4 evidence; not available.

The failed-`sync_all` path is **left as is** in this ADR (a frame fully written but not known synced; a retry appends a duplicate, which `ManifestState::apply` accepts for ADD/REMOVE and an equal `SET_CHECKPOINT`). Whether an fsync failure should poison the manifest, as it poisons the WAL committer permanently, is a separate decision (A6 in the contract) and is listed, not made.

## 3. Prototype `[P]`

30 added lines: fields `len`, `poisoned`; rollback in the error arm; `len += frame.len()` after success. Regression test green; the rest of the `fault_points` suite green (class A 34/34 includes `manifest.write` and `manifest.sync` error rows); engine unit tests unchanged (686/687, the one failure is ADR-TORN-TAIL-ZERO-01's).

## 4. Contract text (PROPOSED; `docs/DURABILITY_CONTRACT.md` §12)

**C-MANIFEST-1.** *A failed MANIFEST append leaves the file exactly as it was before the call, or the manifest is poisoned and accepts no further append; in both cases a later open succeeds or fails only for a reason that existed before the call.* Falsified by `f_d0_3_*`.

**If accepted:** *"a transient write error during a MANIFEST append can leave an unopenable directory; recovery requires an operator."*

## 5. Scope and placement

`src/manifest/mod.rs` only; no format change; callers (`lsm/mod.rs` flush/compaction/reconcile) unchanged. Bundle with the D2 manifest ADR (same file) or land earlier — it is independent of D1b's infrastructure but should precede D1c (class-C torn manifest writes need a defined expected state).

---

## 6. Implementation record (2026-10-13)

Landed in `src/manifest/mod.rs`: `Manifest` gains `len` (the length before the append) and `poisoned`; on a failed `write_all` or `sync_all`, the file is truncated back (`set_len`, seek, `sync_all`); if the rollback itself fails the manifest is poisoned and refuses further appends. Test `f_d0_3_failed_short_manifest_write_is_rolled_back_and_the_retry_leaves_a_readable_file` passes. The premise (an OS write error after a partial write) is still not shown reachable on this machine; the fix is justified by symmetry with `SegmentIo::append` and by the injected class A test.
