# RubiXDB Phase 3C — Architecture Decision Records

## ADR-P3C-1: Recovery API design decision analysis (operating brief §9) — analyzed, NOT implemented

**Status**: Analysis complete; implementation explicitly deferred, per
the operating brief's own instruction ("do not automatically implement
one... if implementation is needed, make it a separately documented
WAL increment rather than quietly changing recovery semantics").

**Context**: `examples/recovery_memory_scaling.rs` (§8) confirmed and
quantified `PHASE3B_ADR.md` ADR-P3B-5's finding: `open_for_recovery`'s
RSS cost is linear in record count at ~134 bytes/record (measured
1M-15M), extrapolating to ~11.4 GiB at the 85M-record scale that
exhausted this project's own 16 GiB development host in Phase 3B. The
root cause is architectural, not a harness artifact: `WalkOutcome.
records: Vec<(u64, WalOpOwned)>` (`src/wal/recovery.rs`) — the
lowest-level primitive `open_for_recovery`/`inspect`/`scan_directory`
all build on — materializes every record as an owned value in one
growing `Vec`, with two separate heap allocations per `Put` record
(`key: Vec<u8>`, `value: Vec<u8>`). There is no streaming variant
anywhere in the call chain, down to this core primitive.

**What callers actually need** (analyzed, not assumed):
1. **A caller reconstructing full in-memory state** (e.g., a future
   MemTable-rebuild-from-WAL recovery path, per operating brief §26's
   own anticipated need) needs every record, in order, but does **not**
   need them all held in memory *by the WAL layer* simultaneously — it
   applies each one to its own target structure (a MemTable insert) and
   can then let the WAL-owned copy be dropped.
2. **A caller only checking WAL health** (this project's own `tests/
   crash_consistency.rs`-style verification, and most of this phase's
   own test harnesses) needs record *count*, highest *sequence*, and
   corruption status — not the record contents at all.
3. **No caller identified in this project's current scope** needs
   random access into the full recovered set, or needs to hold every
   record in memory at once for its own sake (Phase 0's original design
   intent — LSM Engine Spec §7 recovery reconstructing a Memtable — is
   itself a sequential-apply consumer, matching need #1, not a
   full-materialization consumer).

**Where corruption detection must complete**: the existing fail-closed
contract (`wal::mod`'s "# Durability" section, the Group 3.1 amendment)
already processes segments *in order*, stopping at the first
corruption — it does not need to see the whole file before deciding.
A streaming design is therefore **architecturally compatible with the
existing contract as-is**: a consumer would receive records one at a
time in order, and the stream would end (with an explicit corruption/
truncation signal) at exactly the point `open_for_recovery` currently
reports as `corrupted_segments`/`truncated` — no change to *what* is
trusted, only to *how many records are held in memory at once* while
determining that.

**How sequence-gap validation works, streaming or not**: this WAL
never has genuine internal sequence gaps by construction (`seq` is
assigned monotonically by `FileWal`/`GroupCommitter` at append time,
never left with holes) — every "gap" check throughout this project's
own test suite is really checking "does the recovered prefix start
where expected and stay contiguous," which a streaming consumer can
check incrementally (`assert_eq!(seq, expected_next); expected_next +=
1;`) exactly as this phase's own harnesses (`crash_cycle_test.rs`,
`long_soak_test.rs`) already do against the current, fully-materialized
API — no new validation logic is implied by streaming.

**How memory would remain bounded**: three directions evaluated, per
the operating brief's own list:
1. **Streaming iterator** (`impl Iterator<Item = Result<(u64,
   WalOpOwned)>>`, or a `Read`-style pull API): most flexible, matches
   Rust idiom, composes with `for` loops and adapter methods callers
   already know. Cost: the iterator must own/borrow the open segment
   file handles for its whole lifetime, and error handling for
   "corruption discovered mid-stream" needs a clear contract (does the
   iterator yield a final `Err` item and then stop? does it need a
   separate "was there trailing corruption" query after exhaustion,
   mirroring `WalReplayResult::corrupted_segments` today?).
2. **Callback-based replay** (`fn replay(dir, |seq, op| { .. }) ->
   Result<ReplaySummary>`): simplest to reason about correctness-wise
   (the callback can't outlive the call, no lifetime/ownership
   questions about held file handles), closest in shape to what a
   MemTable-rebuild consumer (need #1 above) would actually call
   directly. Cost: less composable than an iterator for a caller that
   wants to filter/transform before consuming; harder to pause/resume.
3. **Bounded replay batches** (yield `Vec<(u64, WalOpOwned)>` chunks of
   a caller-chosen size `N`, bounding worst-case memory to `O(N)`
   instead of `O(total)`): a middle ground — reuses the existing
   `Vec`-returning shape at small scale (so `WalOpOwned`'s existing
   `Clone`/`PartialEq`/etc. derives and every existing test that
   compares a `Vec` of records keep working unmodified against one
   chunk), while capping the worst case. Weakest bound-tightness of the
   three (a caller can still pick a too-large `N`), but the lowest-risk
   migration path — `WalReplayResult` itself could become "the last
   chunk," keeping today's callers (who pass a small/default chunk
   size, or `usize::MAX` for today's exact behavior) source-compatible.

**No decision made on which of these three to build** — that is
deliberately out of this analysis's scope per the operating brief's own
instruction. This ADR records the analysis so a future, separately-
scoped WAL increment can start from it rather than re-deriving it.

**Consequences**: `open_for_recovery`/`WalReplayResult`/`walk_segment`/
`walk_full_segment` are **unchanged** this phase. The known limitation
(`PHASE3B_ADR.md` ADR-P3B-5) persists, now precisely quantified
(`PHASE3C_TEST_RESULTS.md` §8) rather than anecdotal. `PHASE3C_TEST_
RESULTS.md` §11 records this as an explicit, named blocker to full
certification if the certifying engineer judges the current ~15M-record
safe ceiling insufficient for anticipated production WAL sizes before
checkpointing/compaction exists (Stage B/C of the roadmap) — a
judgment call this ADR does not make unilaterally.

## ADR-P3C-2: `purge_before` exposed on `GroupCommitter`/`BatchCoordinatorPool` to make the long soak realistic, not to work around the memory limitation

**Status**: Accepted, implemented.

**Context**: A true 4-hour soak at full throughput (§3) would generate
far more records than ADR-P3B-5/ADR-P3C-1's own measured ~15M-record
safe recovery ceiling — at ~90K ops/sec, 4 hours is ~1.3 billion
records.

**Decision**: Expose `purge_before` (already existing on `FileWal`,
unexposed above it) on `GroupCommitter` and `BatchCoordinatorPool`, and
have the long-soak harness call it periodically during the run,
bounding the live WAL's own footprint to a fixed retention window
(default 5,000,000 records — comfortably inside the proven-safe range)
for the run's entire duration.

**Why this is not "weakening the test" or "avoiding the real
problem"**: a production deployment of this architecture would **never**
run for 4 hours accumulating an ever-growing, never-truncated WAL in
the first place — checkpointing (advancing a durable watermark and
purging everything before it) is a normal, expected operational
practice for any WAL-based system, already a first-class, tested
operation in this codebase since Phase 1 (`FileWal::purge_before`,
`tests/wal_tests.rs::purge_before_only_removes_fully_superseded_
sealed_segments`). Testing 4 hours of *unbounded* accumulation would
test an operational mode no real deployment uses, while *also*
re-triggering the already-diagnosed, already-analyzed (ADR-P3C-1)
memory limitation for no new evidence. Testing 4 hours *with*
checkpointing tests both the write path's long-duration stability
*and* `purge_before`'s own correctness under sustained concurrent
load for the first time (previously only unit-tested against a static,
non-active WAL) — strictly more coverage, not less.

**Consequences**: the long soak's own final recovery check only
recovers the live (unpurged) tail, not the full multi-hour history —
by design. `PHASE3C_TEST_RESULTS.md` records this explicitly so the
soak's evidence is not misread as "the whole run's data was verified
byte-for-byte" when it is "the write path was live-monitored via
in-process stats for the whole run, and the final live tail was
independently verified via a real recovery."

## ADR-P3C-3: crash-cycle kill mechanism — external `Child::kill()`, not a new in-process abort hook

**Status**: Accepted, implemented.

**Context**: Operating brief §5 asks for crashes at "randomized but
reproducible intervals," terminating "the RubiXDB process while the
workload is active."

**Decision**: `examples/crash_cycle_test.rs` spawns a real child OS
process (`examples/crash_cycle_child.rs`) and kills it externally
(`std::process::Child::kill()` — `TerminateProcess` on Windows) after a
randomized, seeded delay, rather than extending the existing `AbortPoint`/
`std::process::abort()` in-process mechanism (`tests/crash_consistency.
rs`) with a "random point" mode.

**Why an external kill, not an extended abort-point mechanism**: the
existing `AbortPoint` mechanism is deliberately deterministic — it
fires at one of 11 named, specific code boundaries, which is exactly
right for proving "this specific transition is crash-safe" (its actual
job) but wrong for what §5 asks for here: a crash that can land
*anywhere*, including mid-syscall, with zero cooperation from the code
being tested. `Child::kill()` achieves this for real — the killed
process has no chance to run any Rust code at all once the OS delivers
the termination, unlike `abort()`, which is still a *cooperative* call
this project's own code makes at a point it chose. Building a "kill
myself at a random `Instant`" in-process mechanism would still only
ever interrupt at whatever granularity a timer-checking loop polls,
never truly asynchronously — an external kill has no such floor.

**Consequences**: this genuinely exercises, under real external-kill
conditions for the first time, this project's own cross-process
file-lock release-on-death guarantee (`ARCHITECTURE.md`'s "Cross-process
file locking" section) — previously only proven by a clean second-
process rejection test (`tests/wal_tests.rs::cross_process_lock_
prevents_concurrent_writers`), never by an actual abrupt death. 40/40
cycles recovered the lock successfully with no hang (`PHASE3C_TEST_
RESULTS.md` §5).

## ADR-P3C-4: observability — two more genuine additions, full metrics layer still deferred

**Status**: Accepted (partial completion, explicitly not claimed
complete).

**Context**: Operating brief §10-§12 re-asks for the full metrics list
Phase 3B (`PHASE3B_ADR.md` ADR-P3B-3) already scoped down to "audit
plus safe additions," for the same reasons that ADR gives (building an
unmeasured, possibly-contended full metrics layer under time
constraints risks the exact "declare production-ready without
evidence" outcome the operating brief forbids).

**Decision**: add the two further safe, real gaps this phase's own
audit found — `bytes_per_batch` (`BatchCoordinatorStats::bytes_total`
+ `avg_bytes_per_batch()`) and a genuine `writes_timed_out` sub-
classification (distinguishing an `await_durable` retry-budget
exhaustion from every other failure cause, verified by a dedicated
test) — both single per-batch atomic increments, the same proven
low-contention shape every existing counter in this codebase already
uses.

**What remains explicitly undelivered, and why**: percentile
`commit_latency` (p50/p95/p99) wired into the production library
itself. `examples/soak_test.rs`/`long_soak_test.rs`'s own per-thread-
local-slot-plus-periodic-aggregation design (Phase 3B, reused
unmodified this phase) is a **validated blueprint**, not yet a
library feature — wiring it into `BatchCoordinatorPool` as an opt-in
capability, with its own on/off overhead measurement (§12), is real,
separately-scopable work this session did not have the remaining
budget to do carefully. Recorded as an explicit blocker (`PHASE3C_
TEST_RESULTS.md` §11), not silently dropped.

**Ownership note** (§10's "for metrics that cannot correctly live
inside the storage core, document the correct ownership layer"):
`recovery_count` (how many times this WAL directory has been recovered
across process restarts) cannot correctly live inside the storage core
at all — `open_for_recovery` is a one-shot, per-process-lifetime call
with no persistent state of its own between processes; counting
"recoveries" is inherently a fact about the *deployment's* restart
history, which only a layer above this library (an operator, a
supervisor, or the embedding application) can observe and record. This
project's own `WalReplayResult` correctly has no such field, and adding
one would mean inventing cross-process state this library does not
otherwise keep.
