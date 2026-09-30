# Increment 14, Blocker 1 — Query Starvation

Real, end-to-end measurement. Tool: `api/examples/query_starvation_test.rs`
(new this increment). Real release `rubixdb-api.exe`, real on-disk
`LsmEngine`, real HTTP via `reqwest`, real `Get-Process` resource
sampling. No mocking, no in-process shortcut.

## 1. Scenario (matches the mission's own Blocker-1 spec)

- A 100,000-row table (`starve_big`), no secondary index, seeded via
  100-row batched `INSERT ... VALUES` statements (9.6-13.0s real seed
  time across runs).
- **Expensive query**: `SELECT grp, COUNT(*), SUM(val) FROM starve_big
  WHERE note LIKE '%needle%' GROUP BY grp HAVING COUNT(*) > 0` — forces
  a full sequential scan with a per-row `LIKE` string match (no index
  can serve this predicate), run back-to-back for the whole phase
  duration by one dedicated worker.
- **Cheap queries**: `SELECT * FROM starve_big WHERE id = <random>`
  (PK point lookup), run continuously by N concurrent workers, N ∈
  {10, 50, 100}.
- **Control-plane probes**: `/healthz`, `/readyz`, `/v1/instance`,
  `/v1/status`, polled continuously (one request per ~20ms) by a
  dedicated worker throughout every phase, including baseline.
- Each level measured twice, back to back: **BASELINE** (cheap +
  control-plane only) and **LOADED** (same, plus the expensive query
  running continuously) — the delta between the two, not a single
  absolute number, is the starvation evidence.

## 2. Real measured results (`phase_secs=12` per phase)

| Level | Phase | cheap p50/p95/p99 (ms) | control-plane p50/p95/p99 (ms) | expensive p50/max (ms) | peak active_requests |
|---|---|---|---|---|---|
| N=10 | BASELINE | 0.84 / 1.53 / 2.09 | 0.15 / 0.32 / 0.50 | — | 11 |
| N=10 | LOADED | 0.84 / 1.64 / 2.35 | 0.15 / 0.32 / 0.57 | 1050.76 / 1209.05 | 12 |
| N=50 | BASELINE | 3.90 / 6.32 / 7.53 | 2.12 / 3.80 / 4.61 | — | 28 |
| N=50 | LOADED | 4.17 / 6.82 / 8.17 | 2.15 / 4.01 / 4.72 | 792.63 / 869.22 | 27 |
| N=100 | BASELINE | 7.79 / 13.37 / 16.17 | 3.82 / 7.55 / 9.67 | — | 48 |
| N=100 | LOADED | 8.17 / 14.20 / 17.23 | 4.14 / 7.83 / 9.91 | 790.58 / 844.57 | 74 |

Zero errors across every phase (0 `err` in every row, both cheap and
control-plane samplers). Full raw run: seed log + all 6 phases
preserved in this increment's commit (`query_starvation_test.rs`
output, re-runnable verbatim via `cargo run --release -p rubixdb-api
--example query_starvation_test`).

## 3. Finding: no starvation at this scale

Control-plane p99 latency during LOADED is, at every level, within
~0.1-0.3ms of its own BASELINE — not the order-of-magnitude
degradation that would indicate the expensive query is blocking
control-plane request handling. Cheap-query p99 shows the same
pattern. The expensive query itself completes in ~0.8-1.2s per
iteration (real full-scan cost over 100,000 rows), never timing out,
never erroring, running 12-16 times per 12s phase without incident.

## 4. Root cause (why this architecture does not starve)

Inspected `api/src/routes/sql.rs` and `sql/src/exec/mod.rs` directly
(not guessed):

1. **Every SQL statement — cheap or expensive — runs inside
   `tokio::task::spawn_blocking`** (`run_with_cancellation_and_
   deadline`, `api/src/routes/sql.rs:190`). This moves execution onto
   Tokio's dedicated blocking-thread pool, off the async runtime's
   worker threads entirely. The expensive query occupies one blocking
   OS thread for ~1s; it never occupies an async worker thread at all.
2. **Control-plane handlers never call `spawn_blocking` and never take
   a lock**: `healthz`/`readyz`/`/v1/instance`/`/v1/status`
   (`api/src/routes/{health,instance,status}.rs`) are plain `async fn`
   that touch only atomics/cheap engine getters (`storage_state()`,
   `sstable_count()`, etc.) — they run entirely on the async worker
   threads the expensive query never touches.
3. **The SQL executor itself holds no lock across a query's
   duration** — `grep`'d `sql/src/exec/mod.rs` for `Mutex`/`RwLock`/
   `.lock()`/`.read()`/`.write()`: zero matches. Reads go through the
   certified engine's own snapshot/MVCC read path (already certified
   lock-free-for-readers), so one long-running read never blocks
   another read or blocks the control-plane getters.
4. **The test machine has 8 logical cores**; a single expensive query
   saturates at most one of them, leaving 7 free for the default
   Tokio async-worker pool (`worker_threads = num_cpus`) — real
   headroom exists, not just architectural isolation.

None of this required, and none of this changed, any certified-engine
code (`WAL`/`Manifest`/`SSTable`/`Compaction`/`Read Engine`/`Write
Engine`) — the isolation is a property of the API layer's existing
`spawn_blocking` design plus the engine's own already-certified
lock-free read path.

## 5. Explicit scope limitation (not hidden)

This scenario matches the mission's own literal Blocker-1 spec: **one**
expensive query against N cheap ones. It does **not** test:
- Multiple simultaneous expensive queries (e.g., enough concurrent
  full scans to saturate all 8 logical cores at once) — a
  CPU-exhaustion scenario distinct from "starvation by one query,"
  and out of this blocker's literal scope.
- A materially larger dataset (the expensive query here costs ~1s;
  a dataset producing a 30s+ scan was not run).
- Any workload beyond `SELECT`/point-lookup — write-path contention
  under a concurrent expensive read was not separately isolated.

These are named as open scope, not folded into the PASS below.

## 6. Verdict

**QUERY STARVATION = PASS** (measured scope: 1 expensive full-scan
query vs. 10/50/100 concurrent cheap PK lookups, 100,000-row table,
zero errors, control-plane and cheap-query tail latency essentially
unchanged between baseline and loaded phases).

**CONTROL-PLANE RESPONSIVENESS = PASS** (same evidence: `/healthz`,
`/readyz`, `/v1/instance`, `/v1/status` all stayed sub-10ms p99 even
at N=100 concurrent cheap queries plus a continuously-running
expensive query).

Required final property from the mission ("one expensive query cannot
make RubiXdb appear unavailable") holds for the scenario actually
measured. No product-layer scheduling change was needed — the
existing `spawn_blocking` + lock-free-read architecture already
satisfies it. No engine escalation required.
