# Changelog

All notable changes to RubixDB are recorded here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project has not cut a
release yet, so everything so far lives under `[Unreleased]`.

## [Unreleased]

### Compaction: production trigger + execution integration, Increment 2 (2026-09-21)

`ADR-COMPACTION-001` Amendment 1 implemented: a real, automatic
background compaction worker (`spawn_compaction_thread`), wired to
`LsmEngine::open` behind a new opt-in `LsmConfig.compaction_auto_
trigger` flag (**default `false`**, deliberately -- reversed from an
initial `true` default after two concrete Increment-1 test failures
showed it would otherwise silently start compacting under already-
certified test surface). Dual wake source (flush-triggered
notification + periodic fallback tick, reusing the existing `storage_
pressure_retry_interval`, no new config field); `CompactionRunGuard`
(one `AtomicBool` RAII guard, the smallest primitive preventing
concurrent compactions, no global lock); `shutdown()` extended with an
explicit, tested contract (in-progress cycle always completes, never
aborted, no leak, no deadlock, no partial publish). Two real bugs
found and fixed empirically: a `shutdown()` message-loss bug
(`try_send` vs. blocking `send` on the bounded worker channel) that
caused a real multi-minute slowdown under heavy test load; and a
narrow pre-existing race in a shared test fixture helper (fixed via a
new, additive `flush_completions` observability counter). A separate,
genuinely unbounded test-design hazard (racing an already-running
worker to observe a live SSTable count) was found and fixed uniformly
across every affected test by building fixtures offline first, then
reopening with the worker enabled. 12 new tests (346/346 total,
`cargo test --lib`, debug and release, run twice post-fix with zero
flakiness): deterministic threshold firing, direct re-entrancy
stress test, storage-pressure defer/resume, failure+retry, shutdown
mid-cycle, snapshot safety, deferred-deletion retry, a bounded
production-like integration run, crash recovery through the real
automatic path, bounded resource safety, and a first bounded
performance/storage-budget/latency baseline (measured on-disk peak
matched the ADR's own theoretical `input+output` figure exactly).
Full regression gate clean; zero changes to WAL/Manifest/error types;
zero new dependency; zero `unsafe`. Full detail: `PHASE_COMPACTION_
INCREMENT2_RESULTS.md`, `PHASE_COMPACTION_ADR.md` Amendment 1,
`PROGRESS.md`'s 2026-09-21 "Compaction Increment 2" entry.

**COMPACTION CORE = PASS. COMPACTION TRIGGER INTEGRATION = PASS.
COMPACTION PRODUCTION READY = NO** -- remaining: full performance
characterization, a real OS-level resource benchmark, long-duration
endurance, storage-pressure endurance, and a final certification
matrix. Write Engine and Read Engine production-ready status
unchanged.

### Compaction: deterministic core implementation, Increment 1 (2026-09-21)

`ADR-COMPACTION-001` implemented as the deterministic core operation
only -- `LsmEngine::compact_once`/`should_compact` (no production
caller yet; no automatic trigger or background thread, by explicit
design), `LsmConfig.compaction_trigger_count` (default 4), the
engine-agnostic size-tiered full-merge k-way merge + version/tombstone
retention algorithm (`src/compaction/mod.rs`, reusing Increment 6's
persistent `SsTableRangeCursor` directly for bounded memory), and a
generalized, streaming SSTable writer entry point
(`sstable::write_from_sorted_records`) -- `write_from_memtable` is now
a thin adapter over the same shared core, verified byte-for-byte
behavior-preserving by a new differential test. A real correctness
refinement was found and fixed during implementation: the architecture
report's own worked truth table had two under-specified rows (only
correct under an unstated single-live-snapshot assumption); the
implemented, tested algorithm is the conservative, generally-correct
one, documented via an erratum rather than a silent rewrite. 26 new
tests plus 2 writer differential tests (334/334 total): correctness
differential (2,000 ops vs. an independent reference model), property
test, all 6 crash-window fault points (each a real panic + real
restart), the previously-zero-coverage orphan-recovery branch,
concurrent flush, concurrent readers, a real Windows positional-read-
after-unlink test, and storage-pressure deferral. Full regression gate
clean; zero changes to WAL/Manifest/error types; zero new dependency;
zero `unsafe`. Full detail: `PHASE_COMPACTION_INCREMENT1_RESULTS.md`,
`PROGRESS.md`'s 2026-09-21 "Compaction Increment 1" entry.

**COMPACTION PRODUCTION READY = NO** -- no automatic trigger, no
dedicated benchmark, no long-duration soak, no final certification.
Write Engine and Read Engine production-ready status unchanged.

### Read Engine: final certification (2026-09-21)

`PHASE_READ_ENGINE_CERTIFICATION.md` (new): final certification of the
single-engine, non-partitioned LSM Read Engine, commit `22be3e4`.
30-row certification matrix (point lookup through protected Write
Engine integrity) -- **30/30 PASS, 0 FAIL, 0 mandatory OPEN**. Full
protected-path audit (`git diff` against the Write Engine's own
certification baseline) confirms zero changes to WAL, Group Commit,
Batch Coordinator, Manifest, checkpoint, WAL purge, or `StoragePressure`
logic across the entire Read Engine phase. Final regression gate and a
bounded (not a new soak) performance reconfirmation both re-run clean
at certification time. Known, explicitly non-blocking limitations
documented rather than hidden: no Compaction yet (read amplification
still scales with live SSTable count), and the "no memory leak" finding
rests on source-level ownership analysis plus two converging soak
observations rather than an external memory profiler (none available
in this environment).

**READ ENGINE PRODUCTION READY = YES** -- scoped explicitly to the
single-engine, non-partitioned LSM Read Engine. Compaction, Router,
Replication, and the larger partitioned RubiXDB architecture are not
certified and do not exist in this codebase yet; full RubiXDB
production readiness is not claimed.

### Read Engine: fresh 4-hour integrated soak against the optimized implementation, Increment 7 (2026-09-21)

Re-validated `ADR-RE-002` Option A against a fresh, full 4-hour,
production-profile integrated write/read soak (identical profile to
Increment 4's own: 8 writers, 16 readers, seed 20260920), not just the
controlled `overlap_repro` benchmark. `RESULT=PASS`: 8,372,161
writes+deletes, 6,116,654 reads, 678,708 range scans (every one
checked against the independent reference model at an aged snapshot,
zero disagreed), zero in-run/post-recovery mismatches, clean recovery,
zero storage pressure events. `range_large` p50 improved **3.26x-3.89x**
against Increment 4 at matched SSTable counts (landing inside Increment
6's own 3.20x-3.69x controlled-benchmark prediction), with a flatter
growth curve (~1.25 apparent exponent vs. Increment 4's ~2.20).
`blocks_read`-based amplification improved 5.73x-8.05x at matched
counts. RSS-vs-SSTable-count fit tightened from R²=0.698 to **R²=0.984**
-- the large non-monotonic RSS swings Increment 4's own soak showed are
essentially gone under the same real workload with the bottleneck
fixed. A separate, bounded crash/recovery run (20/20 cycles) confirmed
real post-recovery read correctness. Full regression gate re-run clean.
Full detail: `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` (new), `PROGRESS.
md`'s Increment 7 entry. **Increment 7 = PASS. Status unchanged: READ
ENGINE PRODUCTION READY = NO** -- final evidence consolidation,
performance validation, resource validation, and the certification
matrix remain outstanding.

### Read Engine: persistent range source cursors, `ADR-RE-002` Option A, Implementation Increment 6 (2026-09-20)

Implemented the optimization Increment 5's investigation identified and
`ADR-RE-002` proposed: `RangeScanIter`'s SSTable sources now use a
persistent, owned-`Arc` cursor (`SsTableRangeCursor`, new,
`src/sstable/reader.rs`) instead of a resume-point `Bound` plus a
fresh `range_scan_raw` call per key -- eliminating the repeated binary
search and repeated block re-read/re-decode Increment 5 traced and
reproduced. **Zero `unsafe`, zero new dependency** (owning `Arc
<SsTable>` inside the cursor sidesteps the self-referential-struct
problem structurally). Before/after benchmark on the identical,
unmodified `overlap_repro` workload (`n=7` reps/checkpoint, 5
checkpoints 20-300 SSTables): `blocks_read` (unchanged counting point)
dropped by an exact, constant **4.714x** at every checkpoint; wall-clock
p50 improved **3.20x-3.69x**. `sstables_consulted`'s semantics were
intentionally revised (once per live SSTable per scan, matching point
lookups' own convention, not once per key drawn) and documented, with a
new regression test (`lsm::tests::range_scan_source_cursor_persists_
across_keys_instead_of_reconstructing_per_key`) asserting the exact
count rather than timing. Resource-lifetime check: 400 repeated
create/consume/drop scan cycles show zero handle delta, zero thread
delta, ~0.55 KB/scan RSS noise (not a leak). Point-lookup code paths
(`get`/`get_as_of`/`contains`) untouched -- confirmed by diff, no
regression. Full regression suite (306/306 `cargo test --lib` debug and
release, `wal_tests`, `crash_consistency`, `pathological_recovery_
matrix`, `fmt`/`clippy`) clean. Full detail: `PHASE_READ_ENGINE_
PERFORMANCE.md`'s Increment 6 section, `PROGRESS.md`'s Increment 6
entry, `PHASE_READ_ENGINE_RANGE_PERFORMANCE_ADR.md` §9. **`ADR-RE-002`:
IMPLEMENTED.** **Status unchanged: READ ENGINE PRODUCTION READY = NO**
-- final corruption/recovery, integrated endurance, performance
validation, and certification matrix remain outstanding.

### Read Engine: memory + range-performance investigation, Implementation Increment 5 (2026-09-20)

Investigated three items Increment 4's completed 4-hour soak flagged
rather than silently resolved: constant `snapshots_live=50`, ~2GB
final RSS, and visibly high late-run `range_large` latency. **Range
scan latency root cause found, traced in source, and independently
reproduced**: `range_large` p50 grew from 1.15ms to 43.1 seconds over
the soak (super-linear, unlike point lookups' known-linear scaling),
traced to `RangeScanIter`'s per-key `refill` re-peeking every source
holding a version of each winning key -- on this project's own
realistic (small-cardinality, heavily-overwritten) endurance workload,
this is O(distinct keys yielded × live SSTable count). Reproduced
exactly in a new, deterministic ~3-minute benchmark
(`examples/read_engine_bench.rs`'s `overlap_repro` section):
`sstables_consulted/sstable` pinned at a constant integer across five
SSTable-count checkpoints. `PHASE_READ_ENGINE_RANGE_PERFORMANCE_ADR.md`
(new, ADR-RE-002) evaluates four fix options and proposes persistent
source cursors (an owned-`Arc` iterator refactor, no `unsafe`, no new
dependency) for a *future* increment's decision -- **no optimization
implemented this increment**. **No memory leak found**: RSS's
monotonic growth is fully explained by per-SSTable index/bloom-filter
metadata (expected pre-Compaction); non-monotonic swings are most
plausibly (not profiler-confirmed) Windows working-set volatility; the
constant `snapshots_live=50` was verified to be the test harness's own
deliberate pool cap, not an engine-side leak -- no snapshot semantics
changed. Full detail: `PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md`,
`PROGRESS.md`'s 2026-09-20 "Increment 5" entry. Certification status
kept distinct rather than collapsed: correctness PASS, performance
OPEN, memory OPEN-but-no-leak-found. **Status unchanged: READ ENGINE
NOT READY.**

### Read Engine: long-duration read soak + integrated write/read endurance, Implementation Increment 4 (2026-09-20)

A real 4-hour soak (`examples/read_write_soak_test.rs`, new) under 8
concurrent writers + 16 concurrent readers against the full real stack
(WAL, MemTable, SSTables, Manifest, checkpoint, WAL purge),
continuously validated against an independent reference model.
`RESULT=PASS`: 13,483,811 writes, 3,375,298 deletes, 4,582,352 reads,
261,455 range scans, zero in-run or post-recovery mismatches, clean
recovery. Also: a mid-session (no restart) corruption-injection test
(`src/lsm/tests.rs`) and `lsm_crash_cycle_test.rs` extended from
"open() returned Ok" to real post-recovery read verification against
exact expected values. Full detail: `PROGRESS.md`'s 2026-09-20
"Increment 4" entry. **Status: READ ENGINE NOT READY** -- three
observations from the soak (RSS growth, constant snapshot count, late-
run range latency) were flagged, not silently resolved, and carried
forward into Increment 5 above rather than assumed benign.

### Read Engine: contains() + performance baseline, Implementation Increment 3 (2026-09-20)

`LsmEngine::contains(key, as_of_seq) -> Result<bool>` (`ADR-RE-001`
§2/§10), backed by a new `SsTable::contains_versioned` that reuses
`get_versioned`'s bloom+index+block walk without constructing an owned
value. 11 new tests (304/304), including two genuine (not simulated)
I/O-failure regression tests across `contains`/`get_as_of`/
`range_scan`, and the differential/property tests extended in place to
a three-way `reference model == get_as_of == contains` invariant.
`examples/read_engine_bench.rs`: a new, real (unmocked) performance
harness; full results in the new `PHASE_READ_ENGINE_PERFORMANCE.md`.
Headline, honestly-reported findings: **`contains()` shows no
measurable performance difference from `get_as_of(..).is_some()`** at
any value size or SSTable count tested (traced to why: the block
decode this method hoped to skip already happens unconditionally);
**point-lookup read amplification scales roughly linearly with live
SSTable count** (no Compaction yet to bound it), dominated by cheap
bloom-negative checks rather than disk I/O; `range_scan`'s
bounded-memory design confirmed with a real number (168KB peak RSS
growth over a 25MiB scan); and a rare, fully-traced, pre-existing
cross-thread `snapshot_seq()` characteristic (not a `contains()` bug,
not a protected-code change made or needed) flagged for future
investigation rather than silently resolved. No cache/mmap/prefetch/
parallel-read/secondary-index optimization added -- per the phase
brief, this increment measures only. Full detail: `PROGRESS.md`'s
2026-09-20 "Increment 3" entry, `PHASE_READ_ENGINE_PERFORMANCE.md`.
**Not** Read Engine production-ready.

### Read Engine: production-grade range_scan, Implementation Increment 2 (2026-09-20)

`LsmEngine::range_scan(start, end, as_of_seq)`/`range(start, end)`: a
real binary-heap k-way merge across active + immutable MemTables + live
SSTables, built on Increment 1's `ReadView`/`Snapshot`/`ReadStats`
foundation, per `ADR-RE-001`. Lazy, ordered, bounded-memory, exactly one
resolved value per logical key, tombstones suppressed, fail-closed on
corruption. Fixed one real, pre-existing bug along the way (test-first,
per the ADR's own explicit authorization): `MemTable::range`'s
`Excluded` bound never actually excluded the boundary key's own
entries. Two more real bugs were caught and fixed before this shipped
by actually running the new tests: a mid-group corruption `Err` was
being silently swallowed instead of propagated, and `BTreeMap::range`
panics (rather than returning empty) on `start > end` or degenerate
`Excluded==Excluded` bounds. 20 new tests (293/293 total), including a
64-case property test against an independent reference model. Full
detail: `PROGRESS.md`'s 2026-09-20 "Increment 2" entry. **Not** Read
Engine production-ready -- that certification has not started.

### Read Engine: architecture report, ADR-RE-001, Implementation Increment 1 (2026-09-20)

New, separately-scoped phase (Write Engine is certified and protected,
unchanged). `PHASE_READ_ENGINE_ARCHITECTURE_REPORT.md` and
`PHASE_READ_ENGINE_ADR.md` (13 resolved decisions) precede any code.
Increment 1 (foundation only, no `range_scan` yet): `Snapshot`/
`SnapshotRegistry` (a real, `Drop`-released, multiset-correct read
watermark, forward-compatible with a future Compaction's `snapshot_
refs` needs), `ReadStats` observability (`read_requests`/`read_hits`/
`read_misses`/`bloom_negatives`/`blocks_read`/`sstables_consulted`,
plus two new counters on `SsTable`), and a `ReadView` foundation type
(`Arc`-clones existing sources, never duplicates a Bloom filter or
index, never copies a whole MemTable). `get`/`get_as_of` gained only
non-functional counter increments -- no behavior change. 17 new tests
(273/273 total), including a deterministic concurrent-flush point-read
test (existing `FlushFaultPoint` machinery, no sleeps) and a
previously-missing lazy-data-block-corruption regression test. Three
real bugs caught and fixed by running these tests before trusting them
(a `Sync`-bound compile error, two freeze-count miscalibrations, and a
real pre-existing `MemTable::range` `Excluded`-bound discrepancy,
explicitly recorded for the next increment). Full detail:
`PROGRESS.md`'s 2026-09-20 Read Engine entry.

### Write-Engine Certification: RSS growth investigated and explained; final decision reaffirmed (2026-09-20)

The soak certified below showed RSS growing +2,334% over its 4 hours.
That was flagged and fully investigated rather than certified past on
trust: traced to source (`SsTable::open()` retains a Bloom filter +
sparse index per open table for the engine's lifetime; no Compaction
exists yet to reclaim old tables) and confirmed by two independent
measurements fitting a near-perfect linear model against SSTable count
(R²=0.9999) — classified expected, bounded-per-table growth, not a
leak. `PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` (new doc). A
regression test locking in the underlying ownership invariants was
added (`lsm::tests::sstable_count_and_immutable_memory_track_flushes_
exactly_no_extra_retention`), and the harness now reports
`min_rss_kb_observed`/`max_rss_kb_observed`, not just start-vs-end.
Performance acceptance was also broadened from one 3-rep set to 15
reps across 3 sessions before being trusted — median clears both hard
targets, with real, already-documented, non-blocking run-to-run
variance reported in full rather than the favorable subset. Final
certification verdict unchanged: **WRITE ENGINE PRODUCTION READY**,
now with a complete 16-gate matrix (`PHASE_WRITE_ENGINE_CERTIFICATION.md`).

### Write-Engine Certification: FINAL DECISION -- WRITE ENGINE PRODUCTION READY (2026-09-20)

The realistic full-pipeline endurance soak (200 writers, 14,400s,
`LsmConfig::default()`) was re-run on a properly provisioned `E:`
volume, per a documented storage budget (`PHASE_WRITE_ENGINE_
STORAGE_BUDGET.md`), and passed clean: `completed_err=0` throughout,
throughput sustained 19,332-26,116 ops/sec with no collapse, 3,294
SSTables published, checkpoint advancing continuously, 0 ENOSPC events.
Fresh 100w/1000w acceptance benchmarks both cleared their hard targets.
Final certification decision: `PHASE_WRITE_ENGINE_CERTIFICATION.md`
(new doc). A real bug in the soak harness's own PowerShell PASS/FAIL
logic (`-notmatch` array-filtering semantics, producing a false FAIL
despite a genuinely healthy run) was found and fixed, verified by
replaying the corrected logic against both this run and the original
failed run before trusting it (`temp/realistic_soak_harness.ps1`).

### Write-Engine Certification: storage-pressure / ENOSPC handling (ADR-WE-SP-001 -- implemented and verified by the re-soak above)

#### Fixed

- The background flush thread's retry loop (`spawn_flush_thread`,
  `src/lsm/mod.rs`) used `max_flush_retries` only to pick a backoff
  duration, never as an actual retry limit: past that budget it fell
  into an **unconditional, unbounded** flat 2-second retry cadence for
  every kind of I/O failure, including a genuinely permanent disk-full
  condition. The 2026-09-19 realistic full-pipeline soak (200 writers,
  `LsmConfig::default()`) hit this exact path when its target volume
  filled at t≈5,100s and spent the remaining ~9,200s of the run
  retrying a doomed flush every 2 seconds instead of failing safe --
  `completed_err` reached 580,190,298, throughput collapsed 97.9%. See
  `PHASE5_ENOSPC_FAILURE_ANALYSIS.md` for the full incident analysis
  (including a from-source proof that the huge `completed_err` number
  was not an accounting bug) and `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md`
  for the fix design and its "Implementation Notes" section for exactly
  what landed. Durability/crash-recovery correctness were never
  affected by the original defect -- this was purely an availability/
  retry/backpressure gap.
- The realistic-soak certification harness (`temp/realistic_soak_harness.ps1`)
  reported PASS on `exit_code == 0` + a clean process tree alone, which
  is how the above defect went uncaught. It now additionally requires
  `completed_err == 0`, no persistent throughput collapse, no ENOSPC/
  retry-storm lines in stderr, a successful recovery line, and a clean
  drained shutdown.

#### Added

- `EngineError::StorageExhausted` (`src/error.rs`) -- a new, additive
  error variant distinct from the generic `Io` variant and from the
  pre-existing `CapacityExceeded` (MemTable-freeze backpressure, a
  different failure mode, contract unchanged). Returned by `LsmEngine::
  put`/`delete` before any WAL append is attempted, once storage is
  confirmed exhausted.
- `lsm::StorageState` (`Healthy` / `StoragePressure` / `StorageFull`),
  an explicit storage-health state machine on `LsmEngine`
  (`storage_state()`, `storage_pressure_events()`), plus
  `LsmConfig::storage_pressure_retry_interval` (default 5s) -- the
  backoff used once a flush's bounded fast-retry budget is exhausted on
  a confirmed ENOSPC-classified failure, replacing the old flat-2s-
  forever cadence for that specific case.
- `LsmEngine::install_flush_io_fault_hook`/`clear_flush_io_fault_hook`,
  a test-only fault-injection point (extends the existing `install_
  flush_fault_hook`/`FlushFaultPoint` pattern to actually substitute a
  real I/O outcome, not just observe) used by the new deterministic
  ENOSPC test below without ever touching real disk capacity.
- Two new tests: `lsm::tests::storage_pressure_state_machine_recovers_after_injected_enospc`
  (in-process, walks the full `Healthy` -> `StoragePressure` ->
  `StorageFull` -> `Healthy` sequence) and `examples/
  storage_pressure_crash_{child,test}.rs` (external-process, kills the
  child while genuinely stuck in `StorageFull` and verifies clean
  recovery -- 10/10 cycles clean).

### Phase 5: RUBIC Manifest (MANIFEST NOT READY FOR COMPACTION -- blockers remain)

Extends the persistent architecture: `... -> RUBIC SSTable -> Manifest
-> Safe WAL Checkpoint/Purge`. Ran the release-gate audit first
(Phase 3C's still-outstanding long soak relaunched at the end of this
phase's own work, after catching and correcting a sequencing mistake
mid-session; the Phase 4B 100-writer anomaly investigated via a
dedicated ablation and conclusively narrowed, not fully explained).

- **`RUBIC_MANIFEST_FORMAT_SPECIFICATION.md`**/**`PHASE5_MANIFEST_
  ARCHITECTURE.md`** (new): the Manifest format was already fully
  specified by the LSM Engine Spec (three edit types, WAL-frame-format
  reuse) -- the real design work was the Manifest-free-to-Manifest-
  authoritative integration: a two-phase recovery split that preserves
  the existing WAL lock-ordering constraint, and the full ten-step
  publish -> checkpoint -> purge sequence.
- **`src/manifest/`** (new): independent (byte-compatible, not shared-
  code) frame implementation, sequential bounded-memory replay with the
  WAL's own torn-vs-corrupt classification, idempotent recovery. Wires
  up the WAL's own `CHECKPOINT_MARKER` op (defined since Phase 4A,
  inert until now) for the first time.
- **`src/lsm/mod.rs`** (extended): the flush pipeline now durably
  publishes, checkpoints, and purges in the exact safe order the WAL
  and LSM specs jointly require; the read path is now Manifest-
  authoritative, never "every `.sst` file found in the directory."
- **A real idempotent-retry bug found and fixed by this phase's own
  crash-cycle testing**: a retried flush attempt could durably resubmit
  a second `CHECKPOINT_MARKER` for one logical flush -- caught by an
  exact-accounting invariant added to the crash harness, not by
  inspection. Fixed via per-step (not just per-SSTable) idempotence
  tracking. Full account: `PHASE5_ADR.md` ADR-P5-4.
- **Flush-thread panic handling** (new): each flush attempt now runs
  inside `catch_unwind`, treated identically to an I/O failure by the
  same proven idempotent-retry machinery -- not a supervised-restart
  thread design, which the operating brief itself flagged as risky.
- **`LsmEngine::recovery_stats()`/`checkpoint_seq()`/Manifest
  inspection accessors** (new): real observability, added because the
  crash test's own exact-accounting invariant needed it.
- **`examples/manifest_soak_test.rs`** (new): a bounded (~3 minute)
  soak with periodic real process kills -- 8/8 cycles clean, WAL byte
  count stayed at exactly 0 across every measurement (checkpoint
  tracked within ~1% of `highest_seq` throughout).

253/253 lib tests pass (216 + 37 new: 32 in `src/manifest/`, 5 new
`LsmEngine` Manifest-integration tests), clippy and fmt clean. Full
design: `PHASE5_ARCHITECTURE.md`/`PHASE5_MANIFEST_ARCHITECTURE.md`;
failure model: `PHASE5_FAILURE_MODEL.md`; decisions: `PHASE5_ADR.md`;
performance: `PHASE5_PERFORMANCE.md`; results and final decision
(authoritative): `PHASE5_TEST_RESULTS.md` -- **MANIFEST NOT READY FOR
COMPACTION -- BLOCKERS REMAIN** (Phase 3C's own WAL certification still
never completed; the true multi-hour Phase 5 soak not yet complete).
No correctness defect found; nothing tested this phase needs to be
redone once those two items close.

### Phase 4B: RUBIC SSTable (RUBIC SSTABLE READY FOR MANIFEST)

Extends the write path: `... -> MemTable -> Immutable MemTable -> RUBIC
SSTable`. Began explicitly before Phase 3C's own WAL certification had
completed (still "Deferred") and while Phase 4A's own certification
remained "NOT YET READY -- BLOCKERS REMAIN" -- documented, provisional
basis: this phase touches no WAL/coordinator internals either, and its
own required benchmark (below) closes one of Phase 4A's two blockers
directly.

The Manifest is explicitly out of scope this phase (a genuine stop-and-
ask decision was made about the resulting WAL-purge/replay-boundary gap
-- see `PHASE4B_ADR.md` ADR-P4B-1): SSTable is a purely additional,
purely derived read-path source; the WAL is never purged/truncated by a
flush, so a corrupt or missing SSTable can never cause data loss this
phase, only reduced read-path availability (`LsmEngine::open` fails
closed on a corrupt discovered SSTable rather than silently degrading).

- **`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`** (new): consolidates the
  already-final LSM-spec byte layout (magic `"RBXSST01"`, CRC32C,
  4096-byte target blocks, 10-bits/key XXH64 bloom filter, 72-byte
  footer) and resolves the Manifest-free decisions this phase needed
  (directory-scan id recovery, "exists and validates" liveness, reused
  WAL `fsync_dir` platform primitive).
- **`src/sstable/`** (new): `format.rs`/`bloom.rs`/`writer.rs`/
  `reader.rs` -- byte-exact encode/decode, atomic tmp-file-then-rename
  publication, bounded-memory reader (index/bloom eager, data blocks
  lazy, lock-free concurrent positional reads). New dependency:
  `xxhash-rust` (pure Rust, zero transitive deps -- the spec-mandated
  XXH64 hash for the bloom filter).
- **`src/lsm/mod.rs`** (extended): background flush thread draining
  `immutables` into published SSTables; read path (`get`/`get_as_of`,
  now fallible) extended to check `active -> immutables -> sstables` in
  recency order; bounded flush retry; a test-only flush-delay hook.
- **A real correctness bug found and fixed**: `SsTable::get_versioned`'s
  `binary_search_by` could skip earlier blocks holding older versions
  of a key whose version run spans a block boundary (a tie-breaking gap
  `binary_search_by` doesn't guarantee against) -- found by this
  phase's own property test, fixed via `partition_point`. Full account:
  `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.6, `PHASE4B_TEST_RESULTS.md`
  §7.
- **`examples/sstable_flush_crash_child.rs`/`sstable_flush_crash_test.rs`**
  (new): 140/140 real external-process-kill crash cycles (two seeds),
  zero failures, against the flush pipeline specifically.
- **`examples/sstable_bench.rs`/`lsm_flush_load_test.rs`** (new):
  SSTable write 102.40 MB/sec / 1.38M records/sec, point lookup
  p50=12µs/p99=34µs; the WAL-only vs. WAL+MemTable vs.
  WAL+MemTable+SSTable-flush comparison Phase 4A's own `ADR-P4A-6`
  deferred -- at the LSM spec's realistic 4 MiB default memtable, flush
  overhead at 1,000 writers is within noise of the WAL-only baseline.

216/216 lib tests pass (170 + 46 new: 43 in `src/sstable/`, 3 new
`LsmEngine` flush-integration tests), clippy and fmt clean. Full design:
`PHASE4B_ARCHITECTURE.md`; failure model: `PHASE4B_FAILURE_MODEL.md`;
decisions: `PHASE4B_ADR.md`; performance: `PHASE4B_PERFORMANCE.md`;
results and final decision (authoritative): `PHASE4B_TEST_RESULTS.md`
-- **RUBIC SSTABLE READY FOR MANIFEST**, conditioned (exactly as Phase
4A's own certification was) on Phase 3C's long-soak certification
eventually landing clean.

### Phase 4A: MemTable + RUBIC format foundation (MEMTABLE NOT YET READY -- blockers remain)

Extends the write path: `Logical Writers -> Dedicated Batch Coordinator
-> Group Commit -> Durable WAL -> MemTable`. Began explicitly before
Phase 3C's own WAL certification had completed (its long soak was
still running) -- documented basis: this phase touches no WAL/
coordinator internals.

- **`RUBIC_FORMAT_SPECIFICATION.md`** (new): the RUBIC storage-format
  family's governance layer. Not Parquet. Not a renaming of the
  existing WAL. References (does not re-invent) the already-specified
  RUBIC SSTable byte layout; genuinely undecided items marked
  `UNDEFINED -- RESERVED FOR SSTABLE DESIGN`.
- **`src/memtable/mod.rs`** (new): `MemTable`/`MemtableValue`, exactly
  per `RubixDB-LSM-Engine-Specification-v1.0.md` §1 ("Status: Final") --
  `BTreeMap<(Vec<u8>, u64), MemtableValue>`, `get_as_of` via
  `range(...).next_back()`, documented size accounting, compile-time-
  enforced `freeze() -> Arc<MemTable>`. No `SkipList` evaluation: the
  spec leaves no degree of freedom there.
- **`wal::replay_streaming`** (new, additive): bounded-memory WAL
  replay, implementing the callback-replay direction `PHASE3C_ADR.md`
  ADR-P3C-1 already analyzed. `open_for_recovery`/`WalReplayResult`/
  `walk_segment`/`scan_directory` unchanged. Fixed a real same-process
  lock-ordering bug found while wiring this up (a not-yet-created WAL
  directory now correctly replays as empty rather than erroring).
- **`src/lsm/mod.rs`** (`LsmEngine`, new): Phase-4A-scoped write-path
  facade -- `put`/`delete`/`get`/`get_as_of`, WAL-durability-before-
  MemTable-apply ordering enforced and verified (a direct fsync-failure
  test, plus 25/25 real external-process-kill crash cycles each showing
  `active_entries == highest_sequence == durable_through` exactly).
  Freeze-to-immutable with bounded backpressure (`EngineError::
  CapacityExceeded`).
- **`examples/lsm_crash_cycle_child.rs`/`lsm_crash_cycle_test.rs`**
  (new): real external-process-kill crash tests at the WAL/MemTable
  boundary, mirroring Phase 3C's own proven design.
- **`examples/memtable_bench.rs`/`lsm_load_test.rs`** (new): performance
  harnesses. MemTable-only measured cleanly (1.45M puts/sec, get
  p50=400ns); the full WAL-vs-WAL+MemTable comparison explicitly
  deferred, not fabricated, after a smoke-scale attempt showed clear
  contamination from the still-running background soak.

170/170 lib tests pass (130 + 40 new: 13 MemTable unit tests, 2
property tests x 1,000 cases, 6 `replay_streaming` tests, 19 `LsmEngine`
tests), clippy and fmt clean. Full design: `PHASE4A_ARCHITECTURE.md`/
`PHASE4A_MEMTABLE_ARCHITECTURE.md`; failure model: `PHASE4A_FAILURE_
MODEL.md`; decisions: `PHASE4A_ADR.md`; results and final decision
(authoritative): `PHASE4A_TEST_RESULTS.md` -- **MEMTABLE NOT YET READY
FOR RUBIC SSTABLE IMPLEMENTATION -- BLOCKERS REMAIN** (the WAL-vs-
WAL+MemTable performance comparison is not run; Phase 3C's own WAL
certification had not completed). No correctness defect found; nothing
tested this phase needs to be redone once those two items close.

### Phase 3C: final WAL/coordinator release certification (long soak in progress)

Targets Phase 3B's own six named blockers directly.

- **`GroupCommitter`/`BatchCoordinatorPool::purge_before`** (new):
  mirrors the existing `rotate()` wrapper, delegating to `FileWal::
  purge_before` under the `wal` lock. Safe to call concurrently with
  ongoing writes. Enables realistic checkpointing during a genuinely
  long soak.
- **`examples/crash_cycle_child.rs` + `crash_cycle_test.rs`** (new):
  periodic forced-crash-during-soak testing via a real external
  process kill (`Child::kill()`, randomized/seeded/reproducible delay)
  against a real child process running the production
  `BatchCoordinatorPool` — a genuinely new, asynchronous, uncooperative
  fault-injection class. 40/40 cycles recovered cleanly, zero
  corruption, monotonic gap-free sequences.
- **`examples/long_soak_test.rs`** (new): extends Phase 3B's
  `soak_test.rs` with periodic checkpointing and CPU sampling
  alongside RSS. A true 4-hour-per-writer-level run (100w then 1000w)
  launched against the production `BatchCoordinatorPool`.
- **`tests/pathological_recovery_matrix.rs`** (new): 9 consolidated
  fixture tests against the existing, unmodified recovery contract —
  9/9 pass, including two genuinely new corruption classes beyond
  Phase 0/1's own coverage.
- **`examples/recovery_memory_scaling.rs`** (new): quantifies
  `PHASE3B_ADR.md` ADR-P3B-5's finding with real swept data (1M-15M
  records) — RSS scales linearly at ~134 bytes/record, recovery
  throughput stays flat regardless of scale. `PHASE3C_ADR.md`
  ADR-P3C-1 analyzes (does not implement) a future streaming/callback/
  bounded-batch recovery API redesign.
- **`BatchCoordinatorStats::{bytes_total, writes_timed_out}`** (new):
  two more genuine, low-contention observability fields.
- Security/dependency review completed: zero `unsafe` code, zero
  payload logging in any Phase 3C addition; `Cargo.lock` fully
  reviewed (no new production dependency); `cargo-audit`/`cargo-deny`
  not installed (crates.io network access unavailable this session,
  decision documented).

130/130 lib tests pass, clippy and fmt clean. Full design:
`PHASE3C_ARCHITECTURE.md`; failure model: `PHASE3C_FAILURE_MODEL.md`;
decisions: `PHASE3C_ADR.md`; results and current status (authoritative
— the long soak, final benchmark comparison, and final certification
decision were still in progress at this entry's own commit time):
`PHASE3C_TEST_RESULTS.md`.

### Phase 3, Increment 3B: coordinator fault matrix + resource/rotation/shutdown hardening (soak run complete — PHASE 3B INCOMPLETE, blockers remain)

Completes the coordinator-level half of Phase 3's production-hardening
scope, distinct from Increment 3A's `GroupCommitter`-level leader-panic
fix.

- **`CoordinatorFaultPoint`** (new, `src/execution/batch_coordinator.rs`):
  7 deterministically injectable points in the Dedicated Batch
  Coordinator's own batch-processing loop (`BeforeBatchFormation`,
  `AfterDrain`, `AfterAppend`, `BeforeAwaitDurable`, `AfterDurable`,
  `BeforeCompletion`, `DuringShutdown`), plus `install_coordinator_
  fault_hook`/`clear_coordinator_fault_hook` (`test-util`-gated).
- **Fixed a real completion-safety gap**, found while wiring up the
  `AfterDrain` test: `process_batch` previously only protected a
  dequeued entry with a `CompletionGuard` once the append loop
  individually reached it — a coordinator panic between dequeue and
  that point would have dropped every entry in the batch with callers
  hanging forever. Every entry now gets its guard as `process_batch`'s
  first action.
- **`queued_bytes` accounting hardened to saturating arithmetic** across
  `batch_coordinator`/`leader_drain`/`sharded_ingress`/`write_pool` —
  consistency fix, not currently exploitable, matching this project's
  own `wal_test.md` §3.7 precedent.
- **New tests**: large-payload byte accounting (deterministic barrier),
  rapid submit/shutdown cycling, frequent rotation under sustained load
  through the full production path, shutdown racing active submission,
  and 7 coordinator-panic tests (one per fault point).
- **Observability**: `BatchCoordinatorStats::{queue_capacity,
  queued_bytes_capacity}`, `GroupCommitStats::{highest_sequence,
  segment_rotations}` (new fields, zero new contention). Full metric-
  list audit and explicit gap accounting: `PHASE3B_TEST_RESULTS.md` §7.
- **`examples/soak_test.rs`** (new): sustained-workload harness against
  the production `BatchCoordinatorPool` with low-contention per-thread
  latency sampling and periodic aggregation, RSS tracking, and a
  start/mid/end drift comparison. Run for 900s (15 min) at both 100 and
  1,000 writers — write path clean at both levels (zero errors/timeouts,
  flat RSS, no degradation trend).
- **A genuine finding, not a Phase 3B-introduced defect**: the
  1,000-writer soak's own post-run recovery-verification step (not the
  write path) was killed by a real host out-of-memory condition while
  `FileWal::open_for_recovery` materialized ~85M records into one `Vec`
  — this project's existing (Phase 0) recovery API has no streaming
  variant, and its memory demand scales with WAL size. Investigated,
  confirmed correct at reduced scale by a supplementary run, documented
  precisely (`PHASE3B_ADR.md` ADR-P3B-5) rather than hidden; the harness
  now warns before repeating it. Fixing the underlying API is out of
  this phase's scope.
- Final post-hardening benchmark: no measurable regression (100w 16,133
  ops/sec, 1000w 91,208 ops/sec — both within the pre-established
  historical noise band and comfortably above target).

128/128 lib tests pass (117 + 11 new), clippy and fmt clean, zero
regressions. Full design: `PHASE3B_ARCHITECTURE.md`; failure model:
`PHASE3B_FAILURE_MODEL.md`; decisions: `PHASE3B_ADR.md`; performance:
`PHASE3B_PERFORMANCE.md`; results and final verdict: `PHASE3B_TEST_
RESULTS.md` — **PHASE 3B INCOMPLETE — BLOCKERS REMAIN** (six explicit,
named gaps against the operating brief's full scope; see that
document's §11 for the complete list and recommendation).

### Phase 3, Increment 3A: leader-failure P0 fix

Fixes a real availability gap `PHASE2B_FAILURE_MODEL.md` §3 diagnosed
but did not fix: a leader thread panicking mid-batch left
`GroupCommitter`'s `leader_active` flag (`src/wal/group_commit.rs`)
stuck `true` forever, degrading every future caller (on any
architecture — Approach A/B/C, or a direct caller) to a repeated-timeout
failure mode instead of a clean, bounded error.

- **`LeaderFailureGuard`** (new, `src/wal/group_commit.rs`): an RAII
  guard, armed the instant a caller is elected leader, disarmed only
  once `run_as_leader` returns normally. If the leader thread instead
  panics, the guard's `Drop` clears `leader_active` and poisons the
  committer during the unwind itself — mirrors `execution::common::
  CompletionGuard`'s existing pattern, not a new abstraction.
- **`PoisonReason`** (new enum, replaces `BatchState::poisoned`'s
  previous bare `io::ErrorKind`): `FsyncFailed(io::ErrorKind)` (the
  original Phase 1 poisoning path, unchanged) or `LeaderPanicked` (new).
  The `Err` a poisoned `GroupCommitter` returns now says which.
- **`GroupCommitter::is_poisoned() -> bool`** (new, public): observability
  accessor: poisoning was already externally observable via `await_
  durable`'s `Err`; this makes it queryable without a live batch.
- No change to the WAL format, `durable_through`'s semantics, sequence
  allocation, rotation, or `FileWal`'s recovery contract. No new
  dependency, no `unsafe` code. Recovery from a poisoned `GroupCommitter`
  is unchanged from Phase 1's own documented model: discard it, reopen
  the WAL directory (`FileWal::open_for_recovery` re-scans from disk),
  construct a fresh one — verified end-to-end by a new test.
- Two pre-existing `execution::leader_drain` tests, whose doc comments
  and implicit timing assumptions described the old, now-fixed behavior
  (~5s shutdown cost; a second request only failing after its full
  retry budget), were updated in place with new `< 1s` timing
  assertions locking in the fix, rather than left stale next to
  passing-but-now-misleading documentation.

Full design and the leader-failure state machine: `PHASE3_FAILURE_
MODEL.md`; decision record: `PHASE3_ADR.md` ADR-P3-1; results: `PHASE3_
TEST_RESULTS.md`; benchmarks: `PHASE3_PERFORMANCE.md` (both the
100-writer and 1,000-writer Phase 2B throughput targets remain met
after this fix, using the same Approach B/Dedicated Batch Coordinator
architecture, unchanged).

### Phase 1: Group Commit

Adds `wal::group_commit::GroupCommitter`, a leader-follower group commit
layer over the existing `FileWal`: concurrent callers share one `fsync`
per batch instead of paying one per write, with a monotone
`durable_through` watermark, bounded backpressure, explicit shutdown, and
observability counters. No change to the WAL's on-disk format, `Wal`
trait signatures, or `FileWal`'s single-writer internal model — see
`PHASE1_ARCHITECTURE.md`/`PHASE1_GROUP_COMMIT.md`/`PHASE1_ADR.md` for the
design and `PHASE1_TEST_RESULTS.md` for full results, benchmark numbers,
and the production-readiness decision (**not production ready**: the
100-writer/1,000-writer throughput targets are not met on the
development machine's disk, even after a controlled window-size sweep
(`PHASE1_ADR.md` ADR-12) substantially closed the gap by fixing the
leader's batch-window formula (`WINDOW_EMA_DIVISOR` `10 → 1`, `max_wait`
`200µs → 5ms`, plus a demand-adaptive probe protecting single-writer
latency) — 100 writers improved from ~67% to ~79% of target, 1,000
writers from ~46% to ~81%; every other gate is met).

- **`GroupCommitter`** (`src/wal/group_commit.rs`): `append`/
  `await_durable`/`append_durable`/`rotate`/`durable_through`/`stats`/
  `shutdown`, plus `with_max_pending_waiters` for explicit backpressure
  configuration. `SyncMode::GroupCommit` is no longer rejected by
  `FileWal::open_for_recovery` (it previously returned `EngineError::
  Unsupported` — see below).
- **`FsyncLatencyTracker`** (`src/wal/metrics.rs`): an `AtomicU64`-only
  EMA `fsync`-latency tracker (`new = 0.1 * sample + 0.9 * old`), driving
  the leader's batch-window sizing and a follower's timeout.
- **`FileWal::durable_seq`**: a new field, advanced only inside `sync()`/
  `rotate()` after a genuinely successful `fsync` — distinct from
  `next_seq() - 1` ("assigned," not "durable"), closing a real footgun
  where an unsynced raw `append()` before constructing a `GroupCommitter`
  could otherwise be silently treated as durable.
- **`AbortPoint`** (`src/wal/mod.rs`) expanded from 4 to 11 variants: the
  7 new ones (`BeforeLeader`, `AfterLeaderElection`, `DuringBatchWaitPre`/
  `Post`, `AfterWatermarkBeforeWake`, `DuringRotationPre`/`Post`) name
  real, reachable boundaries in `GroupCommitter`'s leader/rotation paths.
  `BeforeSync`/`AfterSync` now additionally fire from `GroupCommitter`'s
  own leader `fsync` call, not only from `FileWal::sync()`, which that
  path never calls.
- **`EngineError::Timeout`** (`src/error.rs`): a new variant for a
  follower's bounded wait expiring — required by the algorithm, distinct
  from `Io` (no I/O necessarily failed) and safe to retry.
- Seven new integration test files under `tests/group_commit/` (one per
  milestone, plus a proptest), a write-only load-test harness (`examples/
  group_commit_load_test.rs`), and a permanent append-path diagnostic
  (`examples/append_only_benchmark.rs`).
- **Window-size sweep and batch-window formula fix** (`PHASE1_ADR.md`
  ADR-12, `PHASE1_TEST_RESULTS.md` §9A/§9B): a temporary, feature-gated
  experiment (`phase1-window-experiment` Cargo feature, off by default;
  `examples/window_sweep.rs`) established empirically that the original
  leader batch-window formula (`min(200µs, EMA/10)`) was substantially
  under-tuned, not solely limited by disk `fsync` latency as first
  believed. `WINDOW_EMA_DIVISOR` changed `10 → 1`; every test/harness
  `max_wait` changed `200µs → 5ms`; a demand-adaptive probe
  (`PROBE_WINDOW = 200µs`) added so a lone, uncontended writer never
  pays for batching benefit that will never materialize — a real
  regression the naive fix introduced and this probe resolves, verified
  by re-running M1.1.

### Five follow-up fixes from external review

- **`purge_before` now attempts its directory fsync on the error path
  too**, not only on success — the "resurrection is tolerated" argument
  (every purged segment's data is already durable elsewhere, WAL Spec
  §10) is a second line of defense, not a substitute for actually trying
  the fsync whenever the directory genuinely changed. A `remove_file`
  failure and a subsequent fsync failure are now folded into one error
  that names both, rather than either one being silently dropped.
  Dropped the unconditional `eprintln!` (a library writing to stderr
  unconditionally is untestable and rude to embedders) — the combined
  error message itself now carries what the log line used to.
- **Documented, not "fixed," why recovery's torn-tail truncation doesn't
  need a directory fsync**: `set_len` + `sync_all` flush exactly the
  file's own inode metadata (its size field); no directory *entry* is
  created, renamed, or unlinked, so a directory fsync there would be a
  no-op on every mainstream filesystem and pure cost on the recovery hot
  path.
- **`write_all_at` now has a real Windows implementation** (`seek_write`
  in a retry loop matching `std::io::Write::write_all`'s own
  `Interrupted`-retry rule) instead of falling back to the portable
  seek-then-write-all default. **In verifying this by actually running
  the test on Windows** (this project's dev machine), found a genuine,
  previously-undocumented platform difference: Windows' `seek_write` on
  an ordinary synchronous handle *does* leave the file's position at the
  end of the just-written region, unlike Unix's `pwrite`, which never
  touches it. The byte content lands correctly at the correct offset on
  both platforms either way (nothing in this crate's production code
  relies on the position side-effect), but the doc comment and test
  previously claimed a cross-platform guarantee that turned out to be
  Unix-only — corrected rather than asserted from memory. See
  `WalFile::write_all_at`'s doc comment.
- Expanded `Fault::PartialThenFail`'s doc comment with its exact
  interaction with `std::io::Write::write_all`'s retry behavior and how
  it differs from `ShortWrite`, at the definition site rather than
  requiring a future test author to read `FaultInjectingIo::write`'s
  body to find out.
- Added a `NOTE` comment directly above `scan_directory`'s main loop
  making explicit that the first-corruption-stops-the-scan behavior
  (Group 3.1) is deliberate, and that a future operator-diagnostics
  function walking past corruption would need to be a *different*
  function with a *different* contract, not a loosened version of this
  loop.

### Cross-process file locking

- **`open_for_recovery` now takes an OS-level exclusive advisory lock**
  on the WAL directory (`std::fs::File::try_lock` — `flock` on Unix,
  `LockFileEx` on Windows, both via the standard library, no new
  dependency and no `unsafe`) for as long as the returned `FileWal`
  lives, closing a real gap where two concurrent `open_for_recovery`
  calls on the same directory — two separate processes, or two
  unsynchronized calls within one — could each independently scan,
  truncate torn tails, and append, silently corrupting each other's view
  of the WAL. A second attempt while the lock is held fails immediately
  (`EngineError::WalUnavailable`) — it never blocks, and it is never
  silently allowed to proceed.
- **`inspect` takes a compatible shared lock**: any number of `inspect`
  calls may run concurrently with each other, but not while a writer
  holds the exclusive lock (closing a narrower race — `FileWal::append`
  writes directly with no atomic-rename step, so `inspect` could
  otherwise observe a segment file mid-write). Takes no lock at all
  against a directory no writer has ever opened — `inspect` must never
  create anything.
- Verified with both an in-process regression test and a genuine
  cross-process test (`tests/wal_tests.rs`'s
  `cross_process_lock_prevents_concurrent_writers`, using the same
  spawn-this-test-binary-as-a-child-process technique as
  `tests/crash_consistency.rs`) — a real second OS process is rejected
  while the first is open and succeeds once it's dropped.

### WAL hardening pass (production-readiness review)

A targeted review of the WAL implementation (`src/wal/`) against
production-readiness criteria, fixing 20 issues across crash-safety,
format validation, recovery semantics, thread-safety documentation, and
test coverage, plus the cross-process locking gap above. The on-disk
format (WAL Spec §2) is unchanged — verified byte-for-byte against
`encode_segment_header(42)` and a sample `PUT` frame before and after
this pass.

#### Crash-safety / durability

- **`SegmentIo::append` now rolls back a failed write.** A partial write
  (some bytes physically land, then the write call fails) used to leave
  garbage bytes on disk past the tracked segment length, silently
  corrupting the *next* append's target region. `append` now truncates
  the file back to its pre-append length and fsyncs that truncation on
  any write failure. If the rollback itself fails, the `SegmentIo` is
  marked poisoned (`SegmentIo::is_poisoned`, `FileWal::is_poisoned`) and
  refuses all further I/O rather than write on top of an unknown-length
  file.
- **Directory fsync after segment create/remove.** `create_new_segment_file`
  and `FileWal::purge_before` now fsync the containing directory (Unix;
  documented no-op on Windows — see `file_io::fsync_dir`'s doc comment)
  so a file's *creation or removal*, not just its contents, survives a
  crash.
- **`create_new_segment_file` is now atomic w.r.t. partial failure.** A
  failure at any step after `create_new(true)` — header write, header
  fsync, or the new directory fsync — removes the partially-initialized
  file (best-effort) before returning, so a segment never exists on disk
  without a valid header.
- **`FileWal::rotate` is now atomic.** The new segment file is created
  *before* anything about the current `FileWal` state is touched; if
  sealing the old segment (`sync`) then fails, the just-created file is
  deleted and every field is left exactly as it was.
- **`FileWal::purge_before` removes segments one at a time**, updating its
  internal bookkeeping only after each individual removal succeeds, so a
  mid-list failure leaves accurate state rather than a mismatch between
  disk and memory. The directory is fsynced once after the whole batch.

#### Format validation

- `decode_segment_header` now rejects an unrecognized `format_version` and
  non-zero reserved `flags`, instead of accepting and silently
  misinterpreting a foreign/future format.
- `read_u32_le`/`read_u64_le` now return `Result` instead of relying on a
  `debug_assert!`-only precondition that disappears in release builds.
- WAL-frame encoding is now fully fallible end-to-end
  (`format::encode_frame`'s op-body closure, `write_len_prefixed`): an
  oversized field is rejected at the exact point of violation, not by
  emitting a sentinel value for a separate, later check to catch.
- `decode_wal_body`'s existing trailing-byte/wrong-length strictness for
  `PUT`/`DELETE`/`CHECKPOINT_MARKER` now has explicit regression tests.

#### Recovery semantics (behavior change — see below)

- **Recovery now stops at the first corrupted segment** and trusts
  nothing at or after it, including that segment's own records that
  preceded the corruption point within it. **This amends WAL Spec
  §6.2's original text**, which allowed scanning to continue past a
  corrupted non-last segment. See `wal::mod`'s "# Durability" section and
  `scan_directory`'s doc comment for the full rationale (fail-closed:
  once one segment's integrity is in question, a partial picture
  assembled from what comes after it is not more trustworthy for looking
  more complete). One pre-existing test asserted the old behavior by
  name and by assertion; it has been updated (not deleted) to assert the
  new contract, and a complementary test was added covering the
  "corruption is not in the first segment" case.
- `walk_segment`'s frame-extent overflow case now returns a `Corruption`
  error instead of `.expect()`-panicking on a value that is only
  provably non-overflowing for realistic input.
- Segment-ID arithmetic (`next_segment_id`, used by both `rotate` and
  fresh-segment creation) is now checked, returning `CapacityExceeded`
  instead of wrapping on overflow.

#### `inspect()` is now genuinely read-only

- `canonicalize_existing_dir` (used only by `inspect`) never creates the
  WAL directory — `canonicalize_data_dir` (used by `open_for_recovery`)
  still does.
- `scan_segment` takes a `mutate` flag; when false, every segment file is
  opened read-only, so `inspect` can run against a directory the caller
  can't write to.

#### Concurrency & API surface

- `FileWal` is documented as `Send` but deliberately not `Sync`
  (single-writer type), enforced at compile time via a
  `static_assertions::assert_not_impl_any!` check.
- `SyncMode::GroupCommit` is now rejected by `open_for_recovery`
  (`EngineError::Unsupported`) rather than silently running in
  `Immediate` mode — a caller that asks for batching is told there is
  none yet, instead of quietly getting different behavior than it
  configured.
- `wal::testing` (the `FaultInjectingIo` harness) is now gated behind
  `#[cfg(any(test, feature = "test-util"))]` instead of being
  unconditionally `pub`.

#### Performance

- `SegmentIo::append` uses a new `WalFile::write_all_at` method — a
  single `pwrite`-based syscall on Unix (via `std::fs::File`'s override,
  `std::os::unix::fs::FileExt`), falling back to the portable
  seek-then-write-all on other platforms. The doc comment claiming "one
  syscall per append" was accurate on neither platform before this
  change (it always did a separate `seek` first); it now says exactly
  what happens on each platform.
- Added `benches/append.rs` (behind the new `bench` Cargo feature),
  isolating pure `append` latency from `append_sync`'s `fsync` cost.

#### Tests & fuzz coverage

- Three new proptest cases in `wal::fuzz_tests` (≥1,000 runs each):
  random single-byte corruption anywhere outside the header, a
  partial-write-with-garbage-header-bytes scenario, and a fixed-seed,
  10,000-iteration arbitrary-byte-string panic check.
- `Fault::PartialThenFail` added to the `FaultInjectingIo` harness (bytes
  physically land, then the call fails), with regression tests for both
  the successful-rollback and poison-on-rollback-failure paths.
- New `tests/crash_consistency.rs` (behind the `test-util` feature):
  spawns this same test binary as a child process, which opens a real
  `FileWal`, appends records, and calls `std::process::abort()` at one of
  four configurable points (`FileWal::set_abort_hook`); the parent
  reopens and asserts a corruption-free, gap-free recovered prefix. See
  that file's doc comment for what this does and does not prove
  (`process::abort()` is not a power-loss simulation).
- A read-only-directory test (`#[cfg(unix)]`, `chmod 0o555`) asserting
  `inspect` succeeds where `open_for_recovery` fails with
  `PermissionDenied`.

#### Documentation

- Checked the entire `src/`/`tests/`/`benches/` tree for mojibake — found
  none (all files are valid UTF-8, `§`/`—`/`'` are correctly encoded
  throughout already). Added `scripts/check-encoding.sh`, a corrected
  version of the originally-specified check (the literal
  `grep -rP '[\x80-\xff]'` pattern matches *all* non-ASCII UTF-8 bytes,
  which would flag this codebase's own correct typography as an error).
- Added "# Safety" and "# Durability" sections to `wal::mod`'s
  module-level doc comment.

#### Fixed

- `create_new_segment_file` (rotation and initial-segment creation) now
  writes and fsyncs a new segment's header to a temporary file name and
  `fs::rename`s it into place, instead of writing the header directly at
  the segment's final name. A crash between file creation and header
  write previously left a zero-byte file at a real segment name, which
  recovery correctly (but undesirably) reported as a corrupted segment.
  Found by a deterministic `crash_consistency_across_abort_points`
  failure; see `PHASE1_TEST_RESULTS.md` §9F.2 and `PHASE1_ADR.md` ADR-15.
- Reverted the `filling_active`/`fsyncing_active` batch-pipelining split
  back to the single-phase `leader_active` design: measured to regress
  throughput on this project's development environment (Windows/NTFS)
  rather than improve it, with the change left uncommitted in the tree
  as pipelining's own regression check rather than reverted. See
  `PHASE1_TEST_RESULTS.md` §9E/§9F.1 and `PHASE1_ADR.md` ADR-14.

### Phase 2: Write Worker Pool (implemented, measured, rejected)

Adds `execution::WriteWorkerPool` (`src/execution/write_pool.rs`): a
bounded queue plus a configurable number of worker threads in front of
`GroupCommitter`, meant to separate logical client concurrency from
physical storage execution concurrency. `std`-only (`Mutex`+`Condvar`,
no new dependency), no change to `GroupCommitter`'s durability logic, no
WAL format change. Public API: `submit`/`Completion::wait`/
`wait_timeout`, `shutdown` (three-step: reject new work, drain the
queue, then finalize the underlying `GroupCommitter`), `stats`,
`into_inner`. Bounded everywhere: `queue_capacity`, `max_queued_bytes`,
`submission_timeout` (blocks then `EngineError::Timeout`, never drops a
write or blocks unboundedly), `shutdown_drain_bound`. A worker panic
resolves only its own in-flight request with an error (via an RAII
completion guard) and never loses another queued request; if every
worker terminates unexpectedly, the pool fails cleanly and drains the
remaining queue with an explicit error rather than leaving any caller
blocked forever.

**Measured and rejected as a production default**: a worker-count sweep
(1/2/4/8/16/32/64, plus a parity point at `worker_count = writer_count`)
at 100 and 1,000 logical writers found `GroupCommitter`'s batch size
architecturally capped at the worker count, not the logical writer
count — throughput regressed by one to two orders of magnitude at every
worker count meaningfully smaller than the writer count, and even at
parity (`worker_count = writer_count`) 1,000-writer throughput was 28%
below Phase 1's existing direct-thread architecture. Kept in the tree as
a documented, tested, but not-recommended artifact — see `PHASE2_TEST_
RESULTS.md`/`PHASE2_ADR.md` (ADR-P2-5) for the full evidence and
decision.

#### Fixed

- The worker pool's request-processing path originally retried nothing:
  a single-attempt `append` + `await_durable` call could surface a
  spurious `Timeout` to a caller under real concurrent load even though
  the underlying write was never lost. Fixed by retrying only `await_
  durable` (never `append` — appending exactly once means retrying the
  wait can never duplicate a record), mirroring the retry pattern Phase
  1's own test harness already established as correct
  (`tests/group_commit/support.rs::await_durable_retrying_on_timeout`).
  See `PHASE2_TEST_RESULTS.md` §13 and `PHASE2_ADR.md` ADR-P2-4.

### Phase 2B: three architectures evaluated — target achieved

Adds three further execution-layer architectures, evaluated against
Phase 2's rejected `WriteWorkerPool` and against each other:

- **`execution::leader_drain`** (Approach A, "Leader Queue Drain"): a
  worker drains the entire currently-queued backlog at once (not one
  request per loop iteration, unlike the rejected worker pool), appends
  every entry, then issues one `await_durable` for the whole batch.
  `worker_count=1` reached 16,806/95,686 ops/sec (100w/1,000w medians),
  exceeding both Phase 1 targets on the first attempt. A single-active-
  drain-leader coordination flag (`draining_active`/`DrainLeaderGuard`)
  lets `worker_count>1` provide hot-standby redundancy without
  fragmenting batches (the naive multi-worker failure mode this fixes),
  at a small cost to 100-writer margin.
- **`execution::batch_coordinator`** (Approach B, "Dedicated Batch
  Coordinator", **adopted as the recommended default**): exactly one
  coordinator thread, no worker-election machinery — structurally
  simpler than A. Reached 17,512/93,594 ops/sec (100w/1,000w medians,
  5 independent repetitions each) — the best 100-writer result of any
  architecture measured, with the least code.
- **`execution::sharded_ingress`** (Approach C, "Sharded/Per-Core
  Ingress"): `shard_count` independent ingress queues merged by one
  coordinator, evaluated once (the operating brief's own conditional
  framing — evaluate only if A and B fail, which they did not).
  15,234/96,033 ops/sec — no material improvement over B, confirming
  the single shared queue was never the bottleneck.

All three preserve the WAL format, `GroupCommitter`'s durability
contract, and crash-consistency guarantees unchanged. Zero Phase 1/
Phase 2 regressions across the full cycle. Full account: `PHASE2B_
FINAL_TEST_RESULTS.md`; design: `PHASE2B_ARCHITECTURE_A/B/C.md`;
decisions: `PHASE2B_ADR.md`.

**Target achieved**: Approach B reached a median 17,512 durable
ops/sec at 100 writers (target ≥15,000) and 93,594 at 1,000 writers
(target ≥80,000) — the first phase in this project's history to meet
the original Phase 1 throughput targets.

#### Fixed

- Every Phase 2B architecture's batch-processing path originally (in
  Approach A's first implementation) constructed each request's panic-
  safety guard (`CompletionGuard`) *after* the one shared `await_
  durable` call for a batch, rather than before — leaving every entry
  in a batch unprotected during the call most likely to observe a fault.
  A panic there hung the corresponding fault-injection test past a
  60-second timeout. Fixed by constructing every guard before the
  shared call and keeping them alive across it; Approaches B and C were
  written after this fix and used the correct ordering from the start.
  See `PHASE2B_FAILURE_MODEL.md` §2 and `PHASE2B_ADR.md` ADR-P2B-3.

#### Discovered (pre-existing Phase 1 behavior, not a regression)

- A leader/coordinator thread that panics specifically while inside the
  leader `fsync` call leaves `GroupCommitter`'s own `leader_active` flag
  (`src/wal/group_commit.rs`) permanently stuck — every architecture's
  worker/standby redundancy is powerless against this specific failure,
  since the underlying committer itself becomes globally wedged, not
  just the one thread that died. Verified the system still fails safely
  (bounded, no hang, no false acknowledgment) under this condition.
  See `PHASE2B_FAILURE_MODEL.md` §3.
