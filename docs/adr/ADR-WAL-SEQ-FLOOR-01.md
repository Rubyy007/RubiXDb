# ADR-WAL-SEQ-FLOOR-01 — Sequence numbers never restart at or below the checkpoint

| | |
|---|---|
| Status | **ACCEPTED and IMPLEMENTED 2026-10-13** — maintainer approval 2026-10-13 ("Approve all recommended fixes, then proceed to D1b"): a floor: the next sequence number is at least checkpoint + 1. The proposal text below is kept as written; the implementation record is the last section. |
| Date | 2026-10-13 |
| Finding | **F-D0-5** — docs/PERSISTENCE_MAP.md §5, §9; invariant **R6** |
| Touches (if fix) | `src/wal/mod.rs` (+11 lines: one `pub fn` on `FileWal`) — **protected**; `src/lsm/mod.rs` (+7 lines in `open`) |
| Prototype | `docs/adr/prototypes/D1a-gate-fixes.diff` |
| Test | `tests/pending_fixes.rs::f_d0_5_seq_after_recovery_is_above_the_checkpoint_even_when_the_wal_content_is_missing` (red today / green on the prototype) |

## 1. Context and evidence

`FileWal::open_for_recovery` derives `next_seq` **only from the records still in the WAL** (`wal/mod.rs:958`: `last record seq + 1`, or 1 for an empty log) `[R]`, never from the manifest checkpoint, while recovery skips every record with `seq <= checkpoint` (`lsm/mod.rs:1563`). If the WAL content is missing, the first new commits are numbered from 1 — below the checkpoint — acknowledged, and silently skipped as "already flushed" at the next restart.

Measured `[M]` (re-run as the regression test): after 200 puts and a clean stop the checkpoint is 185; with every `wal/*.log` deleted the engine opens silently and the first new `put` is **acknowledged at seq 1** (test message: "numbered 1, not above the checkpoint 185"); after a clean reopen the key is gone.

**Reachability of the premise is NOT shown.** A legitimate log always ends above the checkpoint (the checkpoint marker is written to the segment after the one flushed, and the active segment is never purged). The log can end below it only if WAL content is lost: a lost segment directory entry (Windows `fsync_dir` is a no-op — assumption A5), a damaged log removed by an operator, or media loss. The product's attestation guard (`ops/wal_tail.rs`, `missing_records`) covers the *cleanly stopped* case only; after a crash there is no attestation. `OPEN_ITEMS.md` (2026-10-11, F-11 closure) already records "a log ending below the manifest checkpoint is not refused".

## 2. Options

* **Fix (floor)** — at open, `file_wal.ensure_next_seq_at_least(checkpoint + 1)`; log a loud warning when it had to raise. New commits are then always above the checkpoint. *Recommended minimum.* It does not recover the missing unflushed records (nothing can), but stops it from also losing everything written afterwards.
* **Fix + refuse** — additionally refuse to open when the log ends below the checkpoint (fail-closed, operator decides). Strictly safer for the data that is *missing*, but turns a recoverable-going-forward state into a refusal, including for operators who deliberately removed the log. Decision needed; not in the prototype.
* **Accept** — document that "after loss of WAL content the engine may reuse sequence numbers and silently lose later commits".
* **Exclude** — only with evidence that WAL content cannot go missing; none exists.

## 3. Prototype `[P]`

`FileWal::ensure_next_seq_at_least(floor) -> bool` (raises `next_seq` and `durable_seq`), called once in `LsmEngine::open` between `FileWal::open_for_recovery` and `GroupCommitter::new` with the checkpoint read before the replay; prints a warning naming the checkpoint when it raises. Test green; full `fault_points` suite green; no existing engine test affected.

## 4. Contract text (PROPOSED; `docs/DURABILITY_CONTRACT.md` §12)

**C-SEQ-1** (= R6 as a clause). *After recovery the next assigned sequence number exceeds the manifest checkpoint, every sequence number in any live table, and every sequence number in the WAL.* Falsified by `f_d0_5_*` and by the oracle's R6 check in every recovery.

**If accepted:** *"if WAL content is missing, sequence numbers may restart below the checkpoint and later acknowledged commits are lost at the next restart."*

## 5. Interaction with ADR-CHECKPOINT-CONTIGUITY-01

The contiguity tracker is initialised from `max(last replayed WAL seq, checkpoint)`. With this floor the WAL, the committer and the tracker agree on the numbering after recovery. Without this ADR, the tracker would be initialised above the WAL's next seq in the missing-WAL case, so the two ADRs should be decided together.

## 6. Placement

Before D1c: the oracle's R6 check is meant to be a gate there, and class C will make "WAL content missing" (lost directory entry) a first-class scenario.

---

## 6. Implementation record (2026-10-13)

Landed: `FileWal::ensure_next_seq_at_least(floor)` in `src/wal/mod.rs`, called from `LsmEngine::open` with `checkpoint + 1`; it logs a warning when it raises the counter. Whether to also *refuse* to start when WAL content is missing below the checkpoint is **not decided here** (the ADR's §5 question; kept open). Test `f_d0_5_seq_after_recovery_is_above_the_checkpoint_even_when_the_wal_content_is_missing` passes. The premise (WAL content going missing) is still not shown reachable.
