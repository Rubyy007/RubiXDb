# ADR-ITEM-C-01 — An operator-settable cap on the embedded host's blocking thread pool

**Status:** ACCEPTED for implementation (Phase B of the Item C mission, 2026-10-07; the Phase A conclusion in `PHASE_ITEM_C_DISCOVERY.md` is "Bounded change required"). **Scope:** one runtime setting of the embedded host; nothing else.

## Decision

Add one strictly parsed environment setting, **`RUBIXDB_LOCAL_MAX_BLOCKING_THREADS`**, that sets `max_blocking_threads` of the embedded host's tokio runtime; **unset or empty means 512, the value in force today**, so the shipped behaviour is unchanged unless an operator lowers it.

## Reason (measured, `PHASE_ITEM_C_DISCOVERY.md`)

* Every SQL statement is dispatched with `tokio::task::spawn_blocking` (`api/src/routes/sql.rs:211`) into a pool whose cap is tokio's default 512 and whose idle keep-alive is 10 s (`cli/src/host.rs:180-183` sets neither). Tokio starts a new thread whenever it counts no idle thread at that instant (`tokio-1.53.1/src/runtime/blocking/pool.rs:393-452`).
* With 16 clients that connect inside the timed window (the shape of the original observation), the as-is server created **301-530 threads in the first 0.04-0.13 s** (14 of 14 runs; 530 = 18 + 512, the default cap reached) and ran at **19.0-26.1 k req/s**, p95 1.07-1.84 ms, p99 1.9-3.9 ms. With the pool capped at 16 / 32 / 64 the same load gave **34 / 50 / 82 threads** (exactly 18 + cap), **27.9-30.0 k req/s**, p95 0.79-0.92 ms, p99 1.18-1.50 ms. With connections opened before the clock starts the as-is server peaked at 73-158 threads and ran at 28.2-29.7 k req/s; capped, 30.0-30.7 k. Writes (fsync-bound, 230-270 req/s) were unchanged by a cap of 16 / 32 / 64 at 16, 32 and 64 writers.
* The growth is therefore a performance and resource-containment defect with a cheap bound, not a correct behaviour of the model, and the operator has no way to bound it today.

## Alternatives considered

1. **Leave as-is and document.** Rejected: the thread burst costs measurable throughput and tail latency (above), and documentation gives the operator nothing to act on.
2. **Cap `max_blocking_threads` (chosen).** Measured: bounds threads exactly, equal or better throughput and tail in every condition tested; a one-line change at the runtime builder; it acts *below* the dispatch, so no statement semantic changes. Weakness: a fixed cap does not remove the burst of thread creation up to the cap; it only bounds it (a cap of 16 creates at most 16).
3. **Replace the per-statement `spawn_blocking` by a bounded worker pool of our own.** Measured to work (fixed pools of 1, 8 and 16 workers all kept 25.6-30.4 k req/s), but it moves queueing, shutdown and panic-isolation into our code, leaves the six admin `spawn_blocking` sites on tokio's pool, and is a larger change than the evidence needs. Rejected as the fix; kept as a documented option.
4. **Move SQL execution off the blocking pool entirely** (run it on the async workers). Assessed, not feasible in this scope: statement execution is synchronous engine code that waits on the WAL fsync; running it inline blocks the async workers. This was measured on the one statement that already does it (`COMMIT`, `sql.rs:526`): `/healthz` p50 0.79 -> 9.3 ms and p99 1.3 -> 27 ms under 16 committing clients. Making the engine asynchronous is a different project. Not proposed.

## Correctness impact

None on any statement semantic, so no escalation:

* **Timeouts:** unchanged. The deadline is an async timer around the join handle (`sql.rs:216`), independent of the pool; the executor's own deadline check is unchanged. Time spent waiting for a free blocking thread counts toward the deadline exactly as it does today; with the default 512 there is no practical wait. With a cap an operator sets below their concurrency, a statement can queue, and a statement queued for longer than its deadline times out with the same error as today. That is the nature of a resource limit (as with `sql_max_sessions_per_principal`), chosen by the operator, and is stated in the setting's documentation.
* **Cancellation:** unchanged (`CancelOnDrop`, `sql.rs:200-208`); a queued, already-cancelled task stops at its first check when it starts.
* **Session state, snapshot semantics, error propagation, response schemas, endpoint status:** untouched; no file in `api/` changes.
* **No deadlock at any cap:** no statement waits on another blocking task (no nested `spawn_blocking`; `grep` shows seven call sites, none inside another), so a full pool delays but cannot wedge. The six admin `spawn_blocking` calls share the pool and may queue behind statements under a low cap, for at most the statement deadline.
* Memory: queued tasks are bounded by the requests in flight (not by the pool).

## Performance impact (predicted from Phase A, to be re-measured in Phase C)

* **Default (unset):** identical to the baseline binary, within its own run-to-run range.
* **Cap 16-64:** thread peak = 18 + cap; cold-start 16-client reads 28-30 k req/s vs 19-26 k, p99 about 1.2 ms vs 1.9-3.9 ms; warm reads equal (30 k vs 28-30 k); writes equal (230-270 req/s) up to 64 writers.

## Configuration surface

| Item | Value |
|---|---|
| Name | `RUBIXDB_LOCAL_MAX_BLOCKING_THREADS` (same family and the same strict parser as `RUBIXDB_LOCAL_RATE_LIMIT_RPS` / `_BURST`, `cli/src/startup_env.rs`) |
| Type | base-10 integer |
| Default | unset or empty: **512** (tokio's default, i.e. today's behaviour) |
| Bounds | **16 to 512 inclusive** |
| Invalid value | startup fails before anything is created, with a message that names the variable (no silent fallback) |
| Scope | the embedded host (`rubixdb gui`, the supported v1 server); **not** the standalone `rubixdb-api` binary (unsupported for v1, D-2) |

*Why those bounds.* **512** is today's effective value and the largest the pool has ever been (a larger value would add exposure and was never measured). **16** is the smallest cap that was measured with the real mechanism (reads at 16 clients; writes at 16, 32 and 64 writers); a lower value would let the pool be smaller than the usual concurrent writer count that group commit batches, and would let one slow statement starve the admin routes, neither of which was measured. The single-worker read result (25.6-27.8 k req/s) is not a basis for allowing 1 (the write side was not tested below 16).

## Implementation scope (files)

`cli/src/startup_env.rs` (the setting, its parser and unit tests); `cli/src/host.rs` (apply it where the runtime is built, line 180); a new real-process test file `cli/tests/blocking_pool_cap_integration.rs`. **No change in `api/`** (the cap acts at the runtime builder, so `api/src/routes/sql.rs` and `api/src/main.rs` stay as they are). No new dependency, no `unsafe`, no protected path.

## Explicitly out of scope

Changing the default (the measured data support 16-64 for this workload; whether to ship a lower default is an operator-visible decision that this ADR does not take); `worker_threads`; `thread_keep_alive`; the standalone API binary; the `COMMIT`-on-an-async-worker finding (recorded in `OPEN_ITEMS.md`); moving statement execution off the blocking pool; per-statement-class pools; admission control or queue limits; a metric for pool depth (the pool's size is visible from outside as the process thread count; exposing it through the observability layer is a separate item).
