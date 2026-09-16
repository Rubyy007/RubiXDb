# RubiXDB Phase 3C — Architecture

## Scope

Phase 3C is a **certification** phase, not a feature phase: its job is
to determine whether the WAL + Group Commit + Dedicated Batch
Coordinator foundation (unchanged since Phase 2B, hardened in Phases
3A/3B) can be frozen and trusted as the durable base for the next
stage (MemTable integration). It closes the gaps Phase 3B's own
results (`PHASE3B_TEST_RESULTS.md` §11) explicitly named as blockers:
a true long-duration soak, periodic forced-crash-during-soak testing,
pathological recovery stress, a quantified recovery-memory analysis,
production observability, observability overhead measurement, and a
full security/dependency certification.

Per operating brief §28 ("do not rush into MemTable while unresolved
WAL certification issues remain") and this project's own established
practice (every prior phase executed as measured, independently-
verified increments — never one unreviewable pass), Phase 3C is
executed the same way. See `PHASE3C_TEST_RESULTS.md` for the
authoritative, current completion status and the final certification
decision (§26).

## What Phase 3C adds

### `GroupCommitter`/`BatchCoordinatorPool::purge_before`

New, small, safe wrappers (`src/wal/group_commit.rs`,
`src/execution/batch_coordinator.rs`) delegating to `FileWal::
purge_before` under the existing `wal` lock — mirrors `rotate()`'s
existing wrapper exactly. Enables realistic bounded-WAL operation
during a multi-hour soak (periodic checkpointing, exactly as a real
deployment would operate) without which a true long-duration run at
full throughput would accumulate far more records than `open_for_
recovery` can safely materialize on this host (`PHASE3B_ADR.md`
ADR-P3B-5). Safe to call concurrently with ongoing writes — `purge_
before` never touches the active segment.

### Periodic forced-crash-during-soak harness

`examples/crash_cycle_child.rs` + `examples/crash_cycle_test.rs`
(operating brief §5-§6): the driver spawns a real child process running
the production `BatchCoordinatorPool` under load, waits a randomized
(seeded, reproducible, `std`-only PRNG) delay, then forcibly kills it
(`Child::kill()` — an external, asynchronous, abrupt termination, not
a self-inflicted `abort()` at a code-chosen point). After each kill,
the parent reopens the same WAL directory and verifies the recovery
contract directly, across many cycles against one accumulating
directory.

### True long-duration soak harness (v2)

`examples/long_soak_test.rs`, extending Phase 3B's `soak_test.rs` with
periodic checkpointing (via the new `purge_before` wrappers above) and
CPU sampling alongside RSS (one `powershell.exe Get-Process` call per
sample interval — no new dependency). Run at 100 and 1,000 writers for
4 hours each (operating brief §3's minimum), sequentially, against the
production `BatchCoordinatorPool`.

### Recovery-memory scaling analysis

`examples/recovery_memory_scaling.rs` (operating brief §8): sweeps
1M/5M/10M/15M-record WAL fixtures, measuring `open_for_recovery`'s
time and RSS (before/after/peak) at each size — the direct, controlled
experiment `PHASE3B_ADR.md` ADR-P3B-5 named but had not run.

### Pathological recovery fixture matrix

`tests/pathological_recovery_matrix.rs` (operating brief §7): 9
consolidated fixtures against the existing, unmodified recovery
contract — no second recovery implementation.

### Observability additions

`BatchCoordinatorStats::{bytes_total, writes_timed_out}` (operating
brief §10) — both per-batch increments (not per-request), matching this
project's own established low-contention pattern. See `PHASE3C_ADR.md`
for the recovery-API design-decision analysis (§9) and the honest
accounting of which requested metrics remain unbuilt and why.

## Preserved architecture (unchanged)

```text
Logical Writers -> Dedicated Batch Coordinator -> Group Commit -> Durable WAL
```

No change to the WAL binary format, `durable_through`'s semantics,
sequence allocation, rotation semantics, `FileWal`'s recovery contract,
`LeaderFailureGuard`'s fail-closed poison policy (Phase 3A), or the
`CoordinatorFaultPoint` completion-guard fix (Phase 3B). Phase 3C found
no correctness defect requiring a redesign of any of these — every
new test in this phase re-confirms behavior already established, or
extends coverage into previously-untested territory (long duration,
external-process kills, large-scale recovery, pathological fixtures)
without finding a reason to change the underlying design.
