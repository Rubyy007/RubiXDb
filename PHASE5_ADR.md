# RubiXDB Phase 5 — Architecture Decision Records

## ADR-P5-0: Release-gate audit — Phase 3C soak sequencing corrected mid-session

**Status**: Accepted, self-corrected.

**Context**: The operating brief required auditing Phase 3C's still-
outstanding long-soak certification and either running it or explicitly
carrying it forward. The true methodology (`PHASE3C_TEST_PLAN.md`:
`long_soak_test -- 100 14400 ...` then `... 1000 14400 ...`, ~8 hours
total) was launched in the background at the very start of this
phase's work, before any of Phase 5's own required clean benchmarks had
been captured.

**Mistake made and caught within the same session**: killing the first
leg's child process early (to reconsider the plan) caused the wrapper
shell script — which simply runs the two `long_soak_test` invocations
sequentially with no exit-code check — to treat the kill as "leg 1
finished" and immediately launch leg 2 (the 1000-writer, 4-hour leg).
Left running, this would have meant every one of Phase 5's own required
benchmarks (`PHASE5_PERFORMANCE.md`) executed while an unrelated,
CPU-heavy 1,000-writer soak competed for this machine's 8 logical
cores — exactly the contamination class `PHASE4A_ADR.md` ADR-P4A-6 was
written to avoid, and exactly the discipline `PHASE3C_TEST_PLAN.md` §1
rule 5 already established.

**Decision**: Stopped the entire soak process tree immediately
(verified via `Get-CimInstance Win32_Process` that no descendant
survived), ran every one of Phase 5's own required clean measurements
first, and relaunched the full, uninterrupted two-leg soak only as the
final action of this phase's own work (`PHASE5_PERFORMANCE.md` §6) —
so it can run unattended afterward without contaminating anything this
phase still needed to measure.

**Consequences**: The Phase 3C long-soak gap is genuinely, not
performatively, being worked — but its full multi-hour result is not
available by the time this document set is finalized. Recorded as
**IN PROGRESS**, not fabricated into a PASS (`PHASE5_TEST_RESULTS.md`
§0). This is also recorded as a process lesson: a wrapper script that
sequences multiple long-running legs must check the exit status of
each leg before proceeding to the next, rather than assuming any
termination means "done, move on" — worth fixing in that script itself
before it's reused again, though not blocking for this phase.

---

## ADR-P5-1: The 100-writer flush anomaly — ruled out by ablation, not explained

**Status**: Accepted (partial resolution, explicitly not overclaimed).

**Context**: `PHASE4B_PERFORMANCE.md` §3.3 recorded, unexplained, that
a small-memtable-with-real-flush configuration measured *faster* at 100
writers than a large-memtable-no-flush configuration — the opposite of
the naive expectation. A hypothesis was floated (bounded `BTreeMap`
depth from frequent freezing) but not tested.

**Decision**: Built `examples/freeze_ablation_test.rs`, a dedicated
harness using the identical write path (`BatchCoordinatorPool::submit`
-> `Completion::wait()` -> `MemTable::insert`) with the *same* freeze
frequency as the flush variant, but discarding each frozen memtable
immediately instead of building an SSTable from it — isolating "freeze
frequency alone" from "flush I/O alone."

**Result**: The freeze-only variant measured at or slightly *below* the
no-freeze baseline (15,965-16,339 ops/sec vs. 16,803-17,154 ops/sec),
never above it — **this conclusively rules out the bounded-`BTreeMap`-
depth hypothesis**: it is not freeze frequency that produces the
speedup. The real-flush variant, measured in the same session under
the same conditions, was still consistently fastest (19,260-20,499
ops/sec).

**What this does NOT do**: explain *what actually* causes the
speedup. Per the operating brief's own explicit instruction ("do not
manufacture a causal explanation"), no further story is constructed —
this is recorded as **conclusively narrowed** (one specific, previously
plausible hypothesis eliminated with real evidence) but **not fully
explained**, an accepted, non-blocking, open observation
(`PHASE5_PERFORMANCE.md` §5). A future investigation with proper
profiling (not available in this environment) would be needed to go
further, and is not attempted here rather than guessed at.

---

## ADR-P5-2: `Manifest` frame format is an independent implementation, not a shared function with the WAL

**Status**: Accepted — see `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §2
for the full technical account.

`wal::format::encode_frame`'s own doc comment (written during Phase 4A)
claimed it was "kept generic... so the Manifest's own edit types can
reuse this same framing function" — but its actual implementation
always prepends an 8-byte `seq` field before the op tag, which the
Manifest's own body layout (`edit_type(1) || type_fields`, no `seq`
prefix) does not have. Refactoring `encode_frame` to genuinely decouple
the `seq` field was judged higher-risk (touching already-certified,
200+-test-covered WAL code for a ~15-line savings) than writing an
independent, byte-compatible-at-the-header-level implementation in
`src/manifest/format.rs`. The stale doc comment on `encode_frame` was
corrected in place (a zero-behavior-change documentation fix) so this
discrepancy is never silently rediscovered.

---

## ADR-P5-3: Manifest recovery is two-phase, to preserve the existing WAL lock-ordering constraint

**Status**: Accepted — see `PHASE5_MANIFEST_ARCHITECTURE.md` §4 for the
full derivation.

`PHASE4A_ADR.md` ADR-P4A-3 already established that `wal::replay_
streaming`'s shared lock must be acquired and released *before*
`FileWal::open_for_recovery`'s exclusive lock, in the same process
(the exclusive lock blocks a same-process shared-lock attempt).
Manifest recovery needs to supply the checkpoint boundary to `wal::
replay_streaming`'s own filtering callback *before* that call runs, but
the write-capable part of Manifest recovery (the SSTable-directory
reconciliation sweep, which can append a recovered `ADD_SSTABLE` edit)
needs the *exclusive* lock. Splitting into `manifest::replay_readonly`
(shared lock, run first, released before `wal::replay_streaming`) and
`Manifest::open_after_exclusive_lock` (run after `FileWal::open_for_
recovery`, re-replaying the small file a second time under exclusive
protection) resolves this without touching `wal::replay_streaming`'s
own signature or lock behavior at all.

**Consequence accepted explicitly**: the Manifest file is replayed
twice per `LsmEngine::open` call (once read-only, once under exclusive
lock). This is a deliberate, bounded, one-time startup cost (the file
is small — `RubixDB-LSM-Engine-Specification-v1.0.md` §6.3's own
"grows unboundedly... accepted Phase 0 limitation" not yet a concern at
this project's tested scale), not a hot-path cost.

---

## ADR-P5-4: A real idempotent-retry bug, found by the phase's own crash-cycle testing, fixed at the granularity the bug actually lived at

**Status**: Fixed, verified by the same test that found it.

**Context**: The first working version of the flush pipeline's retry
loop tracked only whether the SSTable itself had been built and
`ADD_SSTABLE`-recorded (`published: Option<(id, meta)>`) before
retrying. Steps after that — `pool.submit(WalOpOwned::CheckpointMarker
{..})` and the `SET_CHECKPOINT` Manifest append — were unconditionally
re-run on every retry attempt, regardless of whether they had already
durably succeeded.

**How it was found**: `examples/sstable_flush_crash_test.rs`, extended
this phase to assert `active_entry_count() + recovery_stats().
checkpoint_markers_replayed == highest_seq - checkpoint_seq` exactly
after every real crash-kill cycle, failed on 94 of its first 100 real
cycles — not with a crash or data loss, but with an exact-accounting
mismatch that pointed directly at extra, unaccounted-for
`CHECKPOINT_MARKER` records in the WAL. Tracing this back to the retry
loop found the missing per-step guard.

**Fix**: `checkpoint_marker: Option<WalPosition>` and `checkpoint_
recorded: bool` were added alongside `published`, each independently
gating its own step so a retry only ever repeats work that has not yet
durably succeeded (`src/lsm/mod.rs`'s `spawn_flush_thread`, full
account in that function's own doc comment). `pool.rotate()` and
`pool.purge_before(..)` were deliberately left unguarded — both are
already idempotent, already-tested WAL primitives, so repeating them on
retry is harmless by construction, not merely assumed so.

**Re-verified**: 180/180 real external-process-kill cycles (two seeds)
after the fix, zero invariant violations, zero data loss, checkpoint
monotonicity held throughout, orphaned (never-checkpointed) markers
correctly accumulate across repeatedly-interrupted flush attempts and
correctly get subsumed once a later flush's checkpoint covers them —
this is itself the second thing this exercise proved: **an orphaned,
un-checkpointed `CHECKPOINT_MARKER` record is always safe** (it is
inert during replay, consumes a sequence number, and is silently
subsumed by the next successful checkpoint) — a property now recorded
explicitly in `RecoveryStats`'s own doc comment and `PHASE5_FAILURE_
MODEL.md` §3, not left as an implicit assumption.

---

## ADR-P5-5: Flush-thread panic — caught per-attempt, not supervised-restart

**Status**: Accepted, implemented.

**Context**: `PHASE4B_FAILURE_MODEL.md` §1 already named this gap: "a
poisoned lock/panicked thread is a fail-closed event, not silently
recovered... flushing simply stops until the process is restarted."
Once WAL retention/purging depends on flushing continuing to make
progress, a permanently-stopped flush thread is a materially worse
operational failure mode than it was in Phase 4B (WAL and immutable
state now accumulate without the safety valve checkpointing was
supposed to provide) — the operating brief explicitly asked this
phase to decide whether that remains acceptable.

**Decision**: **Not** a supervised-restart design (killing and
respawning the flush thread's `JoinHandle` on panic). The operating
brief itself named the exact risk with that approach: an automatic
restart "could duplicate an SSTable or replay an unsafe checkpoint
transition" if the restarted thread's own state doesn't correctly
reflect what the panicked attempt had already durably done. Instead,
each flush attempt now runs inside `std::panic::catch_unwind`, and a
caught panic is treated as **exactly the same kind of failure as an
I/O error** — logged, retried with the same backoff policy, using the
*same* per-step idempotence guards (ADR-P5-4) that already correctly
handle "some earlier step already durably succeeded, don't redo it."
The thread itself never dies, so there is no restart, no fresh state to
reconcile, and no new failure mode to reason about beyond the one
already proven safe by 180 real crash cycles.

**Verification**: Not exercised by a dedicated fault-injection test
this phase (would require a new test-only panic-injection hook into
`sstable::write_from_memtable` or similar) — this is an explicit,
named scope limitation, not a silent gap. Confidence instead rests on
(a) `catch_unwind`/`AssertUnwindSafe` being a standard, well-understood
Rust mechanism, (b) the caught-panic code path reusing the identical
retry/idempotence state machine already validated by 180 real external-
process-kill cycles (a panic and an I/O error are handled by
structurally the same code from the `Err`/caught-unwind boundary
onward), and (c) code review confirming no lock is held across the
`catch_unwind` boundary in a way that could leave it poisoned (the
`Mutex<Manifest>`/`RwLock` guards are all acquired and released
*within* the closure, never held across it).

---

## ADR-P5-6: `RecoveryStats` added as real observability, not just to fix a test

**Status**: Accepted, exposed as a permanent public API.

ADR-P5-4's bug was found because the crash test needed an *exact*
accounting of WAL records above the checkpoint, which required
counting `CHECKPOINT_MARKER` occurrences during replay — information
`LsmEngine` didn't previously expose. Rather than compute this only
inside the test binary, it was added as `LsmEngine::recovery_stats()`
returning a public `RecoveryStats` struct (`wal_records_visited`/
`_applied`/`_skipped_by_checkpoint`, `checkpoint_markers_replayed`,
`manifest_edits_replayed`, `recovery_duration`) — directly satisfying
the operating brief's own separately-listed observability requirements
("recovery duration," "recovery WAL records replayed," "recovery
Manifest records replayed") rather than treating them as a documentation
exercise disconnected from what the crash test actually needed and
found valuable.
