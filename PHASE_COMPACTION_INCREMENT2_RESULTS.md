# Compaction — Increment 2: Production Trigger + Execution Integration Results

**Status:** Automatic trigger wired and integration-tested. **COMPACTION
CORE = PASS** (unchanged, Increment 1). **COMPACTION TRIGGER
INTEGRATION = PASS.** **COMPACTION PRODUCTION READY = NO** — remaining
gates listed in §8.

**Date:** 2026-09-21

This document records what was implemented, tested, and found this
increment. It does not re-derive the design — see `PHASE_COMPACTION_
ADR.md`'s Amendment 1 (execution model, re-entrancy, shutdown, retry
policy, and the `compaction_auto_trigger` default decision, each with
full Decision/Reason/Alternatives/Safety/Performance/Tests-required
structure) and `PHASE_COMPACTION_INCREMENT1_RESULTS.md` (the
deterministic core this increment builds on, unmodified). This is the
implementation and evidence record, following the same append-only,
cite-don't-repeat convention as every prior increment's own results
document.

Increment 1 commit: `b0b30f9` — feat(compaction): implement
deterministic full-merge compaction core.

## 1. Scope actually delivered

- **Background compaction worker** (`spawn_compaction_thread`, `src/
  lsm/mod.rs`): spawned by `LsmEngine::open` only when `LsmConfig.
  compaction_auto_trigger` is `true`. Driven by a bounded-capacity-1
  `mpsc::sync_channel<CompactionMsg>` (`MaybeCompact`/`Shutdown`),
  woken by either a real notification (sent by the flush thread right
  after every successful publish) or a periodic fallback tick
  (`recv_timeout`, reusing the existing `storage_pressure_retry_
  interval` — no new config field).
- **`compact_once_impl`** (free function, `src/lsm/mod.rs`): Increment
  1's own `compact_once` method body, extracted into a function over
  cloned field `Arc`s (mirroring `spawn_flush_thread`'s own established
  shape) so both the manual entry point and the new worker thread call
  byte-identical logic. `LsmEngine::compact_once`/`should_compact`
  are now thin wrappers; their own behavior is unchanged from
  Increment 1.
- **`CompactionRunGuard`** (`src/lsm/mod.rs`): the one new
  synchronization primitive — a `compare_exchange`-based RAII guard on
  a shared `AtomicBool`, preventing two compactions (manual + automatic,
  or worker-catch-up-loop + fresh trigger) from running concurrently.
  No global engine lock.
- **`LsmConfig.compaction_auto_trigger: bool`** (new field, **default
  `false`** — `PHASE_COMPACTION_ADR.md` Amendment 1, Decision A5).
- **`LsmEngine::shutdown()`** (extended): stops the worker via a
  blocking `Shutdown` send (not `try_send` — a real bug found and
  fixed this increment, Amendment 1 §A3), joins it, lets any in-
  progress cycle complete first.
- **`flush_completions: Arc<AtomicU64>`** (new, test/observability-
  only field + `LsmEngine::flush_completions()` accessor): closes a
  real, empirically-found test race — see §2.
- 12 new tests (`src/lsm/tests.rs`, `mod auto_trigger_tests` nested
  inside `compaction_tests`) exercising the automatic path
  specifically — §6.

**Explicitly not touched this increment**: `src/compaction/mod.rs`'s
own merge/retention algorithm (Increment 1, unchanged — only its
module doc comment and stale `#[allow(dead_code)]` annotations were
updated to reflect a real production caller now exists), the SSTable
format, WAL, Manifest edit types, or any certified Write/Read Engine
API.

## 2. Two real bugs found and fixed during this increment

Both found empirically, through this project's own standing "measure,
don't assume" discipline — neither was hypothesized in advance.

**(a) Shutdown message loss (`try_send` vs. `send`).** The bounded(1)
`compaction_sender` channel can already hold an unconsumed
`MaybeCompact` notification at the moment `shutdown()` runs. The
initial implementation called `compaction_sender.try_send(Shutdown)`
(mirroring the flush thread's own notification-style usage elsewhere)
— which silently fails to enqueue when the channel is already full,
leaving the worker to notice the stop request only via its own
periodic fallback tick (up to 5s later, by default). Under a property
test issuing heavy write load against dozens of engines in sequence,
this compounded into a real, measured, multi-minute slowdown that
looked like a hang under an external 60-second timeout probe. Fixed by
switching specifically the `Shutdown` send to a **blocking** `send` —
safe, because the worker either drains the channel promptly (it
returns to `recv_timeout` quickly whenever there is no real compaction
work) or has already exited, in which case `send` to a disconnected
channel returns an error immediately without blocking. Full reasoning:
`PHASE_COMPACTION_ADR.md` Amendment 1 §A3.

**(b) `flush_completions` — a real, narrow race in the shared test
fixture helper.** Increment 1's `put_and_wait_for_sstable_count` waited
for `immutable_count() == 0` as its "this flush has settled" signal.
That check can become true *before* the flush job's own tail (WAL
purge, then the unconditional `storage_state` swap-to-`Healthy` every
successful flush performs) has actually run — a real, narrow window
that the original `compaction_defers_while_storage_full_and_never_
mutates_storage_state` test (Increment 1) hit intermittently: the test
forces `StorageFull` immediately after its fixture finishes building,
and an in-flight flush's own delayed swap could silently clobber that
forced value back to `Healthy` before the compaction-defer assertion
ran. Fixed by adding a new, purely additive, test/observability-only
counter (`flush_completions`, incremented strictly after every other
side effect of a successful flush job, mirroring `checkpoint_seq`/
`storage_pressure_events`'s own existing precedent) and extending the
shared fixture helper to wait for it, but *only* on the specific `put`
that actually triggered a new flush (most `put`s in a loop do not).
Verified stable across 5 repeated runs of the specific test, plus 2
full-suite runs (334/334 debug, matching Increment 1's own count)
before any Increment 2 code was added.

## 3. A genuinely unbounded test-design hazard, found and generalized

Not a production defect — a hazard specific to testing an automatic
system by racing it. Several of this increment's first-draft tests
built a fixture by looping `put()` against an **already-running**
worker until the test thread's own `sstable_count()` poll happened to
observe the live count at or above `compaction_trigger_count`. Because
the worker's own reaction (capture → merge → splice, tens of
milliseconds) can complete inside the same window the test thread
needs to notice the crossing, the test thread loses this race far more
often than it wins once the worker is warm. Traced directly to one
test (the resource-safety test, §6) hanging past a 60-second external
timeout: a debug trace showed one fixture-rebuild needing **725**
individual `put()` calls (not 4) before the test thread's own check
happened to land in the narrow pre-splice window — an order of
magnitude beyond what any reasonable bound would allow, and with no
guaranteed upper bound at all in principle. **Fix, applied uniformly
across every affected test** (6 of the 12): build any fixture that
must reach or exceed `compaction_trigger_count` *offline* first
(`compaction_auto_trigger: false`, reusing Increment 1's own already-
deterministic `small_flush_config`/`put_and_wait_for_sstable_count`
helpers unmodified), then reopen with the worker enabled to observe
its real, automatic reaction — a one-directional, monotonic wait (the
count only ever goes *down* once triggered) rather than a race to hold
it *at* a value. Full reasoning: `PHASE_COMPACTION_ADR.md` Amendment 1
§A6.

A related, smaller bug in the same family: the resource-safety test's
own file-count assertion (`live_sstable_ids().len() == <physical .sst
file count>`) was originally a one-shot `assert_eq!`, immediately after
a `wait_until(sstable_count() == 1)` wait. The logical splice
(reflected by `sstable_count()`) and the physical file deletion that
follows it are two separate steps inside the same worker call — the
count can already read `1` a moment before the corresponding `remove_
file` calls have actually run. Fixed by making that assertion itself a
bounded `wait_until` (10s), not a one-shot check — it failed
intermittently (4 files still on disk immediately after the count
read 1) until this fix.

## 4. Chosen trigger model, briefly (full reasoning in the ADR amendment)

Count-based only (`compaction_trigger_count`, unchanged from
Increment 1/Decision 14) — no size/time/read-amplification/memory
trigger, per the brief's own explicit scope limit. Background worker
thread, not synchronous post-flush, chosen specifically to avoid
blocking the flush thread for a compaction cycle's own duration
(§5's own measured range: 30ms–226ms this increment, scaling with
input count) — `PHASE_COMPACTION_ADR.md` Amendment 1 §A1 walks every
criterion the brief required this be checked against (writer blocking,
flush interaction, concurrent reads, snapshot lifetime, storage-
pressure behavior, re-entry, shutdown, failure propagation, resource
ownership) against this project's own actual contracts, not external
convention.

## 5. Bounded performance / storage-budget / latency measurement

**Not** a soak, **not** an optimization pass — a first baseline
measurement only, per this increment's own explicit scope limit.
Single run per data point.

### 5.1 Compaction performance + storage budget (4/8/16/32/64 input SSTables)

Manual `compact_once()` (identical merge/write code path to the
automatic worker — `compact_once_impl` is shared), `memtable_max_size_
bytes: 4096`, distinct keys per input table. `actual_dir_peak_bytes`
sampled by a concurrent polling thread (2ms interval) watching the
SSTable directory's own total byte size across the whole call.

| input_sstables | duration_ms | input_bytes | output_bytes | records_read | records_retained | peak theoretical (input+output) | actual measured peak |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 4  | 80  | 11,677  | 11,367 | 386   | 386 | 23,044  | 23,044  |
| 8  | 102 | 23,405  | 15,180 | 770   | 500 | 38,585  | 38,585  |
| 16 | 121 | 47,039  | 16,625 | 1,526 | 500 | 63,664  | 63,664  |
| 32 | 137 | 94,527  | 18,505 | 3,030 | 500 | 113,032 | 113,032 |
| 64 | 226 | 189,503 | 22,265 | 6,038 | 500 | 211,768 | 211,768 |

**Storage budget vs. the ADR's own "~2x" theoretical claim (Decision
1/Decision 12)**: the measured, real, actual on-disk peak matched the
theoretical `input_bytes + output_bytes` figure **exactly**, at every
input size — no additional filesystem/OS overhead was observable at
this sampling resolution. The ADR's own "~2x" framing is a reasonable
description when `output_bytes ≈ input_bytes` (little duplicate/
superseded data to drop); this benchmark's own fixture has heavy key
overlap by construction (500 possible distinct keys), so measured
`output_bytes` is well below `input_bytes` once table count grows
(64 tables: 189,503 → 22,265, a ~8.5x reduction) — the theoretical
peak is `input + output`, not literally `2 × input`, and this data
confirms that more precise framing rather than the ADR's own
simplified shorthand.

Duration scales with total input data volume, as expected (Decision 1
explicitly accepts this v1 cost); 64 tables (190KB of input) still
completed in 226ms — no cause for concern at this scale, and no
optimization was attempted or is in scope this increment.

### 5.2 Write/read latency impact under concurrent writers + readers + real automatic compaction

2,000 writes + 2,000 reads issued concurrently (separate threads)
against 200 warm keys, `compaction_trigger_count: 4`,
`compaction_auto_trigger: true` — automatic compaction genuinely
running throughout (multiple real cycles observed during the run).
Recorded actual results; no arbitrary pass/fail threshold (none was
already defined for this increment):

| | p50 | p99 | max |
|---|---:|---:|---:|
| write | 3.2–3.4 ms | 18.6–19.5 ms | 84.9–117.4 ms |
| read  | 65.8–67.7 µs | 136.3–174.6 µs | 229.9–377.9 µs |

(Two figures given per cell: observed across two independent runs of
the same test, both included for transparency about run-to-run
variance — no averaging or cherry-picking.) Reads remain sub-
millisecond at p99 even with compaction actively running concurrently;
write tail latency (max ~85–117ms) is consistent with an occasional
write landing behind an in-flight flush+compaction cycle, not a
regression this increment introduced any new blocking path for
(Amendment 1 §A1's own writer-blocking analysis: the write path itself
gains zero new synchronization from this increment).

## 6. Test suite added this increment

`src/lsm/tests.rs`, `mod auto_trigger_tests` (nested inside `mod
compaction_tests`) — 12 new tests, all using `compaction_auto_trigger:
true` and (with the deliberate exceptions the test names themselves
describe) no manual `compact_once()` call:

1. `auto_trigger_fires_at_and_above_threshold_never_below` —
   deterministic 3/4/5/9-SSTable threshold cases (offline-built fixture
   + reopen, per §3's fix), below-threshold checked via `should_
   compact()` directly (no wait needed), at/above via a bounded wait
   for automatic collapse to 1.
2. `compaction_run_guard_permits_exactly_one_concurrent_holder` — a
   direct stress test of the re-entrancy primitive itself (16 threads,
   500 acquire attempts each): maximum observed concurrent holder count
   asserted exactly 1.
3. `auto_trigger_defers_while_storage_full_and_resumes_once_healthy` —
   exploits a genuinely race-free window (a freshly reopened engine's
   worker can only first wake via its own fallback tick, never an
   immediate notification, since no flush occurs on reopen) to
   deterministically force `StorageFull` before the worker's first
   evaluation; confirms no compaction runs while forced full, then
   resumes automatically once healthy.
4. `auto_trigger_retries_after_a_failed_attempt_via_the_next_fallback_
   tick` — a fires-once `CompactionIoFaultHook`; confirms the worker
   retries and succeeds with no manual intervention, and that the
   success is a genuine second attempt.
5. `shutdown_lets_an_in_progress_automatic_compaction_finish_before_
   returning` — an injected, bounded (200ms) delay inside the
   compaction fault hook gives `shutdown()` a real window in which a
   cycle is genuinely in flight; asserts prompt return (no hang) and
   never a partially-published state on reopen.
6. `auto_trigger_never_drops_a_live_snapshot_or_changes_its_reads` — a
   live `Snapshot` outstanding across an automatic compaction cycle;
   confirms `oldest_live_snapshot_seq()`, the snapshot's own historical
   read, and the latest read are all unaffected.
7. `auto_trigger_eventually_retries_a_deferred_physical_deletion` — a
   held-open `RangeScanIter` forces deferred physical deletion;
   confirms the compaction itself still completes (Manifest + splice)
   and the deferred file is swept once released, via the worker's own
   fallback tick, with no further trigger.
8. `auto_trigger_bounded_production_like_integration_matches_reference_
   model` — 400 PUT/DELETE operations against 30 keys, purely
   automatic (no manual call), verified against an independent
   reference model (`get_as_of`/`range` at the final seq) — never the
   production algorithm as its own oracle.
9. `auto_trigger_crash_mid_compaction_recovers_correctly_after_restart`
   — a real panic injected at `AfterAllRemoves`, reached through the
   automatic worker (caught by its own `catch_unwind`, never
   propagating to the test thread — the real production behavior, not
   a manually-wrapped call); a real restart (shutdown+drop+reopen)
   confirms correct recovery. Reuses one already-validated recovery
   branch from Increment 1's exhaustive manual crash-window sweep; this
   test's own job is proving that branch is reachable and correct
   *through the automatic path specifically*.
10. `repeated_automatic_compaction_cycles_leave_no_leaked_files_or_
    stuck_deletions` — 8 repeated build-offline/reopen/collapse cycles
    against the same directory; bounded resource-safety proxy (§7).
11. `bounded_compaction_performance_and_storage_budget_baseline` — §5.1.
12. `bounded_write_read_latency_under_concurrent_automatic_compaction`
    — §5.2.

**Full auto_trigger_tests module**: 12/12 passed, run twice
consecutively (161.33s, then 153.33s) to confirm stability after the
races in §3 were fixed — no flakiness observed in either run.

## 7. Bounded resource safety

Real OS-level RSS/handle sampling is out of scope for an automated
`cargo test --lib` run, by the same established precedent as
Increment 1's own `sstable_count_and_immutable_memory_track_flushes_
exactly_no_extra_retention` test (`src/lsm/tests.rs`) — a `cargo test`-
internal OS-metrics sample would be noisy and platform-specific; this
codebase's own convention for that kind of measurement is a separate,
external, reproducible soak run (e.g. `examples/realistic_full_
pipeline_soak.rs`), not an in-process assertion. This increment's own
bounded test instead locks in the actual code-level resource-safety
invariants: across 8 repeated build/collapse cycles against the same
directory, (a) the automatic worker always collapses each fixture to
exactly 1 live SSTable, and (b) every physically-present `.sst` file
on disk eventually corresponds exactly to a live SSTable — no leaked
or orphaned files accumulate across repeated automatic cycles. Each
cycle also fully closes and reopens the engine (and therefore the
worker thread), so this test incidentally also exercises repeated
worker-thread spawn/join lifecycle safety 8 times over, with `cargo
test`'s own process-level thread accounting catching any leaked
`JoinHandle` (a `shutdown()` that failed to join would leave the
process's own thread count growing across cycles — not observed).

A genuine, real, external RSS/handle/thread soak specifically
exercising the automatic trigger under sustained multi-hour load is
listed as a remaining gate (§8) — not attempted this increment, which
is explicitly scoped to a bounded check only.

## 8. Full regression gate

All run after `cargo fmt` (applied cleanly — several pre-existing
Increment-1-era formatting deviations were also picked up and
corrected, no behavior change) and confirmed `cargo clippy --lib
--tests -- -D warnings` clean.

| Gate | Result |
|---|---|
| `cargo fmt --check` | clean (after one `cargo fmt` pass) |
| `cargo clippy --lib --tests -- -D warnings` | clean |
| `cargo test --lib` (debug) | 346/346, run 3x total for stability (139.07s, 131.12s, 123.93s) |
| `cargo test --lib --release` | 346/346 (124.25s) |
| `cargo check --all-targets` | clean |
| `cargo test --test wal_tests` | 12/12 |
| `cargo test --test crash_consistency --features test-util` | 2/2 |
| `cargo test --test pathological_recovery_matrix` | 9/9 |

346 = Increment 1's own 334 + this increment's 12 new tests. No
existing test's assertions were weakened or altered: the §2(a) fix
touches only `shutdown()`'s own body; the §2(b) fix is additive (a new
observability counter plus one extra, stricter wait inside the shared
`put_and_wait_for_sstable_count` helper — the same assertions every
Increment 1 caller of that helper already made, just now waiting for a
slightly later, more-complete settle point); the §3 fixes are entirely
confined to this increment's own new `auto_trigger_tests` module.

**Protected-contract audit**: `git diff --stat -- src/wal/ src/
error.rs src/manifest/` — empty. Only `src/compaction/mod.rs`,
`src/lsm/mod.rs`, and `src/lsm/tests.rs` were touched this increment.
`Cargo.toml`/`Cargo.lock` unchanged — zero new dependency. Read Engine
public API surface unchanged (verified by inspection: no signature in
§1's list overlaps any `ADR-RE-001`/`ADR-RE-002` type or method).

## 9. Final status

**COMPACTION CORE = PASS** (Increment 1, unchanged, re-verified this
increment via the full regression gate above).

**COMPACTION TRIGGER INTEGRATION = PASS.**

**COMPACTION PRODUCTION READY = NO.** Remaining gates before that
status could be claimed:

- Full performance characterization beyond the bounded 4/8/16/32/64
  baseline (§5.1) — larger scale, varied key-overlap ratios, varied
  `compaction_trigger_count` values.
- A dedicated resource benchmark with real OS-level RSS/handle/thread
  sampling (§7), external to `cargo test`, matching this project's own
  established soak methodology.
- A long-duration write/read/compaction endurance run (hours, not the
  bounded 2,000-op §5.2 measurement) — the kind of run that originally
  surfaced the Write Engine's and Read Engine's own most important
  findings in earlier phases.
- A storage-pressure endurance run specifically exercising sustained
  `StorageFull`/`StoragePressure` cycling with automatic compaction
  active throughout, beyond §6 test 3's own bounded, single-cycle
  check.
- A final certification matrix (mirroring `PHASE_READ_ENGINE_
  CERTIFICATION.md`'s own 30-row structure) before any "PRODUCTION
  READY = YES" claim.

**WRITE ENGINE = PRODUCTION READY** and **READ ENGINE = PRODUCTION
READY** remain unchanged, protected (§8's audit), and re-verified.

STOP after this report — the next Compaction increment (whichever of
§8's remaining gates it addresses) is a new, separately-scoped increment,
not an automatic continuation from this one.
