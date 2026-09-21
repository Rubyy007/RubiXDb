# Compaction — Increment 1: Deterministic Core Implementation Results

**Status:** `ADR-COMPACTION-001` implemented as the deterministic core
operation only. **No automatic trigger, no background thread — by
design** (`ADR-COMPACTION-001` Decision 14). **COMPACTION PRODUCTION
READY = NO.**

**Date:** 2026-09-21

This document records what was implemented, tested, and found this
increment. It does not re-derive the design — see `PHASE_COMPACTION_
ARCHITECTURE_REPORT.md` and `PHASE_COMPACTION_ADR.md` for that; this
is the implementation and evidence record, following the same
append-only, cite-don't-repeat convention as every prior increment's
own results document.

## 1. Scope actually delivered

- `LsmEngine::compact_once(&self) -> Result<Option<(SstableMeta,
  CompactionStats)>>` (`pub(crate)`, `src/lsm/mod.rs`): one full,
  synchronous compaction cycle — capture, merge, write, Manifest
  transition, live-list splice, deferred-delete sweep.
- `LsmEngine::should_compact(&self) -> bool` (`pub(crate)`): the pure
  trigger *check* only.
- `LsmConfig.compaction_trigger_count: usize` (default 4, per LSM
  Engine Spec §5.1) — the field the original spec named but the real
  struct never had until now.
- `crate::compaction::merge`/`retain_versions`/`CompactionMergeIter`/
  `MergeStats`/`CompactionStats` (`src/compaction/mod.rs`): the
  engine-agnostic k-way merge and retention algorithm.
- `sstable::write_from_sorted_records` (`src/sstable/writer.rs`): the
  generalized, streaming SSTable writer entry point (`ADR-COMPACTION-
  001` Decision 3) — `write_from_memtable` is now a thin adapter over
  the same shared construction core, unmodified in behavior (§3 below).
- `CompactionFaultPoint`/`install_compaction_fault_hook`/
  `CompactionIoFaultHook`/`install_compaction_io_fault_hook`
  (`src/lsm/mod.rs`): deterministic fault injection, modeled directly
  on `FlushFaultPoint`/`FlushIoFaultHook`.
- `set_storage_state_for_test` (`src/lsm/mod.rs`, `#[cfg(any(test,
  feature = "test-util"))]`): lets a test force `StoragePressure`/
  `StorageFull` deterministically, mirroring `set_flush_delay_for_
  test`'s own established convention.

**Explicitly not delivered, by design, per the brief's own repeated
instruction**: any background thread, automatic scheduling, periodic
trigger loop, or timer. `should_compact()`/`compact_once()` have **no
production caller** — every call site in this diff is a test.

## 2. A real correctness refinement found during implementation

`retain_versions`'s actual, implemented behavior is **more
conservative** than `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §11's
own worked table stated for two of its five rows (`@1`, `@2`) — that
table's `drop` verdicts for intermediate versions were only correct
under an unstated "exactly one live snapshot" assumption.
`oldest_live_snapshot_seq()` exposes only the *minimum* live snapshot
`seq`, never the full set (`ADR-COMPACTION-001` Decision 5's own,
unchanged, API choice) — so the actual, safe algorithm retains
**every** version from the floor (the oldest live snapshot's own
answer) through the newest, not merely the floor and the newest. This
was caught by the implementation's own unit tests
(`src/compaction/tests.rs::retention_snapshot_at_1_keeps_everything_
from_the_floor_forward`/`retention_snapshot_at_2_keeps_everything_
from_the_floor_forward`) failing against the originally-planned
assertions, traced to its root cause, and corrected — in both the test
expectations and, via an added erratum note (the original table left
unedited, per this project's append-only convention), in `PHASE_
COMPACTION_ARCHITECTURE_REPORT.md` §11 itself. A further boundary case
neither the original table nor the first implementation attempt
considered — a live snapshot pinned *strictly between* two versions,
not at either one's own `seq` — was added as its own test
(`retention_snapshot_strictly_between_two_versions_keeps_the_floor`)
and confirmed correct by the same algorithm. **The underlying ADR
decision (use only `oldest_live_snapshot_seq()`, no new Snapshot API,
correctness over reduction ratio) is unchanged** — only the worked
table's specific numbers were imprecise, not the design.

## 3. Streaming SSTable writer — behavior-preserving refactor

`write_from_sorted_records<I: Iterator<Item = Result<(Vec<u8>, u64,
RecordValue)>>>` shares 100% of the block/bloom/index/footer
construction and atomic `.tmp`-then-rename publication discipline with
`write_from_memtable`, which is now a thin adapter (builds the same
iterator shape from `MemTable::range`, computes the same `entry_count`
hint, delegates). **Verified, not assumed, behavior-preserving**:
`write_from_memtable_and_write_from_sorted_records_produce_byte_
identical_output` (`src/sstable/tests.rs`) constructs identical logical
records both ways and asserts **byte-for-byte identical** output files
(not just logically equivalent) — `min_seq`/`max_seq`/`record_count`
match exactly, and every `get_versioned` call agrees between the two
resulting tables. The full pre-existing SSTable writer/reader test
suite (43 tests before this increment) re-ran unmodified and green.
`write_from_sorted_records_propagates_the_first_error_and_aborts`
confirms the fail-closed contract (a corrupted upstream record aborts
the build immediately, no partial `.sst` is ever published).

`entry_count_hint` for a compaction's output is the sum of its inputs'
`record_count()` — an upper bound on true surviving records (compaction
only ever drops records, never adds), safe for the bloom filter's
sizing per `ADR-COMPACTION-001` Decision 3's own reasoning.

## 4. Merge algorithm — bounded memory, persistent cursors, Increment 6 reused directly

`CompactionMergeIter` holds one persistent, owned-`Arc`
`SsTableRangeCursor` (Increment 6's own type, reused unchanged) per
input — never reconstructed per key, consuming the exact same
bounded-memory, no-redundant-block-re-read design the Read Engine's
own range scans already use. At most one winning key's worth of output
records is buffered (`pending: VecDeque`) — never the whole merge
result, never a `Vec<all surviving records>`, never a temporary
`MemTable`. A corrupted input block aborts the whole merge immediately
(checked before computing the next winning key, not after), matching
`ADR-RE-001` §7's established fail-closed contract, verified by
`merge_propagates_corruption_from_an_input_and_stops`.

## 5. Test results

### 5.1 Module-level (`src/compaction/tests.rs`) — 13 tests

Worked truth table (corrected, per §2 above) as literal test cases;
the newly-added strictly-between-two-versions boundary case; single/
two/many-table merge mechanics (overlapping keys, disjoint ranges,
cross-table tombstone suppression); corruption propagation.

### 5.2 Engine-level (`src/lsm/tests.rs::compaction_tests`) — 13 tests

- **Trigger gating / single-table / no-op** (brief §25/§26):
  `should_compact_reports_true_only_at_or_above_trigger_count`,
  `compact_once_returns_none_when_no_live_sstables`, `compact_once_
  below_trigger_count_is_a_no_op`, `compact_once_with_a_single_table_
  reapplies_retention_correctly` (single-table compaction follows the
  *exact same* general algorithm — no special-cased no-op — and
  correctly drops superseded versions even within one table),
  `compact_once_merges_many_tables_and_updates_manifest_and_live_list`
  (end-to-end: Manifest ADD/REMOVE + live-list splice, verified by
  reopening and checking the reconciled live set, not just in-memory
  state).
- **Correctness differential** (brief §23): `compaction_preserves_
  logical_reads_for_every_prior_snapshot_seq` — 2,000 randomized
  put/delete/snapshot operations against an independent reference
  model (never the production merge algorithm as its own oracle, per
  `ADR-RE-001` §17's established principle), `get_as_of`/`contains`/
  `range()` compared against the model **and** against the
  pre-compaction engine's own reads, for every live snapshot seq plus
  "now", both before and after running compaction repeatedly.
- **Property-based** (brief §24): `compaction_tests::property::
  compaction_never_changes_logical_reads` — 48 `proptest` cases
  (`proptest = "=1.11.0"`, unchanged, no new dependency), random keys/
  puts/deletes/snapshot-take/snapshot-drop/compact operations
  interleaved, asserting logical equivalence, sorted range output, no
  duplicate logical key, `contains`/`get_as_of` agreement.
- **Crash windows** (brief §19): `compaction_crash_windows_leave_a_
  correct_recoverable_state` — all 6 `CompactionFaultPoint`s
  (`BeforeOutputWrite`, `BeforeManifestAdd`, `AfterManifestAdd`,
  `DuringRemoveSequence`, `AfterAllRemoves`, `BeforePhysicalDelete`),
  each a real, deterministically-injected panic, caught, followed by
  an actual `shutdown()` + `drop` + fresh `open()` (a real restart, not
  an in-process retry) — every logical read compared before vs. after,
  no `.sst.tmp` ever survives recovery.
- **Orphan recovery** (brief §18, mandatory): `orphan_recovery_after_
  crash_between_remove_durability_and_physical_deletion` — specifically
  exercises `reconcile_sstables_with_manifest`'s "removed-but-
  undeleted orphan" branch, previously flagged as having **zero**
  direct test coverage (`PHASE_COMPACTION_ARCHITECTURE_REPORT.md`
  §1/§7). Confirms input files are still physically present immediately
  after the injected crash (proving the test actually reaches the
  scenario), then confirms the post-restart sweep removes them and the
  Manifest live set is correct.
- **Concurrent flush** (brief §16): `concurrent_flush_publishing_
  during_compaction_capture_is_not_lost` — a flush publishing a new
  table is never lost, whether captured by this cycle or left for the
  next one.
- **Concurrent readers** (brief §17): `concurrent_readers_during_
  compaction_never_see_partial_or_wrong_state` — 4 reader threads
  running `get`/`contains`/`range_scan` continuously through 2
  compaction cycles; zero errors, zero unsorted output, zero duplicate
  logical keys.
- **Windows read-safety, real positional-read-after-unlink** (brief
  §15, explicitly required — the earlier standalone probe in the
  architecture-audit phase only confirmed `remove_file` succeeds while
  a handle is open, never a subsequent read): `long_lived_reader_
  survives_compaction_unlinking_its_table_and_cleanup_eventually_
  happens` — starts a real range scan, pulls one row, runs a real
  compaction that retires (and may physically unlink) the scan's own
  source tables, then **continues consuming the same, already-open
  iterator** and asserts every remaining row is read without error.
- **Storage pressure** (brief §20): `compaction_defers_while_storage_
  full_and_never_mutates_storage_state` — `StorageFull`/`StoragePressure`
  both cause `compact_once` to return `Ok(None)` without touching the
  live list; `storage_state()`/`storage_pressure_events()` are
  confirmed byte-for-byte unchanged by the call; compaction resumes
  normally once `Healthy`.

### 5.3 A real, fixed race in the test fixture helper itself

The shared fixture builder (`put_and_wait_for_sstable_count`) initially
raced against the background flush thread under heavy parallel-test
scheduling load (5 of 334 tests flaked in one full-suite run, 0 flaked
when the same tests ran in isolation) — traced to the fact that a
flush thread briefly has a table live in **both** `sstables` and
`immutables` simultaneously mid-publish (`sstables`' own insert happens
before the corresponding `immutables` removal, `src/lsm/mod.rs`'s
flush-thread body), so neither `sstable_count()` alone (can lag,
causing the loop to over-issue writes that then land in one flush
burst) nor `sstable_count() + immutable_count()` (can transiently
double-count during that same window, causing the loop to under-issue
writes) is race-free on its own. Fixed by pacing each write against a
fully-settled `immutable_count()==0` before issuing the next one --
serializing fixture-building against the flush thread entirely, making
the final live count exact regardless of scheduling delay. Two full
consecutive `cargo test --lib` runs after the fix: 334/334 both times.
This was a test-infrastructure bug, not a production-code bug --
`compact_once`'s own behavior was never in question, only how
reliably the *test fixture* reached its intended starting state under
heavy parallel load.

## 6. Full regression gate

```
cargo fmt --check                                          clean
cargo clippy --all-targets --all-features -- -D warnings   clean
cargo test --lib                                            334/334 (debug)
cargo test --release --lib                                  334/334 (release)
cargo check --all-targets --all-features                    clean
wal_tests                                                    12/12
crash_consistency --features test-util                       2/2
pathological_recovery_matrix (debug)                          9/9
pathological_recovery_matrix (release)                        9/9
```

334 = the pre-existing 306 (Read Engine certification baseline) + 2
new writer differential tests (`src/sstable/tests.rs`) + 13 new
module-level compaction tests + 13 new engine-level compaction tests.
No existing test's assertions were altered — only two pre-existing
raw `LsmEngine { .. }` struct-literal constructions
(`src/lsm/tests.rs`, both already existed for unrelated WAL-fault-
injection tests) needed the three new struct fields added, a purely
mechanical, non-behavioral addition required by the type system.

## 7. Protected-contract audit (post-implementation, not just pre-implementation)

```
git diff --stat -- src/wal/ src/error.rs src/manifest/
```

Empty. **Zero changes** to WAL, `EngineError`, or Manifest format
definitions. `git diff --stat -- Cargo.toml Cargo.lock` is also empty
— **zero new dependencies**. `git diff` across every changed file
contains **zero occurrences of `unsafe`**. `get`/`get_as_of`/
`contains`/`range`/`range_scan`/`Snapshot`/`SnapshotRegistry`/
`oldest_live_snapshot_seq`/`ReadStats`/`ReadView` — every one of their
own existing tests re-ran unmodified and green (§6). No public method
signature changed; `compaction_trigger_count` is a new, additive
`LsmConfig` field with a `Default` value, not a breaking change (every
existing `LsmConfig { .. }` construction in this codebase already uses
`..LsmConfig::default()`/`..Default::default()`, confirmed compiling
unmodified).

## 8. Diff scope (exact)

```
src/compaction/mod.rs   +317  (merge, retention, CompactionStats, MergeStats)
src/compaction/tests.rs +new  (13 module-level tests)
src/lsm/mod.rs           +374 (compact_once, should_compact, fault hooks,
                                pending-delete sweep, compaction_trigger_count,
                                set_storage_state_for_test)
src/lsm/tests.rs         +987 (13 engine-level tests + 2 pre-existing struct
                                literals updated for new fields)
src/sstable/mod.rs         +4 (export write_from_sorted_records, pub(crate)
                                test_support for cross-module test reuse)
src/sstable/tests.rs     +111 (2 writer differential tests)
src/sstable/writer.rs    +109 (write_from_sorted_records + write_from_memtable
                                refactored to a thin adapter over it)
```

No `src/wal/`, `src/error.rs`, `src/manifest/`, or `Cargo.toml`/
`Cargo.lock` change, per §7.

## 9. Resource behavior (qualitative, not a dedicated benchmark this increment)

Not separately benchmarked (brief's own Phase 1 preamble: no
benchmarks this increment). Structurally bounded by design (§4) and
exercised, not just asserted, by the concurrent-readers/long-lived-
reader tests (§5.2) running real compaction cycles against real,
on-disk fixtures without any observed leak, hang, or unbounded growth
across the full test run. A dedicated resource benchmark (RSS/handle/
thread tracking across repeated compaction cycles, mirroring `read_
engine_bench`'s own `cursor_resource_check` precedent) is listed as
future work, not performed here.

## 10. What remains, explicitly not done this increment

- No automatic trigger/background thread (Decision 14 — deferred to a
  separate, future, human-approved increment).
- No dedicated performance benchmark of compaction itself (throughput,
  duration vs. input size, resource peaks under sustained load).
- No long-duration integrated soak exercising compaction under
  sustained concurrent write/read load (the existing Read Engine
  soaks, Increment 4/7, predate Compaction's existence and never
  exercised it).
- No Compaction entry in `PHASE_READ_ENGINE_CERTIFICATION.md`-style
  final certification — this increment's own results are the evidence
  a future certification effort would draw from, not a certification
  itself.

**COMPACTION IMPLEMENTATION = INCREMENT 1 COMPLETE.**
**COMPACTION PRODUCTION READY = NO.**
**WRITE ENGINE = PRODUCTION READY** (unchanged, protected, re-verified §7).
**READ ENGINE = PRODUCTION READY** (unchanged, protected, re-verified §7).
