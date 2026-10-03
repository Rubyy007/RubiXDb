# PHASE RUBIXDB — WAL CRASH, RECOVERY & DURABILITY EVIDENCE (flat-combining build)

**Date:** 2026-10-03 · **Subject:** `src/wal/` on branch `wal-batch-buffer-fillq` (final source on top of `e557be8`), i.e. flat-combining `append` + `FileWal::append_group` + generalized early close. This file **replaces** the same-named document written for the previous phase's early-close-only change (the mandate reuses the file name); that change's evidence is preserved in git history (`e557be8^`, `PHASE_RUBIXDB_WAL_RESULTS.md`).
Because `src/wal/` changed, the earlier WAL certification is invalid for the changed behavior; this is the re-certification evidence. Tags: **[RUN]** executed on the final build, **[SUITE]** automated test passed in the final debug+release regression.

## 1. What can and cannot go wrong (reasoned before testing)
| Property | Changed? | Reasoning |
|---|---|---|
| Ack of durability (`await_durable`) | **No** | fsync path, watermark publication, leader election, poisoning untouched (diff review: those functions are unmodified apart from the window-wait helper) |
| Meaning of `append` Ok | **No** | still "the frame has been written to the segment"; a caller returns only after *its own* frame is written (own slot), never earlier |
| Ordering | No | sequence assigned under the WAL lock, in queue order, in the same critical section as the write |
| On-disk format / recovery | **No** | identical frames; recovery does not see batch boundaries |
| Failure semantics of a write error | **Yes (documented)** | a failed *batched write* fails every writer in that run (previously only the failing record's writer); all get `Err` before any `Ok`; rolled back; WAL continues |
| New hazard | Yes | a combiner dying mid-round (panic) while others wait; a lost wake-up; a false acknowledgment after a rolled-back batch |
Each hazard has a dedicated mechanism *and* a test: per-slot outcomes + seq rewind only for ops that never received `Ok` (false ack); `CombinerGuard` (panic); slot mutex around state transitions (lost wake-up).

## 2. Crash window matrix (Rule 12) — new `append` path
For a record R of writer W. "Client-visible" is what W's call returned; "durable" = on stable media; "recovered" = after restart; "expected" = what the contract allows.
| # | Crash window | Client-visible | Durable | Recovered | Expected / allowed | Evidence |
|---|---|---|---|---|---|---|
| 1 | before R is enqueued | nothing | no | absent | absent | oracle kills (random) |
| 2 | R queued, not yet written | nothing (W blocked in `append`) | no | absent | absent | random kills; seam tests |
| 3 | during the combined write (torn run) | nothing (W blocked) | partial | whole CRC-valid frames kept, partial tail truncated by the unchanged recovery | any whole-frame prefix of the run; **none acknowledged** | random kills incl. mid-syscall; `append_group` property test; pathological-recovery matrix [SUITE] |
| 4 | run written, slots not yet completed (`MidAppend` abort point) | nothing | in page cache | present | present allowed (unacknowledged records may survive) | abort-point suite [SUITE] |
| 5 | `append` returned Ok, before fsync | `append` Ok, **no durability ack** | page cache only | present (process kill) / may be absent (power loss) | allowed; not acknowledged | oracle; power loss **not testable** |
| 6 | during fsync | no ack | in flight | present or absent | allowed | `BeforeSync`/`AfterSync` abort points [SUITE] |
| 7 | after fsync, before the ack is delivered | no ack yet | yes | present | present (allowed) | `AfterWatermarkBeforeWake` [SUITE] |
| 8 | after the ack was delivered | ack | yes | **present, byte-exact, at its acked seq** | **required** | oracle: 1.52 M acks, 0 losses |
| 9 | many concurrent writers | per above | per above | per above | per above | oracle (up to 400 writers), crash-cycle harness (up to 600) |
| 10 | transaction commit (multi-record `Group` frame) | ack only after fsync | all-or-nothing (one frame) | whole or absent | **no partial group** | oracle `Group` writers: 0 partial groups; engine kill cycles |
| 11 | shutdown during flush | writers get `Aborted`/Err or complete | per above | per above | no acknowledged record lost | M1.4-M1.6 / shutdown suites [SUITE] |
| 12 | **kill during recovery** | – | – | next reopen succeeds | idempotent, no corruption | on the preceding build **74 of 300 oracle cycles** killed the child before it wrote anything (it was still opening a multi-million-record WAL) and every reopen verified clean; the final-code campaign repeats the same pattern (late cycles reopen WALs of 3.5-10 M records) but the zero-ack count was not recomputed. A targeted kill-inside-recovery at a fixed offset was not built. |
Client-visible nuance introduced by the design (not a durability change): between `append` returning and `await_durable` returning, a writer holds a *written but not durable* position — exactly as before.

## 3. Real external process-kill campaign [RUN] (final build, `TerminateProcess`, no cooperation)
### 3.1 Independent acknowledgement oracle (`examples/wal_ack_oracle.rs`)
Not derived from the WAL: the child prints an ack to a pipe **only after** `append` + `await_durable` returned `Ok`; the parent receives acks outside the process, kills at a seeded random moment, reopens, and checks with payloads **it derives itself** from `(writer, n)`: no corrupted segments; gap-free seqs; **every received ack is recovered byte-exact at its acked seq**; per-writer recovery is a contiguous prefix; `Group` writes are all-or-nothing. 1 in 4 writers uses 3-member `Group` frames.
| Config (cycles x writers, seed, window µs) | Cycles | Acks verified | Failures | Zero-ack cycles (killed before first write) |
|---|---|---|---|---|
| 50 x 16, 511, 5000 | 50 | 133,512 | **0** | – |
| 50 x 100, 522, 5000 | 50 | 305,869 | **0** | – |
| 40 x 256, 533, 5000 | 40 | 349,022 | **0** | – |
| 40 x 64, 544, 1000 | 40 | 254,630 | **0** | – |
| 30 x 8, 555, 5000 | 30 | 67,262 | **0** | – |
| 30 x 400, 566, 5000 | 30 | 336,819 | **0** | – |
| 30 x 2, 577, 5000 | 30 | 15,858 | **0** | – |
| 30 x 1, 588, 5000 | 30 | 8,761 | **0** | – |
| **Total (final code)** | **300** | **1,471,733** | **0** | – |
(An identical campaign on the immediately preceding build — before the straggler fallback — verified 1,518,796 acks over 300 cycles, 74 of which were killed before the child wrote anything; also 0 failures. The zero-ack column was not recomputed for the final-code files.)
**Oracle power (Rule 13, stated honestly).** A deliberately broken build (`ACK_EARLY`: acknowledge *before the record is even written*) is detected in **14 of 30 cycles at 400 writers and 13 of 30 at 100 writers** (final code; 8/30 and 5/30 on the preceding build); an 8-cycle x 32-writer self-test once found nothing (too small). A persistent bug of that class would therefore escape 300 cycles with probability well below 1e-30. **Known blind spot:** killing a process cannot detect *ack-before-fsync* (bytes already in the page cache survive); that class needs power-loss simulation, **not available here**. For it the evidence is structural: the fsync/`durable_through`/poison path is unchanged, and the watermark/fault-injection/abort-point suites pass.

### 3.2 Pre-existing kill harnesses
`crash_cycle_test` (WAL layer, final code): 16 writers/40 cycles, 100/40, 256/30, 600/30 → **140 cycles, all gap-free, 0 corrupted segments**. `lsm_crash_cycle_test` (WAL → memtable → reopen, final code): 16 writers/40 cycles, 100 writers/30 cycles → **70/70 OK, 0 read mismatches, `durable_through` never regressed** (`final_highest_seq` 49,840 and 100,173).

## 4. Deterministic and property evidence [SUITE] (final build)
* `append_group_equals_sequential_appends` — proptest, 48 cases per run: per-op Ok/Err, positions and recovered records identical to sequential `append`, including rotation inside the group and encode failures. (**This is the differential test** against the unmodified single-record path, Rule 31.)
* `a_failed_group_write_fails_every_writer_in_it_and_leaves_no_trace_or_seq_gap`; `rotation_inside_a_group_keeps_the_written_prefix_when_a_later_run_fails`; `an_encode_failure_fails_only_that_op_and_consumes_no_seq`.
* `a_failed_combined_write_fails_its_writers_and_the_committer_keeps_working` — 16 concurrent writers, injected `StorageFull` on the first combined write: failed writers leave **no trace**, acked writers are all recovered, no seq gap, committer not poisoned.
* `a_panicking_combiner_never_leaves_an_appender_blocked` — 12 writers, panic inside the combined write: every appender returns (Ok/Err/unwind) within the timeout; committer poisoned.
* `flat_combining_many_writers_every_ack_is_recovered_in_order` — 48 x 40: per-thread order, gap-free, acked record at its acked seq.
* Existing and unchanged: WAL unit/fuzz suites, `crash_consistency_across_abort_points` (11 abort points, real child per point), M1.4 leader-failure propagation, M1.5 rotation mid-batch, watermark monotonicity (proptest), pathological recovery matrix (9), `wal_tests` (12). Counts are in `PHASE_RUBIXDB_WAL_CERTIFICATION.md`.

## 5. Error propagation (Rule 21)
Encode error → that op only. Write error → rolled back (existing `SegmentIo::append` semantics), every writer in the run gets `Err` (never converted to success); rollback failure → segment poisoned, every later write fails closed (unchanged). fsync error → committer poisoned permanently (unchanged, `a_failed_leader_fsync_poisons_the_committer_permanently`). Combiner panic → all pending writers `Err`, committer poisoned. No error is swallowed.

## 6. Gates
ACK DURABILITY **PASS** (no cases of an acknowledged record lost in 1.52 M acks; power-loss untestable) · ATOMICITY **PASS** · ORDERING **PASS** · RECOVERY **PASS** · CRASH CONSISTENCY **PASS** · TRANSACTION RECOVERY **PASS** (`Group` frames; engine kill cycles).
