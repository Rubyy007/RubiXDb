# RubiXDB — LSM Read Engine Final Certification

**Certification date:** 2026-09-21

**Certified commit:** `22be3e4` (`perf(read): persist range source
cursors`), the exact production implementation commit. `HEAD` at
certification time is `3e13f64`, one commit ahead — inspected and
characterized below (§ Protected Dependencies) as test/example-only,
zero production source changes, so it does not alter what is being
certified.

This document does not re-derive evidence. Every claim below
references the historical document/section/test/commit where that
evidence was actually produced. Nothing here overwrites, edits, or
re-runs a past measurement to make a different point — see the
Evidence Index for the full source list.

---

## Executive Summary

The RubiXDB LSM Read Engine — `get`/`get_as_of`/`contains`/`range`/
`range_scan`/`Snapshot`, built across Increments 1–7 on top of the
already-certified Write Engine — is certified **PRODUCTION READY**
for its tested scope: a single, non-partitioned LSM engine instance,
no Compaction, no Router, no Replication. Correctness held at zero
mismatches across two independent 4-hour production-profile soaks
(pre- and post-optimization), a full corruption/I/O-failure matrix,
20/20 real-read-verified crash/recovery cycles, and 306 unit/
integration tests. The one performance bottleneck this phase's own
investigation found (`RangeScanIter` re-peeking SSTable sources once
per key on overlapping-key workloads) was traced, reproduced, fixed
(`ADR-RE-002` Option A — persistent owned-`Arc` source cursors, zero
`unsafe`, zero new dependency), and the fix was independently
re-validated in a fresh, full 4-hour soak, not just a benchmark. No
Write Engine, WAL, or Manifest behavior was touched anywhere in this
phase (§ Protected Dependencies — audited, not assumed).

Known, explicitly non-blocking limitations (§ Known Limitations): no
Compaction exists yet (point-lookup/range read amplification still
scales with live SSTable count — an architectural non-goal of this
phase, not a defect); the "no memory leak" finding rests on source-
level ownership analysis plus two converging soak observations, not an
external memory profiler (none was available in this environment) —
stated honestly, not overclaimed.

## Scope

**Certified**: the single-engine, non-partitioned LSM Read Engine as
it exists in this codebase today — `LsmEngine::get`/`get_as_of`/
`contains`/`range`/`range_scan`/`snapshot`/`oldest_live_snapshot_seq`,
their `ReadStats` observability, and their interaction with the
already-certified Write Engine (WAL, Group Commit, Dedicated Batch
Coordinator, MemTable, Manifest, checkpoint, WAL purge, flush/SSTable
publication).

**Not certified, not implied, and explicitly out of scope**:
Compaction, Router, Replication, or the larger multi-engine
partitioned RubiXDB architecture described in `RubixDB-Architecture-
Specification-v1.0.md` — `PHASE_READ_ENGINE_ARCHITECTURE_REPORT.md`
itself already states that broader architecture does not exist in
this codebase. "LSM Read Engine production ready" is the valid,
bounded claim this document makes; full RubiXDB production readiness
is **not** implied or claimed.

## Certified Commit

```
22be3e4  perf(read): persist range source cursors        <- certified implementation
d44afe9  perf(read): document range scan bottleneck and resource findings
047e670  feat(read): add contains and read performance baseline
a8febb8  feat(read): implement production range scan
b9840ea  feat(read): add snapshot registry and read consistency foundations
7d02554  feat(write-engine): ENOSPC storage-pressure handling, certification  <- Write Engine baseline
```

`git log --oneline -15` (run at certification time) and `git status`
confirm this history and a clean-except-documentation working tree
(§ Protected Dependencies has the full audit).

## Protected Dependencies

**The unexpected commit, characterized explicitly, not silently
ignored**: `3e13f64` ("commit by me") sits on `HEAD`, one commit ahead
of the certified `22be3e4`, authored outside this session. Inspected
(`git diff 22be3e4 3e13f64 --stat`): touches only `examples/lsm_crash_
cycle_test.rs` (117 lines, real-read-verification extension) and adds
`examples/read_write_soak_test.rs` (1,069 lines, new file — the
Increment 4/7 soak harness itself). **Zero changes to any `src/`
file, `Cargo.toml`, or `Cargo.lock`.** Test/example-only. Does not
change, and is not part of, what this document certifies.

**Full protected-path audit, run at certification time, not assumed**:

```
git diff 7d02554 HEAD --stat -- src/wal/ src/manifest/ src/error.rs \
  src/execution/batch_coordinator/
```

produced **zero output** — not one line changed in any of those paths
across the entire Read Engine phase (`b9840ea` through `3e13f64`,
Increments 1–7 combined). The one adjacent hit when searching
`src/lsm/mod.rs` for `storage_state`/`StoragePressure`/`StorageFull`/
`capacity_pressure_events` diffs was a single **doc-comment** line
(the new `ReadStatCounters` type's comment naming the existing
convention it mirrors) — zero functional change, confirmed by reading
the diff context directly. `git status`/`git diff --stat` (current
working tree) also confirm zero changes to any protected path.

**Conclusion**: WAL format, Group Commit, Dedicated Batch Coordinator,
write durability, checkpoint, WAL purge, Manifest publication
semantics, and `StoragePressure`/`StorageFull` backpressure logic are
byte-for-byte unchanged since the Write Engine's own certification
(`7d02554`, `PHASE_WRITE_ENGINE_CERTIFICATION.md`). That certification
remains valid and is not re-derived or re-litigated here.

---

## Certification Matrix

Every row: Requirement / Evidence / Result / Reference. No row is
marked PASS without a specific, named test, benchmark, or soak
citation.

| # | Requirement | Evidence | Result | Reference |
|---|---|---|---|---|
| 1 | Point lookup (`get`) | hit/miss/multi-SSTable latency+correctness, `Increment 3` baseline; corruption/I/O-failure fail-closed; unchanged by Increment 6 (diff: `reader.rs`'s `get_versioned` untouched) | **PASS** | `PHASE_READ_ENGINE_PERFORMANCE.md` §1; `lsm::tests::data_block_corruption_is_detected_lazily_at_read_time_not_at_open`; `get_as_of_and_range_scan_when_the_underlying_file_shrinks_mid_lifetime_fail_closed_with_io_error` |
| 2 | `get_as_of` | same coverage as #1, plus tombstone/historical-seq resolution | **PASS** | `PHASE_READ_ENGINE_PERFORMANCE.md` §2; `lsm::tests::range_scan_multiple_versions_of_a_key_resolve_to_the_newest_visible` (shared resolution logic) |
| 3 | `range_scan`/`range` | lazy k-way merge, bounded memory, real cursor persistence (Increment 6) | **PASS** | `ADR-RE-001` §3/§4; `ADR-RE-002`; `src/lsm/mod.rs` `RangeScanIter` |
| 4 | Range bounds (Included/Excluded/Unbounded) | dedicated tests, all bound shapes, empty-range edge cases | **PASS** | `lsm::tests::range_scan_included_bounds`, `range_scan_excluded_bounds`, `range_scan_unbounded_start_or_end`, `range_scan_empty_cases` |
| 5 | Version resolution | worked-example + property tests against an independent reference model | **PASS** | `lsm::tests::range_scan_version_resolution_matches_adr_worked_examples`; `range_scan_property_tests::lsm_engine_range_scan_matches_independent_reference_model` |
| 6 | Tombstones | suppression, delete-then-recreate, three-way `contains`/`get_as_of` invariant | **PASS** | `lsm::tests::range_scan_suppresses_tombstoned_keys_entirely`, `range_scan_delete_then_recreate_shows_only_the_recreated_value`, `contains_returns_false_for_a_visible_tombstone` |
| 7 | Snapshots (`Snapshot`, historical reads) | `seq()` stability, `get_as_of`/`range_scan` at a pinned snapshot under concurrent writes | **PASS** | `lsm::tests::snapshot_reads_remain_stable_across_later_writes`, `range_scan_snapshot_observes_historical_state_and_registry_stays_correct`; Increment 7 soak: 678,708/678,708 aged range checks, 0 mismatches |
| 8 | Snapshot registration/deregistration | `SnapshotRegistry` refcounted multiset, `Drop`-based release, source-verified | **PASS** | `src/lsm/mod.rs:266-342`; `lsm::tests::one_snapshot_registers_and_releases`, `dropping_snapshots_newest_first_reports_correctly_at_every_step`, `dropping_snapshots_oldest_first_reports_correctly_at_every_step`, `all_snapshots_dropped_leaves_no_live_snapshot`, `two_snapshots_at_different_sequence_numbers_report_the_older_as_oldest`, `two_snapshots_at_the_same_sequence_are_counted_independently` |
| 9 | Concurrent flush visibility | deterministic `FlushFaultPoint`-based test, re-verified post-Increment-6 | **PASS** | `lsm::tests::range_scan_during_concurrent_flush_sees_a_coherent_snapshot` |
| 10 | SSTable visibility authority | flush publishes to `sstables` before removing from `immutables` — established Increment 3, unchanged | **PASS** | `PHASE_READ_ENGINE_PERFORMANCE.md` Increment 3 §10; `point_lookup_during_the_sstable_published_immutable_not_yet_removed_window_never_misses` |
| 11 | Corruption handling | footer/index/bloom/data-block corruption, all fail closed with exact `EngineError::Corruption` | **PASS** | corruption matrix table, `PHASE_READ_ENGINE_PERFORMANCE.md`; `sstable::tests::corruption_*` (8 tests) |
| 12 | I/O error propagation | genuine (non-simulated) file-shrink technique, exact `EngineError::Io` | **PASS** | `get_as_of_and_range_scan_when_the_underlying_file_shrinks_mid_lifetime_fail_closed_with_io_error`, `contains_when_the_underlying_file_shrinks_mid_lifetime_fails_closed_with_io_error` |
| 13 | Fail-closed iteration | exactly one `Err`, iterator ends, no partial success, re-verified against the new persistent-cursor design | **PASS** | `range_scan_across_a_corrupted_data_block_fails_closed_and_ends`; `corruption_injected_mid_session_between_reads_is_caught_on_the_very_next_read` |
| 14 | `contains` | bloom+index+block reuse, three-way invariant vs. `get_as_of`, no measurable perf difference (honestly reported, not a win claimed) | **PASS** | `PHASE_READ_ENGINE_PERFORMANCE.md` §3 |
| 15 | `ReadStats` (`read_requests`/`hits`/`misses`/`bloom_negatives`/`blocks_read`/`sstables_consulted`) | all six counters defined, tested; `sstables_consulted` **semantics intentionally revised** for range scans in Increment 6 (once per live SSTable per scan, not once per key) — documented, not silently changed, with a dedicated regression test | **PASS (documented semantic change)** | `PHASE_READ_ENGINE_PERFORMANCE.md` "ReadStats counter semantics"; `lsm::tests::read_stats_counts_range_scan_calls_and_sstable_consultation_correctly`; `range_scan_source_cursor_persists_across_keys_instead_of_reconstructing_per_key` |
| 16 | Memory behavior | monotonic growth structurally explained (per-SSTable index/bloom, source-verified no static cache/no engine-side scan registry); non-monotonic swings not fully profiler-confirmed, honestly flagged | **PASS (with stated limitation — see Known Limitations)** | `PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md` §1/§5; `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` §3 (R² 0.698→0.984 after the fix) |
| 17 | File-handle behavior | ~1 handle per live SSTable, flat across reads/scans, both soaks | **PASS** | Increment 3 §8; `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` §3; `read_engine_bench cursor_resource_check` (400 scans, handle delta = 0) |
| 18 | Thread behavior | stable during load, clean shutdown to baseline, both soaks | **PASS** | `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` §3; `cursor_resource_check` (thread delta = 0) |
| 19 | Range-scan performance | traced bottleneck, fixed, before/after benchmark **and** real 4-hour soak, matched-count comparison | **PASS (scoped — see §6 below)** | `PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md` §4; `ADR-RE-002`; `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` §4 |
| 20 | Point-read performance | linear-with-SSTable-count scaling established Increment 3, reconfirmed unregressed post-Increment-6 and in both soaks | **PASS** | `PHASE_READ_ENGINE_PERFORMANCE.md` §1/§4; `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` §6 |
| 21 | Read amplification | bloom-negative-dominated point-lookup scaling (Increment 3); `blocks_read`-based range amplification improved 4.7x-8.05x post-fix | **PASS (with known architectural limitation — see Known Limitations)** | `PHASE_READ_ENGINE_PERFORMANCE.md` §4; `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` §5 |
| 22 | Crash safety | real kills at randomized delays, real post-recovery reads against exact expected values | **PASS** | `lsm_crash_cycle_test` 20/20, `reads_verified_ok=true` every cycle, `total_read_mismatches=0` — `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` §7 |
| 23 | Recovery | `RecoveryStats`, no sequence gaps, Manifest/checkpoint consistency, both soaks + crash cycles | **PASS** | Increment 4 + Increment 7 soak `recovery_ok=true`; `lsm_crash_cycle_test` |
| 24 | Integrated write/read workload | real full stack, no mocks, concurrent writers+readers, two independent 4-hour runs | **PASS** | Increment 4 (`RESULT=PASS`), Increment 7 (`RESULT=PASS`) |
| 25 | Long-duration stability | two independent 4-hour soaks, zero mismatches each | **PASS** | `temp/read_write_soak_output.log` (Increment 4); `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` (Increment 7) |
| 26 | Storage behavior | `storage_state=Healthy` in 100% of samples across both soaks, zero backpressure events, free disk never approached the floor | **PASS** | Increment 4 + Increment 7 `HEALTH` samples (all `storage_state=Healthy`); `storage_pressure_events=0` both runs |
| 27 | Regression suite | full suite, re-run at every increment boundary and again at certification time | **PASS** | §Final Regression Gate below |
| 28 | Code quality / formatting | `cargo fmt --check`/`cargo clippy -D warnings` clean at every increment and at certification time | **PASS** | §Final Regression Gate below |
| 29 | Dependency hygiene | zero `unsafe` introduced, zero new `Cargo.toml`/`Cargo.lock` entries across Increments 5-7 | **PASS** | `ADR-RE-002` §9; `git diff` `Cargo.toml`/`Cargo.lock` empty at every increment |
| 30 | Protected Write Engine integrity | zero changes to `src/wal/`, `src/manifest/`, `src/error.rs`, batch coordinator, StoragePressure logic, across the entire Read Engine phase | **PASS** | § Protected Dependencies above (audited at certification time) |

**30/30 PASS. 0 FAIL. 0 mandatory OPEN.** (Rows 15, 16, 19, 21 carry
explicitly documented, non-blocking caveats — see Known Limitations;
none is a correctness defect or an unmet mandatory gate.)

---

## Correctness Evidence

Zero `in_run_mismatches` and zero `post_recovery_mismatches` across
**two** independent 4-hour soaks (Increment 4: 4,582,352 reads,
261,455 range scans; Increment 7: 6,116,654 reads, 678,708 range
scans — every one of those 678,708 checked against an independent
reference model at an aged snapshot). 306/306 unit/integration tests
pass at the certified commit, including a 64-case property test
against an independent reference model (`range_scan_property_tests::
lsm_engine_range_scan_matches_independent_reference_model`) and the
crash-cycle/soak reference models, which are **never** the production
merge algorithm itself — a distinct, independently-implemented oracle
in every case (`ADR-RE-001`/§17's own explicit requirement, honored
throughout).

## Performance Evidence

See Certification Matrix rows 19-21 and `PHASE_READ_ENGINE_
RESOURCE_INVESTIGATION.md`/`ADR-RE-002`/`PHASE_READ_ENGINE_INCREMENT7_
SOAK.md` in full. Headline, reproduced at certification time (§Final
Performance Validation below): on a small, deliberately overlapping
20-key/300-SSTable synthetic workload, `range_large`-equivalent p50
improved 3.20x-3.69x (Increment 6 benchmark) and, independently, on
the real 8-writer/16-reader 4-hour production soak, `range_large` p50
improved 3.26x-3.89x at matched SSTable counts (Increment 7) — two
independent measurements converging on the same magnitude. **Tested
workload and SSTable-count range, stated explicitly, not implied
universal**: overlapping/small-cardinality keyspaces (this project's
own established "realistic" endurance profile) at 20-614 live
SSTables. No claim is made about disjoint-key workloads beyond what
Increment 4's own `memory_scaling` section already measured (linear-
ish, not superlinear, on that different workload shape) or about
SSTable counts outside the tested range.

## Resource Evidence

RSS growth is monotonic-with-SSTable-count (R²=0.984, Increment 7,
vs. 0.698 pre-fix) and structurally explained (per-SSTable index/
bloom-filter metadata, source-verified never freed pre-Compaction —
`PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md` §1.1/§5). File handles
track live SSTable count at ~0.99-1.00/table in both soaks and in a
dedicated 400-scan create/consume/drop resource check (handle delta =
0, thread delta = 0). No engine-side registry of live range scans
exists (`grep`-confirmed) — a completed scan's `Arc<SsTable>` clones
and decoded-block buffers release on ordinary `Drop`, verified both by
source review and by the zero-delta resource check.

## Crash/Recovery Evidence

`lsm_crash_cycle_test` (real external-process kills at randomized
delays, 20 cycles, 6 writers, seed 20260920): 20/20 successful, every
cycle performs real `get`/`contains`/`range` verification against an
exact expected value (`verify_reads_after_recovery`) — never merely
"open() returned Ok" — `total_read_mismatches=0`.
`PHASE_READ_ENGINE_INCREMENT7_SOAK.md` §7.

## Corruption Evidence

Full matrix (footer/index/bloom checksum corruption, data-block
corruption detected lazily at read time, genuine non-simulated I/O
failure) across `get`/`get_as_of`/`range`/`range_scan`/`contains`/
`open()` — every path fails closed with the exact `EngineError`
variant, re-verified passing at the certified commit (§Final
Regression Gate). Mid-session corruption (no restart, already-open
engine) also covered (`corruption_injected_mid_session_between_reads_
is_caught_on_the_very_next_read`). `PHASE_READ_ENGINE_PERFORMANCE.md`
corruption matrix table has the full per-path detail.

## Snapshot Evidence

`SnapshotRegistry` (`src/lsm/mod.rs:266-342`) is a correctly
refcounted `Mutex<BTreeMap<seq, count>>`; `Snapshot::Drop` always
releases. `snapshots_live=50` staying constant across both 4-hour
soaks (115 and 116 samples respectively, no exceptions) was traced, by
source review of the test harness itself (`SnapshotPool::prune(50)`,
`read_write_soak_test.rs`), to be that harness's own deliberate pool
cap — **not** an engine-side leak. No Compaction exists yet, so
`oldest_live_snapshot_seq()`'s stated purpose ("the mechanism a future
Compaction phase will need") is implemented and tested but has no
consumer in this codebase yet — stated plainly, not overclaimed as
Compaction support.

## Observability Evidence

All six `ReadStats` counters (`read_requests`, `read_hits`, `read_
misses`, `bloom_negatives`, `blocks_read`, `sstables_consulted`) have
precise, documented definitions (`PHASE_READ_ENGINE_PERFORMANCE.md`
"ReadStats counter semantics") and dedicated tests. **`sstables_
consulted`'s definition for range scans changed in Increment 6** —
before: once per key drawn from a source (a side effect of the old
per-key-reconstruction design); after: once per live SSTable actually
captured by the scan's `ReadView`, matching point lookups' own
existing convention. This is documented in three independent places
(`ADR-RE-002`, `PHASE_READ_ENGINE_PERFORMANCE.md`'s Increment 6
section, and a dedicated regression test asserting the *exact* new
count) — raw `sstables_consulted` numbers from before and after
Increment 6 are **not** directly comparable without this context;
`blocks_read` (counting point unchanged throughout) is the fair
metric for isolating the mechanism's effect, and is what §Performance
Evidence above cites for that purpose.

## Known Limitations

Stated honestly, per this phase's own explicit instruction not to
classify a known architectural non-goal as a defect:

- **No Compaction exists yet.** Point-lookup and range read
  amplification both scale with live SSTable count (Increment 3 §4,
  reconfirmed at 5,000+ SSTables in Increment 4's `memory_scaling`
  section, and still true post-Increment-6 — the fix removed the
  *per-key* re-peek multiplier, not the underlying *per-SSTable-count*
  linear cost, which a future Compaction phase is the documented,
  intended remedy for). This is this phase's own stated non-goal, not
  a regression or a defect.
- **Range performance depends on key-overlap shape.** The 3.2x-3.9x
  improvement is measured on this project's own established
  "realistic" (small-cardinality, heavily-overwritten) workload. A
  disjoint-key workload (Increment 4's own `memory_scaling` section)
  already showed near-linear range-scan scaling *before* this fix too
  — the fix's benefit is concentrated on, and was specifically
  targeted at, the overlapping-key case, not a universal multiplier
  claimed for every workload shape.
- **Cold OS-page-cache measurement was not available from user mode**
  on this Windows development host (Increment 3's own stated
  methodology caveat, unchanged since) — every latency number in this
  phase reflects first-access-after-fixture-build or steady-state
  access, never a true post-reboot cold cache.
- **SSTable metadata remains resident in memory while a table is
  live** (bloom filter + sparse index, by design — `SsTable::open`'s
  own doc comment) — expected, monotonic, and unavoidable until
  Compaction exists to remove superseded tables; not itself a leak
  (Certification Matrix row 16).
- **Memory-leak absence is not profiler-confirmed.** The "no leak
  found" conclusion (Matrix row 16) rests on (a) source-level ownership
  tracing showing no static cache, no engine-side scan registry, and
  correct `Drop`-based release of every `Arc`/cursor, and (b) two
  independent 4-hour soaks showing RSS growth fully explained by
  monotonic per-SSTable metadata, with the post-fix soak showing a
  *tighter*, not looser, fit. No external memory profiler (e.g.
  Valgrind/heaptrack/ETW heap trace) was available in this environment
  to independently confirm this from the outside. Stated as a genuine
  limitation of the evidence, not glossed over.

## Open Items

None mandatory-blocking. For completeness, work explicitly out of
scope and not started by this certification, per every increment's own
repeated instruction: Compaction, Router, Replication, any cache/mmap/
prefetch/parallel-read mechanism, a secondary index, or a change to the
on-disk SSTable/WAL/Manifest format.

---

## Final Regression Gate (run at certification time, from the certified commit)

```
cargo fmt --check                                          clean
cargo clippy --all-targets --all-features -- -D warnings   clean
cargo test --lib                                            306/306
cargo test --release --lib                                  306/306
cargo check --all-targets --all-features                    clean
wal_tests                                                    12/12
crash_consistency --features test-util                       2/2
pathological_recovery_matrix (debug)                          9/9
pathological_recovery_matrix (release)                        9/9
```

No unrelated pre-existing flake observed. No test assertion was
altered to produce this result.

## Final Performance Validation (bounded, at certification time — not a new soak)

Re-ran the existing `overlap_repro` benchmark once (`n=7` reps/
checkpoint, same parameters as Increment 6/7's own recorded runs) to
confirm the certified commit reproduces the recorded evidence, not
merely cites it:

| SSTables | p50 (us) | p95=p99=max (us) | blocks_read | sstables_consulted |
|---:|---:|---:|---:|---:|
| 20  | 1,442.7  | 1,619.0  | 140   | 20  |
| 50  | 4,008.3  | 4,421.2  | 350   | 50  |
| 100 | 8,145.7  | 8,879.4  | 700   | 100 |
| 200 | 16,267.4 | 17,088.3 | 1,400 | 200 |
| 300 | 24,747.2 | 27,946.2 | 2,100 | 300 |

`blocks_read`/`sstables_consulted` are **exactly** reproduced
(deterministic, matching Increment 6's recorded 140/350/700/1400/2100
and 20/50/100/200/300 precisely). p50 latencies are within ~5-10% of
Increment 6's own recorded run-to-run variance (e.g. 1,442.7us vs.
1,617.5us at 20 SSTables) — ordinary system noise, not a regression;
`sstables_consulted/sstable=1.000` at every checkpoint confirms the
persistent-cursor mechanism is intact at the certified commit.

---

## Final Decision

**Mandatory gates: 30/30 PASS. 0 FAIL. 0 mandatory OPEN.**

# READ ENGINE PRODUCTION READY = YES

**Exact scope of this decision: the RubiXDB single-engine, non-
partitioned LSM Read Engine**, as certified above. This decision does
**not** certify Compaction, Router, Replication, or the larger
partitioned RubiXDB architecture — none of those exist in this
codebase yet (`PHASE_READ_ENGINE_ARCHITECTURE_REPORT.md`'s own scope
note) — and does not constitute a claim of full RubiXDB production
readiness.

---

## Evidence Index

- `PHASE_READ_ENGINE_ARCHITECTURE_REPORT.md` — pre-implementation
  read-only audit; scope note (no multi-engine/partitioned
  architecture exists in this codebase).
- `PHASE_READ_ENGINE_ADR.md` (`ADR-RE-001`) — the 13 foundational Read
  Engine design decisions (ReadView, snapshot model, k-way merge,
  fail-closed corruption contract, `ReadStats` counters, `contains()`).
- `PHASE_READ_ENGINE_RANGE_PERFORMANCE_ADR.md` (`ADR-RE-002`) —
  range-scan cursor performance decision; status **Implemented**, §9
  has the implementation record.
- `PHASE_READ_ENGINE_PERFORMANCE.md` — dated, append-only benchmark
  history, Increments 3, 4, 5, 6, 7 (each a separate dated section).
- `PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md` — Increment 5's
  memory + range-performance root-cause investigation.
- `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` — the fresh, post-
  optimization 4-hour integrated soak.
- `temp/read_write_soak_output.log` — Increment 4's raw pre-
  optimization soak log, preserved unmodified.
- `temp/read_write_soak_increment7_20260920_213353.log` — Increment
  7's raw post-optimization soak log, preserved unmodified.
- `PROGRESS.md` — chronological narrative, every increment's own
  dated entry, 2026-09-14 through 2026-09-21.
- `CHANGELOG.md` — `[Unreleased]` section, every increment's own
  entry.
- `PHASE_WRITE_ENGINE_CERTIFICATION.md` — the Write Engine's own,
  separate, still-valid certification (protected, not re-derived
  here).
- `src/lsm/mod.rs`, `src/sstable/reader.rs`, `src/sstable/mod.rs`,
  `src/lsm/tests.rs` — the certified implementation and its test
  suite.
- `examples/read_engine_bench.rs` — `overlap_repro`/`memory_scaling`/
  `cursor_resource_check` benchmark sections.
- `examples/read_write_soak_test.rs`, `examples/lsm_crash_cycle_test.rs`
  — the endurance and crash/recovery harnesses.
