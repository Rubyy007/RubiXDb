# ADR-CHECKPOINT-CONTIGUITY-01 — The manifest checkpoint must mean "every seq at or below it is in a table"

| | |
|---|---|
| Status | **ACCEPTED and IMPLEMENTED 2026-10-13** — maintainer approval 2026-10-13 ("Approve all recommended fixes, then proceed to D1b"): option (ii), the contiguous-applied watermark captured at freeze time. The proposal text below is kept as written; the implementation record is the last section. |
| Date | 2026-10-13 |
| Finding | **F-D1a-1** (docs/D0_BASELINE.md, D1a appendix C; `OPEN_ITEMS.md`; contract gap G13) |
| Touches (if fix) | `src/lsm/mod.rs` (+ one line in `src/lsm/tests.rs` that builds an `LsmEngine` literal). **No** change in `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`. |
| Certification note | `src/lsm/mod.rs` is not in `docs/PROJECT_STATE.md`'s protected list, but it is the code of the certified Write Engine / Read Engine (PROJECT_STATE "Certifications on record"). This finding contradicts the multi-writer reading of those certifications; the fix needs their evidence re-run. |
| Prototype | `docs/adr/prototypes/D1a-gate-fixes.diff` (all five gate fixes, one diff; **only** the `src/lsm` hunks belong to this ADR). Built and measured in a scratch copy outside the repository. |
| Tests | `tests/pending_fixes.rs` (`--features fault-points`): `f_d1a_1_put_…`, `f_d1a_1_batch_…` (red today, green on the prototype), `guard_f_d1a_1_…` (green on both). |

Evidence tags: `[R]` read in source, `[M]` measured here, `[A]` analysis (not measured), `[P]` measured on the prototype only.

---

## 1. Context — what is wrong, with evidence

`LsmEngine::put` / `delete` / `write_batch` return `Ok` after two steps that are **not ordered with respect to other writers** `[R]` `src/lsm/mod.rs:1892-2000`:

1. the WAL group commit makes the frame durable and the caller is released (`completion.wait()`), seq `s` assigned in append order;
2. later, the caller takes the MemTable write lock, inserts its entry at seq `s` and, if the MemTable is now full, freezes it.

Callers released by the same batch (or by neighbouring batches) take the lock in arbitrary order. If caller B (seq `s+1`) fills and freezes the MemTable before caller A (seq `s`) has inserted, then:

* the frozen table's `max_seq` is `s+1`; the flush thread records `SET_CHECKPOINT{flushed_through_seq = max_seq}` (`lsm/mod.rs`, flush thread) and `purge_before(max_seq)` may delete the WAL segment holding `s`;
* A then inserts into the **new** MemTable and its call returns `Ok(s)` — **acknowledged**;
* recovery skips every WAL record with `seq <= checkpoint` (`lsm/mod.rs:1563`) `[R]`, so A is in no table and is skipped from the WAL: **lost**, after a crash and also after a **graceful** stop and reopen.

Deterministic reproduction `[M]` (no sleeps decide the order; the test waits for the execution log of the `lsm.before_apply` seam to show A parked): A = seq 1, B = seq 2, both acknowledged, manifest checkpoint 2, A's key absent from the crash image **and** after a clean reopen. `tests/pending_fixes.rs` `f_d1a_1_put_…` and `f_d1a_1_batch_…`: **both FAIL on the current tree** (`left: None`, message names seq and checkpoint).

Statistical `[M]` (4 writers × 100 ops, 8 KiB MemTable, no fault rule; `tests/fault_points/inspect.rs` `hunt_lost_ack_without_faults`, 30 runs per cell):

| tree | abort after settling, compaction off | abort, compaction on | graceful stop, compaction off |
|---|---|---|---|
| current | **7 / 30** runs lost data | **7 / 30** | **10 / 30** |
| prototype of option (ii) | 0 / 30 | 0 / 30 | 0 / 30 |

Pristine `git archive dde8ae9` (no registry code at all), graceful stop + reopen, 4 writers: 5 of 15 runs; 1 and 2 writers: 0 of 15 (D1a appendix C). Not measured: the rate at the default 4 MiB MemTable, Unix.

## 2. (a) The invariant — restated

**What the contract says today** (`docs/DURABILITY_CONTRACT.md` §1, "Checkpoint"; `docs/PERSISTENCE_MAP.md` §3.3, §5): *`SET_CHECKPOINT{flushed_through_seq}` — every commit with `seq <=` it is contained in a live SSTable.* Recovery relies on exactly this (it skips `seq <= checkpoint`), and so does `purge_before`.

**Why that sentence is false under concurrency, by construction:** the engine sets `flushed_through_seq := max seq in the table`. A table contains only the entries *that had been applied when it was frozen*; "every lower seq" is true only if apply order equals seq order, and nothing enforces that (two callers released together race for a lock). The sentence describes the single-writer behaviour, where it holds.

**Replacement invariant (all options below are judged against it):**

> **I-CKPT-1.** A checkpoint value `c` may be recorded for a table `T` (and the WAL purged up to `c`) only if every commit unit with `seq <= c` — acknowledged or not — had been applied to `T`'s MemTable, or to a MemTable frozen and published no later than `T`, **before `T` was frozen**; or needs no apply (a checkpoint marker).
> **I-CKPT-2.** Recorded checkpoints are non-decreasing (already enforced by `ManifestState::apply`).
> **I-CKPT-3.** `purge_before` is called only with a value that satisfies I-CKPT-1.
> **I-CKPT-4.** Replaying a WAL record whose entry is already in a table is idempotent: same key, same seq, same value, no visible effect. (Restates R5 "no double application" as "no visible effect"; today it is moot because the checkpoint filter prevents the replay.)

## 3. Options

Invariants refer to `docs/DURABILITY_CONTRACT.md` §8 (R1–R13). "Weakens" means a guarantee that holds today is conditioned.

| | (i) apply in seq order | **(ii) checkpoint = contiguous-applied watermark captured at freeze** | (iii) recovery replays `seq > min(unflushed)` | (iv) hold the freeze until lower durable seqs have applied |
|---|---|---|---|---|
| Idea | writers wait their turn to apply (ticket by seq) | a small tracker records which seqs are applied; at freeze (under the write lock) read `contiguous()`; that, not `max_seq`, is the checkpoint, marker and purge value | store and replay from a lower bound instead of the max | skip/defer `freeze_locked` while an unapplied lower seq exists |
| What must be tracked | the next seq to apply, plus *every seq that will never be applied* (markers, failed completions) | the same information, but nobody waits on it | the lowest unflushed seq — i.e. the same tracker, persisted | the same tracker, plus a deferral policy |
| R1–R13 | all preserved; also removes the read anomaly of §6 | all preserved; R5 restated as I-CKPT-4 | all preserved; changes the on-disk meaning of `SET_CHECKPOINT` (D2 interaction) | all preserved |
| Weakens | commit latency becomes head-of-line: one stuck writer blocks every later writer's return `[A]`; a seq that is never applied hangs all writers (**deadlock risk**) | none acknowledged; checkpoint may lag the table's `max_seq`, so recovery re-applies up to one in-flight window of records already in tables (I-CKPT-4) | recovery cost grows with the unflushed window | `memtable_max_size_bytes` becomes a soft bound; if the awaited writer never applies, the MemTable grows without limit (**memory risk**) |
| Latency / throughput | worst `[A]` | none measurable `[P]` (below) | none on the write path | none on the write path, backpressure changes |
| Memory | none | one `BTreeSet<u64>` of seqs applied above the watermark: normally ≤ number of in-flight writers; **unbounded if a gap never fills** (§5) | none | unbounded MemTable growth (above) |
| Space | none | WAL retained until the watermark passes: longer by the in-flight window; unbounded if a gap never fills | WAL retained longer | none |
| Scope | `src/lsm/mod.rs` (apply path) | `src/lsm/mod.rs` (+1 test literal) | `src/lsm`, **and** `src/manifest` semantics + `src/wal` purge | `src/lsm/mod.rs` |
| Verdict | rejected: deadlock/latency risk outweighs; same accounting problem as (ii) with a worse failure mode | **recommended** | collapses into (ii): to pick the lower bound you need the tracker, and `purge_before` needs it too; it only adds an on-disk meaning change | rejected: same tracker as (ii) plus an unbounded-memory failure mode |

## 4. Recommendation: option (ii)

Prototype, `[P]`, in `docs/adr/prototypes/D1a-gate-fixes.diff` (the `src/lsm/mod.rs` hunks), line by line:

| Hunk (`src/lsm/mod.rs`) | Lines | Why |
|---|---|---|
| `use std::collections::{…, BTreeSet, …}` | 1 | set of seqs applied above the watermark |
| `struct ApplyTracker`, `TrackerInner`, `new`, `account`, `contiguous` | ~40 | the accounting: `account(seq)` inserts and advances the watermark through consecutive seqs |
| `FlushMsg::Flush(Arc<MemTable>)` → `Flush(Arc<MemTable>, u64)` | 1 + 2 sites | carry the watermark read **under the write lock at the instant of the freeze** to the flush thread |
| field `apply_tracker: Arc<ApplyTracker>` in `LsmEngine`; initialised in `open` from `max(last replayed seq, checkpoint)` (the `_summary` of the WAL replay is now used) | 6 | after recovery everything the WAL still holds is accounted for |
| `spawn_flush_thread(…, apply_tracker, …)` | 2 | the flush thread must account the checkpoint-marker seq |
| `apply_after_durable`: `account(seq)` right after the insert; `apply_batch_after_durable`: once after the member loop | 2 | the tracker sees a seq exactly when its entry is in the MemTable, **before** the freeze check in the same critical section |
| `freeze_locked`: send `self.apply_tracker.contiguous()` | 1 | I-CKPT-1: read under the write lock |
| flush thread: `flushed_through_seq` of the WAL marker, the `SET_CHECKPOINT`, `checkpoint_seq.store`, `purge_before` take the freeze-time watermark instead of `meta.max_seq` | 4 | I-CKPT-1, I-CKPT-3 |
| flush thread: `apply_tracker.account(position.seq)` after the marker's `wait()` | 1 | a marker consumes a seq no writer applies; without this the watermark could never pass it |
| `src/lsm/tests.rs`: `apply_tracker: Arc::new(super::ApplyTracker::new(0))` in the one raw `LsmEngine { … }` literal | 1 | construction only; no assertion changed |

Total: 70 added lines in `src/lsm/mod.rs`, 1 in `src/lsm/tests.rs`. `src/wal`, `src/manifest`, `src/sstable`, `src/compaction`, `src/error.rs` unchanged. Recovery code is unchanged (it still skips `seq <= checkpoint`; the checkpoint is simply never too high any more).

Measured on the prototype `[P]`: the two deterministic tests pass; the three 30-run hunts: 0/90 (current tree: 24/90); the whole `fault_points` suite passes (class A 34/34, class B single 164/164, concurrent 31/31, relational 31/31; the current tree has 2 FAIL in the concurrent column); the whole workspace on the prototype (55 binaries) differs from the clean run only in the two F-D0-1 tests of ADR-TORN-TAIL-ZERO-01 (and a `repo_hygiene` artefact of the git-less scratch copy); with that one hunk reverted both pass, so **no existing test is affected by this option** apart from the one raw `LsmEngine` literal.

Cost `[P]`, write path, product config (GroupCommit 5 ms / 256 KiB), 100-byte values, 8 s per cell, **3 alternating repetitions**, this machine (fsync-bound SATA SSD; raw numbers in `docs/D1a_DECISION_GATE.md`):

| writers / MemTable | ops/s before (mean, range) | ops/s after (mean, range) |
|---|---|---|
| 1 / 4 MiB | 283 (257–314) | 293 (284–304) |
| 4 / 4 MiB | 570 (550–592) | 572 (566–581) |
| 8 / 4 MiB | 1126 (1074–1161) | 1071 (922–1205) |
| 4 / 64 KiB | 533 (516–543) | 515 (504–535) |

Every difference is inside the run-to-run spread of one side; p50 latency is unchanged (single writer 2.8–3.2 ms, 4 writers 6.4–7.2 ms); p99/p999/max are noisy in both columns. **No cost is distinguishable at n = 3; a regression below roughly 5–10 % could not be seen.** The added work (one mutex lock and a set insert per commit, inside the write lock that is already held) is not isolated by this measurement; it is small against a multi-millisecond fsync, but that is `[A]`.

## 5. Residual risks of the recommended fix (stated, not hidden)

1. **A durable seq that no caller ever applies stalls the watermark.** The one realistic path: `completion.wait()` returns `Timeout` after `await_retry_budget` (5 s) — the frame was appended and may become durable later, the caller got `Err` (outcome unknown, allowed by C-ACK-2) and never applies. The watermark then stops at that seq: no checkpoint progress, **no WAL purge (disk grows), and the tracker's set grows by one entry per later commit** until restart (where the record is replayed — consistent with "outcome-unknown may be present"). Today the same event is silent. Decision needed on a bound: a cap on the set with a loud log and a metric, versus fail-stop. (An fsync *failure* poisons the committer permanently, so no stall issue arises there.)
2. **Duplicates after recovery.** Records between the watermark and the table's real `max_seq` are replayed into the MemTable although already in a table; the next flush writes them again. Equal `(key, seq, value)` across layers is benign for point reads and scans (one version per key is returned) — exercised by the prototype's compaction-on hunts and matrix, **not proven** for every consumer (compaction with a live snapshot, `ops::check`, backup). I-CKPT-4 makes this a stated obligation.
3. **A second, related finding that this fix does not fix: F-D1a-2.** `snapshot_seq()` is the *durable* watermark (`lsm/mod.rs` `snapshot_seq`), so a snapshot taken between A's acknowledgement and A's apply has `seq >= A` yet cannot see A — `scan` at that snapshot omits a durable commit. The backup's "consistent at `snapshot_seq`" statement (docs/PERSISTENCE_MAP.md §7) depends on this. The tracker makes a correct boundary available (`contiguous()`); **D3 must use it or an equivalent**. Not fixed here; recorded in `OPEN_ITEMS.md`. (Observation test: `observe_f_d1a_2_snapshot_seq_can_exceed_the_applied_prefix`, `--ignored`.)
4. The raw `LsmEngine` literal in `lsm/tests.rs` is the only existing test that must change (construction only).

## 6. Accept / exclude — consequences, in one sentence each

* **Accept:** the contract would have to say *"under concurrent writers an acknowledged commit unit may be lost across any restart, graceful or not, when a MemTable freeze overtakes its apply"* — that removes "acknowledged commits are recovered" (R1) as a claim for any multi-writer deployment, which includes the product's HTTP server and every SQL transaction.
* **Exclude** (declare unreachable): not supportable — it is reproduced deterministically with a test seam, 24/90 times without one, on a pristine export of the certified commit.

## 7. (b) Contract clause (PROPOSED; drafted in `docs/DURABILITY_CONTRACT.md` §12)

**C-CKPT-1.** *A manifest checkpoint `c` is recorded, and the WAL purged up to `c`, only when every commit unit with `seq <= c` (acknowledged or not) is contained in the table being published or in a table published earlier, or is a checkpoint marker. Consequently every acknowledged commit unit survives any restart — graceful stop or crash of the process — regardless of the order in which concurrent callers applied their entries.* **Falsified by:** `tests/pending_fixes.rs` `f_d1a_1_*` (deterministic) and by any row of the D1a concurrent column carrying the F-D1a-1 signature (acknowledged seq ≤ checkpoint, in no live table).

## 8. (c) Regression test

`tests/pending_fixes.rs` (`required-features = ["fault-points"]`):

* `f_d1a_1_put_acknowledged_before_a_freeze_that_overtook_its_apply_survives` and `…_batch_…`. Writer A is parked by `lsm.before_apply=nth=1:delay=3000` after its WAL acknowledgement; the test waits for the **execution log** to show the seam fired (not for time), runs writer B with a value larger than the 4 KiB MemTable, waits until the frozen table is published, joins A (`Ok`, acknowledged), then (1) copies the live directory as a crash image and recovers it and (2) stops the engine gracefully and reopens it; A's key must be present in both.
* Why it is deterministic: the order A-durable → B-applies-and-freezes → A-applies is forced by the seam and by waiting on observable state; the only time constants are timeouts that fail loudly, not races that pick the outcome.
* **Fails on the current code** `[M]` (7 of 11 tests in the file fail; this one with `left: None`, "manifest checkpoint = 2 (>= 1 …)"); **passes on the prototype** `[P]` (11 of 11).
* Guards that must stay green: `guard_f_d1a_1_ordinary_sequential_writes_survive_restart`.

## 9. (d) Scope

`src/lsm/mod.rs` only (+ the construction line in `lsm/tests.rs`). Not in the protected list; certified Write/Read Engine code. The D1a seam `lsm.before_apply` (2 statements in `src/lsm/mod.rs`, behaviour-neutral, already in the tree) is test infrastructure for this ADR and is registered in ADR-FAULTPOINTS-01 §12.1.

## 10. (e) Placement

**Before D1b.** Class-C expected states and the concurrent column (A4: N = 4, M = 5) are only meaningful on top of an engine whose acknowledgement is recoverable; D3's consistency boundary depends on the watermark. If the maintainer chooses **accept**, D1b may proceed only with the contract text of §6 in force and every concurrent expected state defined as "acknowledged unless a freeze overtook its apply".

## 11. Verification plan once authorised

Apply the `src/lsm` hunks; run `tests/pending_fixes.rs` (11/11), the D1a suite (concurrent column must be 31/31 over **M = 5** repetitions), the three hunks above at ≥ 100 runs per cell, `cargo test --workspace --no-fail-fast` (expect the D0 baseline), the write-path benchmark at ≥ 10 repetitions, and a soak with compaction on and a live snapshot (duplicates, risk 2). Re-run the Write Engine certification evidence.

---

## 12. Implementation record (2026-10-13)

Landed in `src/lsm/mod.rs` (one line in `src/lsm/tests.rs` for the raw `LsmEngine` literal). Differences from the prototype are none in behaviour; the tracker is `ApplyTracker { contiguous, above: BTreeSet }`:

* `FlushMsg::Flush(Arc<MemTable>, u64)` carries the contiguous watermark read **under the write lock at freeze**; the flush thread uses it for the manifest checkpoint, the WAL checkpoint marker and `purge_before` (never the table's own `max_seq`).
* every apply calls `account(seq)`; a checkpoint-marker record's position is accounted too (`account(position.seq)`), so markers never stall the watermark.
* the tracker is initialised at `open` from `max(last replayed WAL seq, checkpoint)`.
* a durable seq that is never applied (an `await_retry_budget` timeout) stalls the watermark: the WAL is then not purged and the set grows — the residual risk named in §6, unchanged.

Evidence on the repository after landing: `tests/pending_fixes.rs` `f_d1a_1_put_*`, `f_d1a_1_batch_*` and the guard pass (10 of 10 invocations; 0 of 10 before); the 4-writer hunts found 0 lost acknowledged commits in 300 runs (24 of 90 before); the D1a concurrent column is 31/31 over 6 runs (28/31 before). The write-path cost is not distinguishable from run-to-run spread at n = 3 per cell (docs/D1a_DECISION_GATE.md §8). **F-D1a-2 (`snapshot_seq()`) is NOT fixed by this ADR** and stays open for D3.
