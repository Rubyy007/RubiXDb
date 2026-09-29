# Phase: RubiXDB Increment 13 — Final Reliability Record

Consolidation document (Phase BD). Detailed evidence lives in
`cli/tests/crash_recovery_integration.rs` (real process-kill matrix),
`cli/tests/gui_instance_integration.rs` (instance lifecycle/races),
`PHASE_RUBIXDB_ENDURANCE.md` (sustained-run recovery-adjacent
evidence), and `PHASE_RUBIXDB_GUI_INSTANCE_INCREMENT_RESULTS.md`
(instance-manager-specific reliability findings).

## 1. Real process-kill crash matrix

Four real tests, actual compiled binary, real `Child::kill()`
(ungraceful `TerminateProcess`/`SIGKILL`, no destructors, no
graceful-shutdown handler), real restart, real query through the real
product path — not the raw engine test harness:

| Case | Result |
|---|---|
| Committed write, real kill, restart | Survives. **PASS.** |
| Uncommitted (`BEGIN`, no `COMMIT`) write, real kill, restart | Never visible. **PASS.** |
| Committed DDL (schema+table+index), real kill, restart | Catalog and index both recover correctly. **PASS.** |
| Sustained concurrent write load, real kill at an unpredictable moment | No torn/partial rows; every surviving row internally consistent (12/60 attempted rows survived in the recorded run). **PASS.** |

The core engine's own crash-consistency certification
(`tests/crash_consistency.rs`) is unchanged and unaffected — this
matrix closes the gap that nothing before this increment had proven
these same guarantees hold when exercised through the actual product
entry point (CLI → HTTP → embedded server → engine) rather than the
engine directly.

## 2. Instance lifecycle reliability

Real, process-level, `cli/tests/gui_instance_integration.rs`:

| Case | Result |
|---|---|
| First run, no explicit config, becomes owner | **PASS.** |
| Data persists across two entirely separate OS processes | **PASS.** |
| Two `-c` processes racing an unstarted instance | Exactly one owner, no duplicate storage. **PASS.** |
| Two `gui` processes racing | Loser attaches, never duplicates. **PASS.** |
| `gui`-owned instance + separate CLI client, shared data | **PASS.** |
| `instance list`/`status` against real, changing state | **PASS.** |
| Port collision with an unrelated process | Owner still acquires the lock, falls back to a real ephemeral port, never attaches to the unrelated process. **PASS.** |
| Two independently named instances | Independent ports, independent directories, no crossover (`two_named_instances_get_independent_ports`). **PASS.** |

## 3. Startup race / instance locking

The OS-level `flock`/`LockFileEx` primitive itself (never a PID file)
is the entire mechanism — verified released on both clean exit and a
real killed process (§1's own instance-crate-level test,
`lock_is_released_when_owner_process_is_killed`). No staleness
heuristic exists anywhere in this codebase because the OS guarantee
makes one unnecessary. **PASS.**

## 4. Compaction interaction

`compaction_auto_trigger: true` throughout every real endurance/load
run in this increment (never disabled to make a run "cleaner") — the
180-second sustained endurance run and the 1-64 concurrency load
ladder both ran with automatic Compaction active the whole time, with
correctness verified at the end of each (final row counts, final
`SELECT` correctness). No Compaction-specific failure observed.
**PASS** (for the durations/workloads actually run — a dedicated,
longer Compaction-under-endurance stress scenario beyond what these
runs already exercised was not run as its own separate case).

## 5. Cancellation / deadline reliability

Real HTTP-disconnect cancellation (`api/tests/api_cancellation.rs`):
a genuinely expensive query under real concurrent contention,
cancelled via a real dropped connection, server remains fully
responsive immediately after, no zombie query observed, all
concurrent background work eventually completes. **PASS** (Phase O).

Deadline enforcement: inherited, unchanged Increment-12 evidence
(`statement_exceeding_its_deadline_is_a_controlled_timeout`) — a
statement exceeding its configured deadline returns a safe, controlled
error, resources released. **PASS** (Phase P, inherited).

## 6. Session/snapshot reliability under real sustained load

`PHASE_RUBIXDB_ENDURANCE.md` §4: 3,511 real session/transaction
cycles over 180 seconds, including periodic real snapshot retention
held open across concurrent writes from other clients (Phase J),
zero session-related errors, handle/thread counts flat across the
whole run. **PASS.**

## 7. API/CLI/GUI semantic consistency

Not independently re-verified as its own dedicated test this
increment beyond what is already implied by every surface (CLI, the
GUI's own SQL console, and direct API calls in the fuzz/bench/
endurance/cancellation tests) exercising the identical `POST /v1/sql`
handler — there is only one SQL execution path in this product
(`PHASE_RELATIONAL_SQL_API_ARCHITECTURE.md`), so semantic divergence
between surfaces is structurally not possible, not merely untested.
Stated as an architectural guarantee, not a fresh empirical result
from this increment. **PASS by construction**, not by a dedicated new
comparison test.

## 8. What remains genuinely open

- **Restart/recovery of an in-progress `CREATE INDEX` backfill**
  specifically (Phase U) — the DDL crash test (§1) covers a completed
  `CREATE INDEX`, not a kill mid-backfill. Not run this increment.
- **Commit-acknowledgment-loss semantics** (Phase S: server commits,
  client loses the response) — not tested as its own scenario
  distinct from the crash-kill matrix's own commit/rollback proof.
- **A materially longer endurance duration** for Compaction/recovery
  interaction specifically.
- **Delete-safety reliability** (Phase AO/AP) — not applicable; no
  delete-object UI exists anywhere in this product yet
  (`PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §10).
- **Multi-instance simultaneous sustained use** (Phase AJ, two
  instances both under real concurrent load simultaneously, not just
  both existing) — instance *isolation* is proven (§2); sustained
  *simultaneous load* on two instances was not run as its own case.

## 9. Overall reliability verdict

**RELIABILITY = PASS for every gate with real evidence above** —
crash-kill matrix, instance lifecycle/races, locking, Compaction
interaction (within tested scope), cancellation/deadlines, session/
snapshot endurance, and semantic consistency by construction. §8's
items are real, named open scope, not silently claimed.
