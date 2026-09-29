# Phase: RubiXDB Product Surface — Performance Baseline

## 0. Status and scope

This is a **baseline measurement**, not the final performance
certification Increment 13's Phase 70 describes
(`PHASE_RUBIXDB_PERFORMANCE_CERTIFICATION.md`) — that document is
explicitly gated on every mandatory hardening gate being closed first
(load testing through the full concurrency ladder, endurance runs,
fuzzing, crash-kill matrix), which this pass has not yet completed.
What follows is real, measured, reproducible evidence for the API's
current end-to-end `POST /v1/sql` performance, gathered against the
real release build, plus two real findings acted on while gathering
it.

**What is measured:** whole-request wall-clock latency (network + JSON
parse + auth + parse SQL + bind + plan + execute + serialize response),
via `api/examples/sql_bench.rs`, a new, reusable, real HTTP benchmark
tool (real `reqwest` client, real concurrent Tokio tasks, real percentile
computation from actual collected samples — never estimated).

**What is not measured here:** per-phase server-side breakdown (parse
vs. bind vs. plan vs. execute vs. serialize as separate numbers) — no
such instrumentation is exposed by the server today; this is a
documented limitation of this pass, not a claim of numbers this tool
doesn't produce. CPU/RSS sampling during the run, sustained endurance
behavior, and concurrency beyond 16 (reads) / 8 (writes) are also not
covered here — see §5.

## 1. Environment

- Real release build (`cargo build --workspace --release`), real
  `rubixdb gui --no-browser` embedded server (the actual product
  entry point, not a bespoke test harness), real generated local
  credential, loopback HTTP.
- Real production frontend build present (`RUBIXDB_FRONTEND_DIST`) but
  not exercised by this particular run (API-only workload).
- Fresh instance per run (`RUBIXDB_INSTANCES_ROOT` pointed at an empty
  temp directory), `bench_ro` seeded with 2,000 rows (10 distinct
  `grp` values, 200 rows each) plus one secondary index on `grp`.

## 2. A real bug found while gathering this baseline: local rate limit too tight for a single legitimate client

The embedded local server inherited the standalone `rubixdb-api`
binary's own default rate limit (200 req/s, burst 400) verbatim. That
default was sized for a *shared, multi-tenant* deployment's threat
model — protecting the service from any one abusive principal among
many. A local instance has exactly one principal (`local`, generated
per-instance) and no other tenant to protect against; the very first
real concurrent-read benchmark run (concurrency=4, 200 iterations)
immediately produced real `429 RATE_LIMITED` responses from its own
legitimate, single-client workload.

**Decision record** (Phase 50 format):

| | |
|---|---|
| PROBLEM | Local single-user instance rate-limited its own legitimate concurrent workload |
| MEASURED BASELINE | Real `429` responses at concurrency=4 against `rate_limit_rps=200.0, burst=400` |
| ROOT CAUSE | A multi-tenant-deployment default reused unexamined for the local single-principal case |
| OPTION 1 | Remove rate limiting entirely for the loopback-only local instance |
| OPTION 2 | Raise the local default substantially, keep the mechanism, add an operator override |
| OPTION 3 | Make the limiter per-connection instead of per-principal |
| SECURITY IMPACT | Option 1 removes a real, if narrow, defense-in-depth layer against a compromised local process or a DNS-rebinding-style browser attack against `127.0.0.1`; Option 2 keeps that layer with headroom; Option 3 adds real complexity (connection identity tracking) for no additional protection in the single-principal case |
| MEMORY IMPACT | None (same one-bucket-per-principal token bucket, still exactly one principal) |
| COMPLEXITY | Option 2: one changed constant plus two new env var reads, in `cli/src/host.rs` only |
| SELECTED DESIGN | Option 2 |
| WHY | Keeps a real, bounded ceiling (residual protection) while removing the actual measured bottleneck; smallest, most conservative change |

Implemented: `cli/src/host.rs`'s embedded-server config now defaults to
`rate_limit_rps=2000.0, burst=4000` (still bounded, not unbounded),
overridable via `RUBIXDB_LOCAL_RATE_LIMIT_RPS`/`_BURST`. Verified: the
same benchmark run that previously produced hundreds of `429`s now
produces **zero** rate-limit errors across the full read+write
workload (§4). The standalone `rubixdb-api` binary's own
`RUBIXDB_RATE_LIMIT_RPS`/`_BURST` env-configured default (200/400,
appropriate for its actual shared-deployment threat model) is
completely unchanged.

## 3. A real bug found in the benchmark tool itself (not the product)

The first version of the INSERT workload loop never cleared its table
between concurrency levels, so every level after the first restarted
its row-id counter at 0 and collided with rows the *previous* level had
already inserted — nearly every request beyond concurrency=1 failed
with a genuine (but benchmark-induced) primary-key conflict. Found by
inspecting the raw error stream (hundreds of `409 CONFLICT_ERROR`
responses), traced to the missing `DELETE FROM bench_rw` the
update/delete loops already had. Fixed; the corrected run (§4)
produced zero errors of any kind.

## 4. Results

All numbers are real samples from the release build described in §1.
`n` = successful samples; `throughput` = `n / wall-clock duration` for
that concurrency level (not an average of per-request rates).

### Read workloads (200 iterations/level)

| Workload | c=1 p50/p95/p99 | c=4 p50/p95/p99 | c=16 p50/p95/p99 | c=16 throughput |
|---|---|---|---|---|
| PK lookup | 0.21 / 0.33 / 0.37 ms | 0.27 / 0.36 / 0.42 ms | 0.63 / 1.35 / 2.47 ms | 20,736 req/s |
| Indexed lookup (secondary index) | 2.34 / 2.69 / 2.94 ms | 2.64 / 3.51 / 4.03 ms | 6.97 / 18.20 / 30.31 ms | 1,721 req/s |
| Range scan (50 rows) | 3.88 / 5.93 / 6.43 ms | 4.21 / 7.12 / 7.53 ms | 9.31 / 36.81 / 62.67 ms | 1,044 req/s |
| Full-table `COUNT(*)` (2,000 rows) | 3.77 / 4.91 / 5.21 ms | 4.27 / 6.25 / 6.97 ms | 7.99 / 43.85 / 77.50 ms | 1,106 req/s |
| `GROUP BY`/`HAVING` | 4.36 / 7.79 / 9.42 ms | 5.84 / 9.25 / 10.28 ms | 10.85 / 51.60 / 88.31 ms | 798 req/s |

### Write workloads (100 iterations/level, lower concurrency — see §5)

| Workload | c=1 p50/p95/p99 | c=8 p50/p95/p99 | c=8 throughput |
|---|---|---|---|
| INSERT | 3.06 / 4.84 / 8.12 ms | 26.61 / 31.46 / 31.81 ms | 290.6 req/s |
| UPDATE | 3.50 / 6.01 / 6.76 ms | 29.40 / 33.75 / 34.16 ms | 276.6 req/s |
| DELETE | 3.55 / 5.11 / 5.82 ms | 26.66 / 28.24 / 28.42 ms | 299.5 req/s |

## 5. Bottleneck notes (Phase 6, real evidence, not guessed)

- **PK lookup scales cleanly with concurrency** (throughput roughly
  doubling from c=1 to c=4, still climbing at c=16 with p50 still
  sub-millisecond) — the read path has real headroom at this data
  size.
- **Indexed lookup is ~10x PK lookup's latency.** Confirmed via a real
  `EXPLAIN` that the planner correctly chose `IndexScan` (not a
  fallback full scan) for the benchmark's `WHERE grp = ...` predicate
  — this is not a missing-index bug. A secondary-index lookup requires
  an index-entry read followed by a primary-key row fetch (two
  physical reads vs. PK lookup's one), which is the certified,
  documented shape of this engine's secondary-index architecture
  (`PHASE_RELATIONAL_INDEX_ARCHITECTURE.md`,
  `PHASE_RELATIONAL_INDEX_INCREMENT5_RESULTS.md`) — not something this
  increment touched (`git diff --stat -- src/relational/` is empty)
  or a newly introduced regression.
- **Write throughput plateaus (~250–320 req/s) regardless of
  concurrency, while write latency scales roughly linearly with it**
  (INSERT p50: 3.06ms at c=1 → 26.61ms at c=8, throughput roughly
  flat) — consistent with this LSM engine's certified group-commit
  write path (`PHASE1_GROUP_COMMIT.md`, `PHASE_WRITE_ENGINE_
  CERTIFICATION.md`): writes are coordinated through a single batching
  path by design, so added concurrency increases queuing/batch-wait
  latency rather than raw throughput once the batch window is
  saturated. Consistent with prior increments' own documented write
  characteristics, not a new finding specific to the product surface
  built in this increment.
- Both of the above are **pre-existing, certified engine
  characteristics**, not defects introduced or discovered in the
  API/CLI/GUI product surface itself — re-investigating or optimizing
  them would mean touching certified storage/index code, explicitly
  out of this increment's scope ("Do NOT redesign Write Engine, Read
  Engine... unless a proven product-surface dependency requires a
  minimal change" — no such dependency was found; the product surface
  correctly exposes the engine's real behavior, it does not add
  overhead of its own beyond ordinary HTTP/JSON framing, visible in
  the sub-millisecond PK-lookup numbers above).

## 6. Explicitly not yet done

- Concurrency beyond 16 (reads) / 8 (writes) — the mission's full
  ladder goes to 32/64; not run yet.
- CPU/RSS sampling during the benchmark run.
- Sustained endurance (this was a single bounded run per workload, not
  a long-duration soak).
- A fuzzing/expensive-query-flood pass under load.
- GUI/frontend-side performance (browser render time, JSON parse time)
  — this baseline is server/API-side only.

These remain open items for the full Increment 13 certification pass,
named here rather than silently folded into an "it's fast enough"
claim this evidence doesn't fully support yet.
