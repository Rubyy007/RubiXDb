# ADR-LIFECYCLE-001 -- Cooperative cancellation of startup index recovery at shutdown

Status: **ACCEPTED and IMPLEMENTED (option A)** -- authorized by the maintainer on 2026-10-05 ("implement option A, not option C"); implemented in Phase 2 Increment C2 (see section 5). Originally PROPOSED the same day (Increment C; baseline finding F-06).
Date: 2026-10-05. Decision owner: maintainer.

## 1. Context (measured, not assumed)

* After a kill during `CREATE INDEX`, the next start runs `IndexBuilder::recover_incomplete_builds` on a background thread (`rubixdb-index-recovery`) **after** the server is already serving (`api/src/recovery.rs`, started from `cli/src/host.rs` and `api/src/main.rs`). It re-runs the whole backfill ("restart, not resume", `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` section 8).
* Graceful shutdown closes the listener, drains requests (bounded 30 s), then **joins the recovery thread with no bound**, then calls `engine.shutdown()` (`host.rs`, `api/src/main.rs`).
* Baseline measurement (400,000 rows): shutdown requested while recovery ran took **11.0 s, 14.0 s, 17.2 s** to exit (acknowledgement 0.010-0.016 s; port already free). Recovery itself took 11.5-15.7 s; `OPEN_ITEMS` records 21-35 s at 600,000 rows. The stop time therefore scales with table size and has no upper bound.
* The only in-flight work during that wait is the backfill itself. No client request is being served (the listener is closed).

## 2. Why this cannot be fixed inside the Phase 2 scope

A bound on the wait needs a way to **stop the backfill early**. `IndexBuilder::backfill` (`src/relational/index.rs`, `backfill_and_activate`, `recover_incomplete_builds`) takes no cancellation input and checks nothing between chunks (`create_index_online` states the same). The options available without touching it are all unacceptable:

| Option | Why not |
|---|---|
| Give up waiting after N seconds and call `engine.shutdown()` while the backfill thread is still writing | Racing a writer against engine shutdown is an unproven state in the certified engine contract. |
| Give up waiting and exit the process without `engine.shutdown()` | That is exactly a process kill (safe for durability, and the index is retried at next start) but would be reported as a graceful stop: it blurs the "graceful vs crash" separation the lifecycle contract requires. |
| Keep waiting (status quo) | Unbounded. Mitigated only by messaging (done in Increment C: the admin response and `rubixdb instance stop` now say it is waiting). |

Changing `src/relational/index.rs` is a change to certified relational behaviour and is outside the mission's modifiable scope, so per the architecture change-control rule this ADR is written instead of the change.

## 3. Options

**A (recommended). Cooperative cancellation at chunk boundaries.**
* Add an optional cancellation flag (`Arc<AtomicBool>`, the same shape as `rubixdb_sql::exec::CancellationToken`) to the recovery entry points only: `recover_incomplete_builds_cancellable(&self, cancel: &AtomicBool)` and the same for `recover_incomplete_drops` (`sweep_index_entries`). `backfill` checks it once per `BACKFILL_CHUNK_ROWS` chunk, **outside** the epoch write lock.
* On cancellation: stop, return a distinct result (`Cancelled`), **do not** call `mark_index_failed`, leave the index `Building` (or `Dropping`). The next start restarts it from scratch, which is exactly the state a kill leaves, so correctness rests on the already-certified restart protocol and adds no new on-disk state.
* Existing callers and `create_index_online` (client-driven `CREATE INDEX`) are unchanged; the original methods delegate with a never-set flag.
* `api::recovery::spawn_index_recovery` passes the flag; shutdown sets it right after the listener closes, then joins. The join is then bounded by one chunk (a bounded, measured quantity) and no timeout is invented.
* Files that would change: `src/relational/index.rs`, `src/relational/index_tests.rs`, `api/src/recovery.rs`, `cli/src/host.rs`, `api/src/main.rs`. No change to `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`, `Cargo.*`.
* Required evidence before acceptance: unit test that a cancelled build leaves the index `Building` and a later `recover_incomplete_builds` completes it with a correct index; real-binary test (the Increment C tests already in `cli/tests/index_backfill_crash_integration.rs`) asserting stop time during recovery is bounded to a small constant and independent of table size (measure at 200,000 and 600,000 rows); the existing mid-backfill crash test unchanged; concurrent writes during a cancelled recovery leave the index consistent when it completes.

**B. Bound the wait and exit without engine shutdown.** Rejected (section 2).

**C. Status quo plus messaging.** Implemented in Increment C as the interim: `/readyz` and the admin shutdown reply report recovery, `instance stop` states that it is waiting. The stop time is unchanged.

**D. Make recovery resumable.** Rejected by the existing backfill ADR (restart, not resume).

## 4. Decision requested

Authorize option A (and the listed files, `src/relational/` only) as a follow-up increment, or choose C as the permanent behaviour and close F-06 as accepted. Until decided, F-06 stays **OPEN**.

## 5. Implementation record (Increment C2, 2026-10-05)

* `src/relational/index.rs` only (no WAL / manifest / SSTable / compaction / `src/error.rs` / `Cargo.*` change): `RecoverySummary { recovered, cancelled }`; `recover_incomplete_builds_cancellable(&AtomicBool)` and `recover_incomplete_drops_cancellable(&AtomicBool)`; `backfill` and `sweep_index_entries` read the flag once per chunk (500 rows / 1,000 entries), outside the epoch write lock and before the chunk's writes. The existing `recover_incomplete_builds` / `recover_incomplete_drops`, `create_index_online` and `drop_index_online` keep their signatures and never cancel. A cancelled index is left `Building` / `Dropping` (not `Failed`, not promoted, catalog row not removed). A test-only chunk hook (`cfg(test)`, absent from every other build) makes mid-pass cancellation deterministic.
* `api/src/recovery.rs`: `RecoveryState::Cancelled` (`/readyz` `index_recovery: "cancelled"`), `IndexRecovery::request_cancel`. Cancellation is requested at the **start** of shutdown (the shutdown trigger in `cli/src/host.rs` and `api/src/main.rs`, and the admin shutdown handler), in parallel with the request drain, so the later join waits at most one chunk.
* Evidence: 4 engine tests (cancel before start; cancel at the chunk-2 boundary then writes then restart gives an index equal to the table, 1,290 of 1,290 entries; cancel mid drop-sweep leaves `Dropping` and the restart removes it; originals never cancelled); real binary at 400,000 rows, 3 runs: **stop during recovery 0.11 / 0.12 / 0.11 s (exit 0, no leftover process, port released)** against 11.0 / 14.0 / 17.2 s (baseline) and 10.8 / 13.1 / 19.1 s (pre-Phase-2 HEAD, same session); `cli/tests/index_backfill_crash_integration.rs` (200,000 rows): the stop returns in < 4 s, the owner reports the interruption, the next start restarts the index and finishes it (index `ready`, lookups correct, 200,000 rows).
* Not claimed: stop latency above 400,000 rows (expected to stay one chunk, not measured); recovery that is cancelled repeatedly in a row (each restart begins again from scratch by design).
