# RubiXDB — LSM Compaction Final Certification

**Certification date:** 2026-09-22

**Certified commits:** `b0b30f9` (Increment 1, deterministic core),
`c2536d7` (Increment 2, automatic trigger + execution integration),
`57a68d9` (Increment 3, performance/resource/endurance validation —
classified explicitly in § Protected Dependencies below: not purely
test/example-only, it carries one small, additive, non-behavioral
production change). `HEAD` at certification time is `57a68d9` — the
same commit, nothing further has been added since Increment 3 closed.

This document does not re-derive evidence. Every claim below
references the historical document/section/test/commit where that
evidence was actually produced, or a fresh command run at
certification time (labeled as such). Nothing here overwrites, edits,
re-runs a past measurement to make a different point, or smooths over
disclosed variance — see the Evidence Index for the full source list.

---

## Executive Summary

RubiXDB's Compaction subsystem — size-tiered, full-merge, automatic,
built across Increments 1–3 on top of the already-certified Write
Engine and Read Engine — is certified **PRODUCTION READY** for its
tested scope: a single, non-partitioned LSM engine instance's
Compaction, no Router, no Replication, no leveled/partial selection.
Correctness held at **zero mismatches** across every differential/
property/concurrency/endurance check run against it, including a real
4-hour production-profile soak (~14.9M writes, ~2.6M deletes, 3.3
billion point reads, 9.87M range scans, 193 real automatic compaction
cycles, 0 in-run mismatches, 0 post-recovery mismatches across all
20,000 independently-tracked keys). Crash safety was proven through
**38/38** real external-process kills against the live automatic
worker — 18 precisely targeted at each of the 6 `CompactionFaultPoint`
windows via a stdout-marker technique, plus 20 broader random-delay
cycles. Resource behavior is bounded by live input-table count, not
cumulative data volume processed (RSS +9.7% while compacted-through
volume grew ~668x in the scaling benchmark; handles and threads
returned **exactly** to their pre-open baseline after shutdown across
100 repeated cycles). No Write Engine, WAL, Manifest format, or Read
Engine public-API behavior was touched anywhere across all three
increments (§ Protected Dependencies — audited fresh at certification
time, not assumed).

Known, explicitly non-blocking items (§ Known Limitations): the
certified strategy is v1's own deliberate scope cut (size-tiered,
full-merge, all live tables, one output — no leveled or partial
selection, per the authoritative spec, not a gap); real,
disclosed-not-hidden run-to-run performance variance at 64+ input
tables; RSS growth across long/high-cycle-count runs is real, bounded,
and structurally explained, but is **not** literally zero (stated
plainly below, not overclaimed); no external memory profiler was
available in this environment (the same limitation the Read Engine's
own certification already disclosed).

## Scope

**Certified**: RubiXDB's single-engine, non-partitioned LSM Compaction
subsystem as it exists in this codebase today — the deterministic
core merge/retention operation (`crate::compaction::merge`/`retain_
versions`), its `LsmEngine`-level integration (`compact_once_impl`,
`should_compact`, `compact_once`), the automatic background worker
(`spawn_compaction_thread`, gated by `LsmConfig.compaction_auto_
trigger`), its interaction with the already-certified Write Engine
(WAL, Group Commit, Dedicated Batch Coordinator, MemTable, Manifest,
checkpoint, WAL purge, flush/SSTable publication, `StoragePressure`/
`StorageFull`) and the already-certified Read Engine (`get`/`get_as_
of`/`contains`/`range`/`range_scan`/`Snapshot`/`oldest_live_snapshot_
seq`), and the new, purely additive `CompactionMetrics`/`LsmEngine::
compaction_metrics()` observability surface added in Increment 3.

**Certified strategy, exactly**: size-tiered, full-merge — compact
**all** currently-live SSTables into exactly **one** new SSTable
whenever `compaction_trigger_count` (default 4) is reached. This is
the v1 strategy the authoritative specification itself selected as "a
documented scope cut, not an oversight" (`RubixDB-LSM-Engine-
Specification-v1.0.md` §5.1, `ADR-COMPACTION-001` Decision 1).

**Not certified, not implied, and explicitly out of scope**: leveled
compaction, partial/selective compaction, any future measurement-
driven strategy change (`ADR-COMPACTION-001` Decision 1 explicitly
defers this to Architecture Spec §17's own ablation protocol), Router,
Replication, or the larger multi-engine partitioned RubiXDB
architecture. "LSM Compaction production ready" is the valid, bounded
claim this document makes; full RubiXDB production readiness is
**not** implied or claimed.

## Certified Commits

```
57a68d9  test(compaction): validate production endurance and resource behavior   <- Increment 3 (see classification below)
c2536d7  feat(compaction): integrate automatic compaction trigger               <- Increment 2
b0b30f9  feat(compaction): implement deterministic full-merge compaction core   <- Increment 1
d94064d  docs(read): certify production-ready read engine                      <- Read Engine certification (baseline)
3e13f64  commit by me                                                          <- pre-Compaction, test/example-only (Read Engine cert's own audit)
22be3e4  perf(read): persist range source cursors                              <- Read Engine certified implementation
7d02554  feat(write-engine): ENOSPC storage-pressure handling, certification    <- Write Engine baseline
```

`git log --oneline -15`, `git status`, and `git rev-parse HEAD` (run
at certification time) confirm this history and a clean working tree
— no uncommitted changes, nothing ahead of `57a68d9` on this branch's
own history beyond what this certification itself adds.

## Architecture Decisions

Every decision below is inherited, not re-derived here — see the ADR
itself for full Decision/Reason/Alternatives/Safety/Performance/Tests-
required structure on each:

- **`ADR-COMPACTION-001`** (17 original decisions + Amendment 1, 6
  further decisions A1–A6): strategy (size-tiered full-merge, Decision
  1), input selection (`ReadView`-style capture, Decision 2), output
  formation (generalized streaming writer, `write_from_sorted_
  records`, Decision 3), version/tombstone retention (Decision 4/6),
  snapshot interaction (`oldest_live_snapshot_seq()`, no new API,
  Decision 5), Manifest transition (`AddSstable` then `RemoveSstable`
  ×N, Decision 7), crash protocol (existing Manifest replay + orphan
  sweep, Decision 8), physical deletion (`Arc`-refcount-gated,
  deferred not blocking, Decision 9), concurrency model (brief
  write-lock splice, no global lock, Decision 10), storage-pressure
  behavior (observe only, never mutate, Decision 11), resource limits
  (bounded by input-table count, Decision 12), API surface
  (`pub(crate)`, no public API without a real caller, Decision 13),
  trigger deferred-then-delivered (Decision 14, resolved by Amendment
  1), fault injection (`CompactionFaultPoint`/`CompactionIoFaultHook`,
  Decision 15), observability (`CompactionStats`, Decision 16), test
  strategy (Decision 17). Amendment 1: execution model (background
  worker, not synchronous post-flush, §A1), re-entrancy
  (`CompactionRunGuard`, one `AtomicBool`, §A2), shutdown contract
  (§A3 — a real bug, `try_send` vs. blocking `send`, found and fixed
  during this same increment), retry/backoff (dual wake source, no new
  config field, §A4), `compaction_auto_trigger` default reversed from
  `true` to `false` after two concrete test failures (§A5), and a
  genuinely unbounded test-design hazard found and generalized (§A6).

No decision in this ADR was revisited, weakened, or silently changed
by Increment 3 — Increment 3 added observability
(`CompactionMetrics`) and validation only.

## Protected Dependencies

**Full protected-path audit, run at certification time, not
assumed:**

```
git diff d94064d HEAD --stat -- src/wal/ src/manifest/ src/error.rs \
  src/execution/batch_coordinator/
```

produced **zero output** — not one line changed in any of those paths
across the entire Compaction phase (`b0b30f9` through `57a68d9`,
Increments 1–3 combined), measured from the Read Engine's own
certification commit (`d94064d`) forward.

```
git diff d94064d HEAD -- src/lsm/mod.rs | grep -E \
  "^-.*pub fn (get|get_as_of|contains|range\(|range_scan|snapshot\(|oldest_live_snapshot_seq)\b"
```

produced **zero output** — no Read Engine public method signature was
removed or altered anywhere in the Compaction phase.

```
git diff d94064d HEAD --stat -- Cargo.toml Cargo.lock
git diff d94064d HEAD -- src/ | grep -c "^\+.*unsafe"
git diff d94064d HEAD --stat -- RUBIC_SSTABLE_FORMAT_SPECIFICATION.md \
  RUBIC_MANIFEST_FORMAT_SPECIFICATION.md RubixDB-WAL-Specification-v1.0.md
```

all confirm: **zero new dependency**, **zero `unsafe`** introduced,
**zero on-disk format specification changed** (SSTable, Manifest, and
WAL format documents byte-identical) across the whole phase.

**Increment 3 (`57a68d9`), classified explicitly, not silently
included as test-only**: `git show --stat 57a68d9` shows 10 files —
8 are documentation/examples (`CHANGELOG.md`, `PROGRESS.md`,
`PHASE_COMPACTION_PERFORMANCE.md`, `PHASE_COMPACTION_INCREMENT3_
ENDURANCE.md`, and the four new `examples/compaction_*.rs` harnesses),
and 2 touch production source: `src/lsm/mod.rs` (+139/-0) and
`src/lsm/tests.rs` (+85/-2, the test file). The `src/lsm/mod.rs`
change is exactly one new, purely additive observability surface —
`CompactionMetrics` (a struct) and `LsmEngine::compaction_metrics()`
(one new public accessor) — added because `compact_once`/`should_
compact` remain `pub(crate)` by deliberate, still-honored `ADR-
COMPACTION-001` Decision 13, leaving no other way for an external
harness to observe per-cycle `CompactionStats` from the real automatic
worker. It is updated only on a successful `compact_once_impl` cycle,
consulted by no correctness or trigger decision (the same
non-load-bearing status `ReadStats` already has), changes no existing
function signature, and removes no existing field. Verified directly
by reading the diff, not assumed from the commit message.

**Conclusion**: WAL format, Group Commit, Dedicated Batch Coordinator,
write durability, checkpoint, WAL purge, Manifest publication
semantics, `StoragePressure`/`StorageFull` backpressure logic, and
every certified Read Engine public method's own signature and
semantics are byte-for-byte unchanged since the Write Engine's
(`7d02554`) and Read Engine's (`d94064d`/`22be3e4`) own certifications.
Neither certification is re-derived or re-litigated here; both remain
valid.

---

## Certification Matrix

Every row: Requirement / Evidence / Result / Reference. No row is
marked PASS merely because a related test exists — each cites the
specific test, benchmark, or soak that actually exercised it.

| # | Requirement | Evidence | Result | Reference |
|---|---|---|---|---|
| 1 | Compaction strategy | size-tiered, full-merge, all live tables, one output — matches the authoritative spec's own v1 scope cut exactly, no leveled/partial claim | **PASS** | `ADR-COMPACTION-001` Decision 1; `RubixDB-LSM-Engine-Specification-v1.0.md` §5.1 |
| 2 | Trigger correctness | `compaction_trigger_count` (default 4) gates automatic firing exactly at/above threshold, never below | **PASS** | `should_compact_reports_true_only_at_or_above_trigger_count`; `auto_trigger_fires_at_and_above_threshold_never_below` (3/4/5/9-table cases) |
| 3 | Input selection | brief-read-lock `ReadView`-style capture; a concurrent flush's new table is never lost, captured this cycle or eligible next | **PASS** | `ADR-COMPACTION-001` Decision 2; `concurrent_flush_publishing_during_compaction_capture_is_not_lost` |
| 4 | Full-merge correctness | k-way merge (`CompactionMergeIter`/`SsTableRangeCursor`) over single/two/many tables, overlapping and disjoint ranges, cross-table tombstone suppression | **PASS** | `single_table_merge_reproduces_its_own_live_content`, `two_table_merge_overlapping_keys_newer_table_wins`, `two_table_merge_disjoint_ranges`, `many_table_merge_tombstone_across_tables_suppresses_older_put` |
| 5 | Output SSTable correctness | `write_from_sorted_records` produces **byte-for-byte identical** output to `write_from_memtable` for equivalent logical records — not just logically equivalent | **PASS** | `write_from_memtable_and_write_from_sorted_records_produce_byte_identical_output` (`src/sstable/tests.rs`) |
| 6 | Streaming / bounded memory | persistent, owned-`Arc` cursors, at most one winning key's records buffered, never a temporary `MemTable` or full-result `Vec` | **PASS** | `ADR-COMPACTION-001` Decision 3/12; RSS scaling: +9.7% (4,200→4,608 KB) while cumulative compacted-through data grew ~668x and cycle count grew 30x — `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §6.1 |
| 7 | Version retention | the exact retention rule (superseded + snapshot-safe), full worked truth table as literal tests, a corrected erratum documented not hidden | **PASS** | 7 `retention_*` tests (`src/compaction/tests.rs`); `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §11 erratum |
| 8 | Snapshot safety | no live snapshot ever loses a version it could still validly observe, across real automatic cycles | **PASS** | `auto_trigger_never_drops_a_live_snapshot_or_changes_its_reads`; snapshot endurance: 30 rounds, up to 3 overlapping live snapshots, 39 real compaction cycles, **0 mismatches** — `PHASE_COMPACTION_PERFORMANCE.md` §6 |
| 9 | Tombstone safety | tombstones follow the identical retention rule (no special case); no resurrection; delete/recreate correct | **PASS** | Retention truth-table tests (row 7); tombstone/version endurance: 60 rounds × 6-op PUT/PUT/DELETE/PUT/DELETE/PUT histories, 12 real compaction cycles, **0 mismatches** — `PHASE_COMPACTION_PERFORMANCE.md` §6 |
| 10 | Manifest ADD ordering | `AddSstable` durably appended+fsynced before any `RemoveSstable` | **PASS** | `ADR-COMPACTION-001` Decision 7; `compact_once_merges_many_tables_and_updates_manifest_and_live_list`; `BeforeManifestAdd`/`AfterManifestAdd` crash-window coverage below |
| 11 | Manifest REMOVE ordering | one `RemoveSstable` per input, each durably appended+fsynced, after the output `AddSstable` | **PASS** | Same as row 10; `DuringRemoveSequence`/`AfterAllRemoves` crash-window coverage below |
| 12 | Crash consistency | full crash-window matrix, in-process **and** real external-process kills through the live automatic worker | **PASS** | `compaction_crash_windows_leave_a_correct_recoverable_state` (6 fault points, in-process); `auto_trigger_crash_mid_compaction_recovers_correctly_after_restart`; **38/38** real external `Child::kill()` cycles (18 targeted, all 6 `CompactionFaultPoint`s + 20 random-delay) — `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §4 |
| 13 | Orphan recovery | the previously-zero-coverage "`RemoveSstable` durable, physical file still present" recovery branch, now exercised and proven | **PASS** | `orphan_recovery_after_crash_between_remove_durability_and_physical_deletion`; `reconcile_sstables_with_manifest`'s orphan branch (`src/lsm/mod.rs:1781-1785`) |
| 14 | Physical deletion safety | `Arc`-strong-count-gated, deferred not blocking; empirically verified safe on this project's actual Windows target | **PASS** | `ADR-COMPACTION-001` Decision 9; `auto_trigger_eventually_retries_a_deferred_physical_deletion`; Windows `remove_file`-while-open empirical probe, `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §6 |
| 15 | Long-lived reader safety | a **real, in-progress range scan** continues correctly after Compaction unlinks its own source table mid-scan — not merely "`remove_file` succeeded" | **PASS** | `long_lived_reader_survives_compaction_unlinking_its_table_and_cleanup_eventually_happens` (`src/lsm/tests.rs`) |
| 16 | Concurrent flush safety | flush's own splice and Compaction's own splice correctly serialize via the shared `sstables` lock; no lost publish | **PASS** | Row 3's own reference; `ADR-COMPACTION-001` Decision 10 |
| 17 | Concurrent reader safety | `get`/`contains`/`range_scan` readers alongside real compaction cycles, in-process and at production scale | **PASS** | `concurrent_readers_during_compaction_never_see_partial_or_wrong_state`; concurrent read/write/compaction: ~2.37M mixed ops, 14 real cycles, **0 mismatches** — `PHASE_COMPACTION_PERFORMANCE.md` §5 |
| 18 | Automatic worker re-entrancy | exactly one concurrent compaction cycle ever, regardless of caller (manual, worker trigger, worker catch-up) | **PASS** | `compaction_run_guard_permits_exactly_one_concurrent_holder` (16 threads × 500 attempts, max observed concurrent holder = 1) |
| 19 | Shutdown behavior | in-progress cycle always completes, never aborted; no worker leak; no join deadlock; across all 4 timing classes | **PASS** | `shutdown_lets_an_in_progress_automatic_compaction_finish_before_returning`; shutdown endurance: 8/8 cycles (idle, pending-notification, mid-compaction, fully-settled), max 41.08ms, no hang — `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §3 |
| 20 | Retry behavior | no busy loop, bounded fallback-tick cadence, eventual success, log-once-per-attempt | **PASS** | `auto_trigger_retries_after_a_failed_attempt_via_the_next_fallback_tick`; Increment 3: 3 injected failures, exactly 3 log lines, success in 343.6ms — `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §2 |
| 21 | `StoragePressure` behavior | Compaction defers/skips triggering, never mutates `storage_state`/`storage_pressure_events` | **PASS** | `auto_trigger_defers_while_storage_full_and_resumes_once_healthy`; Increment 3: 0 cycles under forced `StoragePressure` — `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §1 |
| 22 | `StorageFull` behavior | same conservative defer; automatic resume once `Healthy`, with no manual intervention | **PASS** | `compaction_defers_while_storage_full_and_never_mutates_storage_state`; Increment 3: 0 cycles under forced `StorageFull`, resumed=true after recovery — `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §1 |
| 23 | Storage budget | measured on-disk peak vs. the ADR's own theoretical `input_bytes+output_bytes` model | **PASS** | Increment 2 baseline (4/8/16/32/64 tables, exact match); Increment 3: 64 and 256 tables, **0.00% delta** both — `PHASE_COMPACTION_PERFORMANCE.md` §3 |
| 24 | Resource bounds | peak memory/handles bounded by live **input table count**, not cumulative data volume ever processed | **PASS** | `ADR-COMPACTION-001` Decision 12; RSS scaling row 6 above; 4-hour soak: live SSTable count never exceeded 3 despite ~14.9M writes |
| 25 | RSS behavior | actual measured trend, stated honestly — real, bounded growth, not zero | **PASS** | Scaling: 4,200→4,608 KB over 150 cycles (+9.7%). 4-hour soak: 59,944–70,080 KB band (~17%), not a monotonic climb — back half oscillates rather than continuing to grow. **Not claimed zero.** `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §6.1/§7.2 |
| 26 | Handle behavior | no growth across repeated cycles; returns **exactly** to pre-open baseline after shutdown | **PASS** | 79 (baseline) → 87 → 86 → 86 (10/50/100 cycles) → 79 (post-shutdown, exact match). 4-hour soak: 102–106 band, flat — `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §6.2/§7.2 |
| 27 | Thread behavior | same — no growth, returns exactly to baseline | **PASS** | 4 (baseline) → 7 → 7 → 7 → 4 (post-shutdown, exact match). 4-hour soak: 28–33 band (33 an early-startup transient, 28 steady-state) — `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §6.2/§7.2 |
| 28 | Performance characterization | 4–256 input-table sweep, 9 overlap×value-size shapes, all raw reps preserved, real variance disclosed | **PASS (scoped — see Known Limitations)** | `PHASE_COMPACTION_PERFORMANCE.md` §1/§2 |
| 29 | Long-duration endurance | a real 4-hour production-profile soak with automatic Compaction active throughout | **PASS** | 8 writers, 16 readers, ~14.9M writes, ~2.6M deletes, 3.3B point reads, 9.87M range scans, 193 real compaction cycles, **0 in-run mismatches** — `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §7 |
| 30 | Post-recovery correctness | a real shutdown + reopen after the full 4-hour run, every tracked key individually verified | **PASS** | recovery_ms=493.2, **20,000/20,000** keys verified, **0 post-recovery mismatches** — `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §7.3 |
| 31 | Corruption / fail-closed behavior | corrupt input aborts the merge immediately (never silently skips/drops); injected I/O failure never produces partial output or a corrupted Manifest | **PASS** | `merge_propagates_corruption_from_an_input_and_stops`; failure-retry evidence (row 20) — no partial output/corrupted Manifest across 3 injected failures |
| 32 | Differential / reference-model validation | production algorithm never its own oracle, throughout | **PASS** | `compaction_preserves_logical_reads_for_every_prior_snapshot_seq` (2,000 ops); `auto_trigger_bounded_production_like_integration_matches_reference_model` (400 ops); concurrent/snapshot/tombstone endurance (rows 17/8/9); 4-hour soak (3.3B reads checked against an independent model) |
| 33 | Property-based validation | randomized keys/puts/deletes/snapshots/compactions vs. logical-equivalence invariants | **PASS** | `compaction_never_changes_logical_reads` — 48 `proptest` cases, `proptest = "=1.11.0"` (unchanged, pinned) |
| 34 | Regression suite | full suite, re-run at every increment boundary and again at certification time on an idle machine | **PASS** | §Final Regression Gate below |
| 35 | Code quality / dependency hygiene | `fmt`/`clippy -D warnings` clean; zero `unsafe`; zero new dependency; zero format-spec change | **PASS** | §Protected Dependencies above; §Final Regression Gate below |

**35/35 PASS. 0 FAIL. 0 mandatory OPEN.** (Rows 25/26/27/28 report
real, disclosed measured behavior rather than an idealized "zero
growth"/"no variance" claim — none is a correctness defect or an
unmet mandatory gate; see Known Limitations.)

---

## Correctness Evidence

Zero mismatches across every differential/concurrency/endurance
surface Compaction was checked against, at every scale: 13
module-level retention/merge tests, 14 engine-level tests (including a
2,000-op differential run and a 48-case property test), 12 automatic-
trigger integration tests (including a 400-op bounded reference-model
run), a dedicated concurrent read/write/compaction check (~2.37M mixed
ops, 14 real cycles), snapshot endurance (39 cycles), tombstone/
version endurance (12 cycles), and the 4-hour production soak (3.3
billion point reads, 9.87M range scans, 193 real cycles) — **39
dedicated Compaction tests plus 4 dedicated Increment 3 harnesses, 0
mismatches anywhere.** Every differential/property/endurance check
compares against an independently implemented reference model — never
the production merge algorithm as its own oracle (`ADR-RE-001` §17's
own established principle, honored throughout every increment).

## Snapshot Evidence

`oldest_live_snapshot_seq()` (already-certified `SnapshotRegistry`,
Read Engine row 8) is consulted exactly once per compaction cycle, at
the retention decision point (`ADR-COMPACTION-001` Decision 5) — no
new Snapshot API was introduced. The retention rule's own worked truth
table (7 literal test cases, `src/compaction/tests.rs`) plus a
corrected erratum (documented, not silently rewritten —
`PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §11) establish: a version is
retained if it is the newest surviving version of its key, **or** some
live snapshot's `as_of_seq` could still require it. Snapshot endurance
(Increment 3) exercised this at production concurrency: up to 3
overlapping live snapshots, created and released across 30 rounds and
39 real automatic compaction cycles, with every live snapshot's own
historical read checked against an independently tracked per-key
version history at every round — **0 mismatches**. After every
snapshot was released, `oldest_live_snapshot_seq()` correctly returned
`None`, and a further write+compaction cycle proceeded normally
(nothing left permanently pinned).

## Manifest Evidence

Compaction's Manifest sequence is exactly `AddSstable` (durable) then
one `RemoveSstable` per input (each durable) — no new `ManifestEdit`
variant was introduced (`ADR-COMPACTION-001` Decision 7); the
`AddSstable`/`RemoveSstable` types and `ManifestState::apply`'s
idempotent handling of both are unchanged, already-certified Write
Engine mechanism. `reconcile_sstables_with_manifest`'s directory-vs-
Manifest reconciliation is the sole recovery authority — a physically
present `.sst` file is never trusted over what the Manifest's `live_
sstables`/`ever_added` state says, in every code path and every crash
window tested (rows 10–13 above). No Manifest format change (§Protected
Dependencies).

## Crash/Recovery Evidence

**In-process, deterministic fault injection** (6 `CompactionFaultPoint`
windows: `BeforeOutputWrite`, `BeforeManifestAdd`, `AfterManifestAdd`,
`DuringRemoveSequence`, `AfterAllRemoves`, `BeforePhysicalDelete`) —
`compaction_crash_windows_leave_a_correct_recoverable_state`, each a
real injected panic caught, followed by a real `shutdown()`+`drop`+
fresh `open()` restart (not an in-process retry), every logical read
compared before vs. after, no `.sst.tmp` ever surviving recovery.

**Real, external-process crash cycles through the live automatic
worker** (Increment 3, `compaction_crash_cycle_test.rs`): a targeted
sweep precisely killed the child process while execution was inside
each of the same 6 `CompactionFaultPoint` windows (via a stdout-marker
technique, not a random delay), 3 reps each — **18/18 successful**;
plus a broader, untargeted random-delay sweep — 10/10 then 20/20
successful (**38/38 total**). Every cycle verified: `LsmEngine::open()`
never errors; no `.sst.tmp` survives; every physically-present `.sst`
file's id is in the Manifest's live set and vice versa (no orphaned
live file, no incorrectly-deleted input, no missing output);
`get`/`contains`/`range` never error and return sorted, duplicate-free
output; `highest_sequence`/`durable_through`/`checkpoint_seq` never
regress within a directory's own history. A real test-harness bug (not
a production defect) was found and fixed while building this sweep —
see `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` §4.1 for the full,
undisguised account.

## Orphan Recovery Evidence

The one crash-recovery branch flagged in the pre-implementation audit
as "structurally complete but zero test coverage"
(`reconcile_sstables_with_manifest`'s "removed-but-undeleted orphan"
branch, `src/lsm/mod.rs:1781-1785`) is now directly, explicitly
exercised: `orphan_recovery_after_crash_between_remove_durability_and_
physical_deletion` durably records a `RemoveSstable` edit, leaves the
corresponding physical file present (confirmed present immediately
after the injected crash, proving the test genuinely reaches this
scenario), crashes, reopens, and confirms the post-restart sweep
removes the orphan and the resulting Manifest live set is correct.

## Reader Safety Evidence

`long_lived_reader_survives_compaction_unlinking_its_table_and_
cleanup_eventually_happens` starts a **real range scan**, pulls one
row, runs a **real compaction** that retires (and, once the scan's own
`Arc<SsTable>` clone is the only thing keeping the file's strong count
above 1, physically unlinks) the scan's own source table, then
**continues consuming the same, already-open iterator** — every
remaining row reads correctly. This is the substantive evidence for
reader safety on this project's actual Windows target platform, not
merely the earlier, narrower architecture-audit-phase finding that
`remove_file` succeeds while a second handle is open (`PHASE_
COMPACTION_ARCHITECTURE_REPORT.md` §6) — that finding is real and
cited, but this test is what actually proves a positional read
survives the unlink, end to end.

## Storage-Pressure Evidence

Both `StoragePressure` and `StorageFull` cause `compact_once`/the
automatic worker to defer/skip a cycle entirely (`Ok(None)`, zero
attempt), confirmed both in-process (`compaction_defers_while_storage_
full_and_never_mutates_storage_state`, `auto_trigger_defers_while_
storage_full_and_resumes_once_healthy`) and at Increment 3's dedicated
endurance scale (forced `StorageFull` then `StoragePressure`, **0**
compaction cycles in either state, `storage_state()`/`storage_
pressure_events()` confirmed byte-for-byte unchanged by the call).
Compaction resumed **automatically**, with no manual intervention,
once storage recovered to `Healthy` through the certified Write
Engine's own path — matching `ADR-COMPACTION-001` Decision 11 exactly.
The Write Engine's own `StoragePressure` contract (only a successful
flush's own path may clear `StorageFull`, `ADR-WE-SP-001`) is
unmodified (§Protected Dependencies).

## Resource Evidence

Peak memory is bounded by **live input table count**, not cumulative
data volume ever compacted (`ADR-COMPACTION-001` Decision 12) — RSS
grew only 9.7% (4,200→4,608 KB) while cumulative compacted-through
data grew ~668x and cycle count grew 30x in the dedicated scaling
benchmark. Handles and threads showed **no growth** across 10→50→100
repeated automatic cycles and returned **exactly** to the pre-open
process baseline after `shutdown()` (handles 79→87→86→86→79, threads
4→7→7→7→4) — no leaked worker thread, no leaked file handle
attributable to the engine instance. At full 4-hour production scale,
RSS stayed in a 59,944–70,080 KB band (not a monotonic climb — the
back half of the run oscillates rather than continuing upward),
handles in 102–106, threads in 28–33 (33 an early-startup transient),
and live SSTable count never exceeded 3 despite ~14.9M writes and 193
real compaction cycles. **Stated plainly, per this project's own
"measure, don't assume" discipline: this is real, bounded growth, not
zero growth** — see Known Limitations for the full, honest framing.

## Performance Evidence

See Certification Matrix row 28 and `PHASE_COMPACTION_PERFORMANCE.md`
in full. Headline: storage-budget peak matches the ADR's own
theoretical `input_bytes+output_bytes` model **exactly** (0.00% delta)
at both 64 and 256 input tables. Merge duration scales roughly with
total input data volume (Decision 1's accepted v1 cost model), but
**real run-to-run variance is disclosed, not smoothed**: e.g. 128
input tables measured 461.77ms–1,791.69ms across 3 back-to-back reps
on the same machine, no monotonic warm-up/cool-down pattern. Compaction
measurably **improves** read latency at production concurrency by
bounding live SSTable count — point-read p50 dropped ~9x, range p50
dropped 3-20x, compaction enabled vs. disabled, identical workload —
at the cost of a higher write **max** tail latency (325.5ms vs.
246.8ms) consistent with an occasional write landing behind an
in-flight flush+compaction cycle's own brief lock window, not a new
unbounded blocking path. **Tested scale, stated explicitly, not
implied universal**: 4–256 input SSTables, 3 overlap densities × 3
value sizes (32B–8KB), this project's own established key-cardinality
conventions. No universal latency SLA is claimed or was ever specified
to certify against.

## Endurance Evidence

The primary production endurance evidence is Increment 3's 4-hour
soak: 14,400s, 8 writers, 16 readers, `LsmConfig::default()` +
`compaction_auto_trigger: true`, ~14,893,196 writes, ~2,627,416
deletes, ~3,305,200,124 point reads, ~9,870,366 range scans, 193 real
automatic compaction cycles, 17,460,431 cumulative records dropped,
**0 in-run mismatches, 0 post-recovery mismatches** (all 20,000
independently-tracked keys verified correct after a real shutdown +
reopen, recovery in 493.2ms). Supplemented by 38/38 real crash cycles
(above) and 8/8 shutdown-endurance cycles across all 4 timing classes
(idle, pending-notification, mid-compaction, fully-settled — max
41.08ms, no hang). Full detail, including the complete resource/state
trend table sampled every 300s across the whole 4 hours: `PHASE_
COMPACTION_INCREMENT3_ENDURANCE.md` §7.

## Corruption Evidence

A corrupt input SSTable aborts the merge attempt immediately (`merge_
propagates_corruption_from_an_input_and_stops`) — inherited for free
from the already-certified Read Engine's own fail-closed block/index/
bloom-checksum validation (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`,
`ADR-RE-001` §7), since Compaction's merge consumes the identical
`SsTableRangeCursor` machinery every read path uses. Injected
compaction-specific I/O failure (Increment 3's failure-retry evidence,
row 20) never produced a corrupted Manifest or a partial output file
across 3 forced failures — each failed attempt is a clean no-op from
the outside, retried via the existing bounded fallback cadence, never
a busy loop.

## Differential/Property Evidence

Never the production algorithm as its own oracle, at every scale:
`compaction_preserves_logical_reads_for_every_prior_snapshot_seq`
(2,000 randomized put/delete/snapshot operations, `get_as_of`/
`contains`/`range()` compared against an independent reference model
for every live snapshot seq plus "now"); `compaction_never_changes_
logical_reads` (48 `proptest` cases, `proptest = "=1.11.0"`, unchanged
exact-pinned dependency); `auto_trigger_bounded_production_like_
integration_matches_reference_model` (400 ops, purely automatic, no
manual `compact_once` call); Increment 3's concurrent/snapshot/
tombstone endurance and the 4-hour soak's own 3.3-billion-read
verification against an independently tracked per-key model.

## Known Limitations

Stated honestly, per this project's own standing rule not to convert a
real blocker into a "known limitation," and not to convert a genuine
scope decision into an apology:

- **v1 strategy is size-tiered, full-merge only — by explicit,
  authoritative design, not a gap.** No leveled or partial-selection
  compaction exists or is certified here; the spec itself defers that
  to a future, measurement-driven decision (`ADR-COMPACTION-001`
  Decision 1). Full-merge's own accepted cost (worst-case ~2x
  transient disk usage, a merge cost proportional to total live data
  volume every cycle) is real and was measured, not hidden (§Storage
  Budget/§Performance Evidence above).
- **Real, disclosed run-to-run performance variance at 64+ input
  tables** (e.g. 128 tables: 461.77ms–1,791.69ms across 3 reps; 256
  tables: 896.34ms on the first rep, then 3,985.34ms and 3,462.38ms) —
  consistent with this project's own already-documented characteristic
  for other performance-sensitive paths (`PHASE_WRITE_ENGINE_
  CERTIFICATION.md` §3's own "performance run-to-run variance...
  remains unresolved at the root-cause level"). Not investigated
  further this phase; not hidden either.
- **RSS growth is real, not zero.** +9.7% (4,200→4,608 KB) across 150
  compaction cycles in the scaling benchmark; a 59,944–70,080 KB band
  (~17%) across the full 4-hour production soak. Both are
  structurally explained (bounded by live input-table count, matching
  the ADR's own design intent — cumulative compacted-through data grew
  ~668x in the same scaling run while RSS grew only 9.7%) and neither
  shows an unbounded, ever-climbing trend, but this document does not
  claim literally zero growth, per this project's own explicit
  instruction not to overclaim.
- **No external memory profiler was available in this environment** —
  the same limitation the Read Engine's own certification already
  disclosed (`PHASE_READ_ENGINE_CERTIFICATION.md` § Known
  Limitations). The "bounded, not leaking" conclusion rests on
  source-level ownership tracing (`ADR-COMPACTION-001` Decision 12)
  plus converging empirical evidence (scaling benchmark + 4-hour
  soak), not an external heap-trace tool.
- **A resource-contention test flake was observed and resolved
  during this phase, documented for completeness.** Two unrelated
  `wal::group_commit` tests failed once when the full parallel test
  suite ran concurrently with the 4-hour soak on the same machine;
  both passed cleanly in isolation while the soak was still running,
  confirming contention (matching `PHASE_WRITE_ENGINE_CERTIFICATION.
  md` §3's own precedent for exactly this class of finding), not a
  regression. The regression gate below was re-run on an idle machine
  for the authoritative result.

None of the above is a mandatory-gate failure or an unresolved
correctness question.

## Out-of-Scope Features

Not certified, not started, not implied by this document: leveled or
partial-selection compaction, Router, Replication, any multi-engine/
partitioned architecture, secondary indexes, a cache/mmap/prefetch
mechanism, or any change to the on-disk SSTable/WAL/Manifest format.

---

## Final Regression Gate (run at certification time, on an idle machine)

```
cargo fmt --check                                          clean
cargo clippy --all-targets --all-features -- -D warnings   clean
cargo test --lib                                            347/347 (debug, 123.09s)
cargo test --release --lib                                  347/347 (release, 126.05s)
cargo check --all-targets --all-features                    clean
wal_tests                                                    12/12
crash_consistency --features test-util                       2/2
pathological_recovery_matrix (debug)                          9/9
pathological_recovery_matrix (release)                        9/9
```

347 = the Read Engine's own certified 306 baseline + 41 Compaction-
phase tests (2 writer differential tests in `src/sstable/tests.rs` +
13 module-level + 14 engine-level + 12 automatic-trigger tests). No
existing test's assertions were weakened or altered anywhere across
Increments 1–3.

**A resource-contention flake, isolated and resolved, not silently
retried away**: earlier in this same certification session, a `cargo
test --lib` run coincided with the still-running 4-hour soak and
showed 2 failures in `wal::group_commit` — a file this phase never
touched. Both were re-run individually (`--test-threads=1`) **while
the soak was still active** and passed cleanly, confirming contention
from running the full parallel suite alongside an 8-writer/16-reader
soak on the same machine, not a regression (§Known Limitations). The
gate recorded above was run with no other workload active.

---

## Final Decision

**Mandatory gates: 35/35 PASS. 0 FAIL. 0 mandatory OPEN.**

# COMPACTION PRODUCTION READY = YES

**Exact scope of this decision: RubiXDB's single-engine, non-
partitioned LSM Compaction** — size-tiered, full-merge, automatic
background trigger — as certified above, built on the already-
certified Write Engine and Read Engine. This decision does **not**
certify leveled or partial compaction, Router, Replication, or the
larger partitioned RubiXDB architecture — none of those exist in this
codebase yet — and does **not** constitute a claim of full RubiXDB
production readiness.

**Per this project's own stop condition: do not proceed to Router,
Replication, or partitioning work as part of this same task.** This
certification closes the Compaction phase; any of that future work
starts as its own separately-scoped phase, only when explicitly
instructed.

---

## Evidence Index

- `PHASE_COMPACTION_ADR.md` (`ADR-COMPACTION-001`) — the 17
  foundational decisions plus Amendment 1's 6 further decisions
  (execution model, re-entrancy, shutdown, retry, default rollout
  posture, and a generalized test-design hazard).
- `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` — the pre-implementation
  read-only audit (existing infrastructure, protected-contract table,
  Windows delete-while-open empirical probe, worked retention truth
  table).
- `PHASE_COMPACTION_INCREMENT1_RESULTS.md` — deterministic core
  implementation and evidence record (13+13 tests, the retention
  erratum, the streaming-writer differential test, the shared-fixture
  race found and fixed).
- `PHASE_COMPACTION_INCREMENT2_RESULTS.md` — automatic trigger +
  execution integration (12 new tests, 2 real bugs found and fixed,
  the `compaction_auto_trigger` default reversal, the first bounded
  performance/storage-budget/latency baseline).
- `PHASE_COMPACTION_PERFORMANCE.md` — Increment 3's performance/
  correctness-under-load report (4–256-table sweep, 9-shape sweep,
  storage budget, concurrent read/write/compaction, snapshot/
  tombstone endurance, read/write latency impact, SSTable-count
  stability).
- `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` — storage-pressure/
  failure-retry/shutdown endurance, 38-cycle crash endurance, the
  harness's own 3 found-and-fixed bugs (documented, not hidden),
  resource scaling, and the full 4-hour production soak.
- `PROGRESS.md` — chronological narrative, 2026-09-14 through
  2026-09-22, every increment's own dated entry.
- `CHANGELOG.md` — `[Unreleased]` section, every increment's own
  entry.
- `PHASE_WRITE_ENGINE_CERTIFICATION.md` — the Write Engine's own,
  separate, still-valid certification (protected, not re-derived
  here).
- `PHASE_READ_ENGINE_CERTIFICATION.md` — the Read Engine's own,
  separate, still-valid certification (protected, not re-derived
  here; this document's own structure mirrors it directly, per
  instruction).
- `src/compaction/mod.rs`, `src/compaction/tests.rs`, `src/lsm/mod.rs`,
  `src/lsm/tests.rs`, `src/sstable/writer.rs`, `src/sstable/tests.rs`
  — the certified implementation and its test suite.
- `examples/compaction_bench.rs` — performance/resource/endurance
  benchmark harness (13 sections).
- `examples/compaction_crash_cycle_test.rs`,
  `examples/compaction_crash_cycle_child.rs` — real external-process
  crash-cycle harness (targeted fault-point sweep + random-delay
  mode).
- `examples/compaction_soak.rs` — the 4-hour production endurance soak
  harness, with its own race-free, independently-tracked correctness
  model.
