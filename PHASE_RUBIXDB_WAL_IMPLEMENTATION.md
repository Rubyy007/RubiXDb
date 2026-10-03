# PHASE RUBIXDB — WAL IMPLEMENTATION (flat-combining group commit)

**Date:** 2026-10-03 · **Branch:** `wal-batch-buffer-fillq` (not merged to `master`) · **Source:** committed on the branch by the user as `e557be8` ("night commit"); working tree identical to it at the time of writing.
**Scope check:** `git diff master --stat -- src/` = only `src/wal/` (`group_commit.rs`, `mod.rs`, `ops.rs`, new `group_append_tests.rs`): **+1,146 / -13 lines including ~470 lines of tests**. `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/lsm/`, `src/execution/`, SQL/API/CLI/GUI: **zero diff**. Rationale and alternatives: `PHASE_RUBIXDB_WAL_ARCHITECTURE_OPTIONS.md`.

## 1. What changed (production vs experimental, Rule 26)
| Piece | File | Class | Notes |
|---|---|---|---|
| `WalOp::to_owned_op` | `ops.rs` | PRODUCTION | borrowed → owned copy so a combiner can encode other threads' ops |
| `FileWal::append_group`, `flush_run`, `group_write_error` | `mod.rs` | PRODUCTION | batched write, one syscall per same-segment run |
| flat-combining `GroupCommitter::append`, `AppendSlot`, `AppendQueue`, `CombinerGuard`, `combine_until_done` | `group_commit.rs` | PRODUCTION | |
| count-aware, quiescence-guarded early close; probe restricted to lone-writer regime; `appended_seq`, `prev_batch_records` | `group_commit.rs` | PRODUCTION | retained from the previous phase and generalized |
| `fail_group_write_on_nth`, `panic_on_next_group_write` fields | `mod.rs` | TEST-ONLY (`#[cfg(test)]`) | fault seams |
| close-reason counters (`closes[zero,bytes,deadline,probe,cohort]`) | `group_commit.rs` `batch_timing` | TEST-UTIL only | compiled out of production |
| `quiesce_experiment` (env overrides of the early-close constants) | `group_commit.rs` | EXPERIMENT (`phase1-window-experiment`, off by default, removable) | used to choose constants |
| `examples/commit_pipeline_proto.rs`, `fsync_*_probe.rs`, `windows_io_modes_probe.rs`, `wal_commit_latency.rs`, `wal_soak.rs`, `wal_ack_oracle.rs` | `examples/` | ANALYSIS/CERTIFICATION TOOLS | not on any production path |
No benchmark-only shortcut, test-only durability behavior, or hardware constant is in production code beyond the documented, measured early-close constants (§4).

## 2. Design
### 2.1 `FileWal::append_group(ops: Vec<WalOp>) -> Vec<Result<WalPosition>>`
Semantically N sequential `append` calls, with **one write syscall per run of frames landing in the same segment**.
* Frames are encoded with the unchanged `encode_wal_frame(seq, op, max_record_len)`; an op whose encode fails fails **alone** and consumes no seq.
* Rotation follows `append`'s rule exactly: a segment that already has content *including frames pending in the current run* is rotated before a frame that would exceed `max_segment_size` (`flush_run`, then `rotate()`).
* A run is written with `SegmentIo::append(&run)` — the existing single-`write_all_at` with **rollback-on-failure** (truncate to the pre-run length; poison if the rollback fails). On failure every op of the run and every later op of the call gets `Err`; `next_seq` is left at the run's first seq.
* Earlier, already written runs keep their `Ok(position)` ("prefix succeeded, rest failed").
* `MidAppend` abort hook fires once per successfully written record (after the run is written), preserving the hook's per-record count.

### 2.2 `GroupCommitter::append` — flat combining
```
append(op):  owned = op.to_owned_op(); slot = new AppendSlot
             lock queue; push (slot, owned); leads = !queue.combining; queue.combining = true
             if !leads { match slot.wait() { Done(r) => return r, Combine => {} } }
             combine_until_done(slot)
combine_until_done(own):
   while !own.is_done():
       batch = take up to 4096 from queue front
       results = lock WAL; wal.append_group(batch ops); unlock
       for (slot, result): slot.complete(result)      # Ok(position) or Err
       update batch_bytes, appended_seq
   lock queue; if head exists { head.promote() } else { combining = false }     # hand-off or release
   return own result
```
* **Contract preserved:** an appender returns only after *its own frame has been written* (or has failed), so `Ok` still means "written to the segment" and the failure of a batched write reaches every writer in it **before any of them holds `Ok`**.
* **Durability unchanged:** `await_durable`, the leader election, the fsync with no lock held, `durable_through` publication and poisoning are untouched.
* **Per-slot outcomes:** each appender waits on its *own* slot (mutex + condvar, plus a lock-free flag for a bounded spin of 256 iterations then a blocking wait — never an unbounded busy-wait). No shared watermark is consulted for append success.
* **Panic safety:** `CombinerGuard` (RAII). On unwind it completes every taken and every still-queued slot with `Err`, releases the combiner role, and sets `poisoned = LeaderPanicked` (outcome of an interrupted write is unknowable ⇒ fail closed).
* **Bounds:** queue length ≤ number of caller threads (each blocks until written); per-round work ≤ `APPEND_COMBINE_MAX_OPS = 4096` (a safety bound, not a tuning knob); transient run buffer ≤ 4096 x `max_record_len`; no new thread, channel, or unbounded buffer.

### 2.3 Early close (generalized) and the probe fix
`spin_wait_for_batch_window(batch_base_seq)`: in addition to the existing exits (byte cap, deadline, probe) the leader exits when **(a)** the open batch (`appended_seq - batch_base_seq`) ≥ the previous batch's size **and** no record was appended for `q = clamp(400 ns x cohort, 100 µs, 1 ms)`, **or (b, straggler fallback)** at least one record is open and arrivals have been quiet for `min(4 x q, 2 ms)`. Rule (b) bounds the cost of an unreachable cohort target (real product concurrency fluctuates; a batch smaller than the last would otherwise wait out the whole fsync-sized deadline — measured **-30%** on the product multi-table write path without it).
Three corrections vs the previous phase, all measured:
1. **No cohort-size cutoff** (previously 256). With combining it cost M1.3 ~13k and had no remaining justification.
2. **The lone-writer probe now applies only if the previous batch had ≤ 1 record** (`PROBE_ONLY_UP_TO_COHORT = 1`). Measured: with the old probe, ~49% of batches at 100 writers closed on the probe with ~70 of 100 records (`closes=[0,0,12,630,707]`), creating ~1,330 fsyncs instead of 1,000; with the fix 997 of 1,001 batches close by the cohort rule and every batch holds ~100 records. Single-writer behavior is unchanged (previous batch = 1 ⇒ probe active).
3. **No decaying high-water target.** One was implemented to cure a suspected downward ratchet; the evidence for the ratchet turned out to be a measurement artifact in my soak tool and the mechanism showed no benefit, so it was reverted (`PHASE_RUBIXDB_WAL_PERFORMANCE_FINAL.md` §6).
The early close can only close the window **earlier than the existing deadline**; it never extends it and never changes which records an fsync covers (decided later at `snapshot_sync_target`).

## 3. Constants and their evidence (Rule 27)
| Constant | Value | Evidence / safe default |
|---|---|---|
| `QUIESCENCE_WINDOW` (floor) | 100 µs | throughput flat for 50-200 µs after the probe fix (M1.2 16.7-18.7k, M1.3 82-100k, 4 warm interleaved runs each); no cliff |
| `QUIESCENCE_NS_PER_RECORD` | 400 ns | scales the quiet interval with cohort size so large cohorts are not closed on a scheduling hiccup; indistinguishable from 200 ns at 100 writers |
| `QUIESCENCE_MAX` | 1 ms | far below the fsync-latency-sized window it shortens |
| `STRAGGLER_QUIET_MULTIPLIER` / `STRAGGLER_QUIET_MAX` | 4 / 2 ms | chosen by a sweep on BOTH closed-loop M1.x and the real product multi-table path: x4 = M1.2 16.2-17.7k, M1.3 98-102k, product parity; x8 product -3..-7%; disabled product -30% |
| cohort target | 100% of previous batch | 90% indistinguishable |
| `PROBE_ONLY_UP_TO_COHORT` | 1 | preserves the lone-writer protection exactly (M1.1 unchanged) |
| `APPEND_COMBINE_MAX_OPS` | 4096 | safety bound on per-round work/memory |
| `PROBE_WINDOW` | 200 µs (unchanged) | pre-existing |
All defaults are *safe* in the sense that a wrong value can only produce smaller batches (less throughput), never a durability or ordering change.

## 4. Tests added
* `append_group_equals_sequential_appends` (proptest, 48 cases, tiny segments forcing rotations, small `max_record_len` forcing encode failures): same per-op Ok/Err, same positions, same recovered records as sequential `append`.
* `an_encode_failure_fails_only_that_op_and_consumes_no_seq`.
* `a_failed_group_write_fails_every_writer_in_it_and_leaves_no_trace_or_seq_gap`.
* `rotation_inside_a_group_keeps_the_written_prefix_when_a_later_run_fails`.
* `flat_combining_many_writers_every_ack_is_recovered_in_order` (48 threads x 40: per-thread order, gap-free seq, acked record at its acked seq).
* `a_failed_combined_write_fails_its_writers_and_the_committer_keeps_working` (no false ack, no gap, committer not poisoned).
* `a_panicking_combiner_never_leaves_an_appender_blocked`.
* `cohort_close_decision_table` (updated: straggler fallback boundaries at 4x quiet, no early close when nothing arrived or no prior batch, no size cutoff), `small_prior_batch_does_not_ratchet_batches_small` (from the previous phase).
Existing suites (WAL unit/fuzz/abort-point/fault-injection, M1.1/M1.4-M1.6, watermark, pathological recovery, crash consistency) are unchanged and pass (see certification).

## 5. Known limitations / risks (not hidden)
* **Product-level SQL write throughput is unchanged (parity ±5%)** by this change: the engine's write path is `BatchCoordinatorPool` (one coordinator thread appends sequentially, then one `await_durable`) and the relational layer holds a per-table lock across the durable commit (separate item, untouched, Rule 23). The gain is for callers that append from many independent threads (the M1.2/M1.3 shape and any future direct multi-threaded use). An intermediate build *did* regress the product path by 30% (§2.3); that was found by the product-level A/B and fixed before certification. Measured in `..._PERFORMANCE_FINAL.md` §5.
* **Tail latency:** p99.9/max are worse at 256-512 writers (disclosed in `..._PERFORMANCE_FINAL.md` §4).
* **Pre-existing load-sensitive test:** `concurrent_followers_all_fail_fast_when_the_leader_panics` fails 11 of 12 times under CPU saturation on the **unmodified** baseline as well as on this branch (its 22.7 ms follower timeout can expire before a panicking leader unwinds); it passes unloaded and failed once in the debug full-workspace run. Not changed.
* A batched write failure fails *all* writers in that run (previously only the failing record's writer). All receive `Err` with no state change (rolled back); callers may retry. This is stricter than necessary but never lies.
* The combiner copies each op (`to_owned_op`): one allocation per append (~100 ns), negligible against the ~4 ms fsync.
* `COHORT`/quiescence constants were chosen on this machine; they are robust (flat region) but not re-measured on other hardware (NVMe unavailable).
