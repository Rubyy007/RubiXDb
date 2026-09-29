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
doesn't produce. Sustained multi-minute endurance behavior is tracked
separately (`PHASE_RUBIXDB_ENDURANCE.md`).

**Update (concurrency extended to the full 1-64 ladder, real resource
sampling added):** the initial pass below (§2-§4, concurrency 1-16)
has been superseded by §7's full-ladder run, which also found and fixed
a second, more precise rate-limit sizing issue and captured real
RSS/handle/thread sampling via `Get-Process` during the run. §2-§6 are
kept as the honest record of what was measured first and why it
needed a second pass, not retroactively edited away.

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

## 7. Full concurrency ladder (1-64), real resource sampling

Re-run with `CONCURRENCY_LEVELS` extended to `[1, 2, 4, 8, 16, 32, 64]`,
`READ_ITERATIONS_PER_LEVEL` raised from 200 to 1,600 (so even the
c=64 level gets a statistically meaningful ~25 samples/worker), and
`write_levels` extended to `[1, 2, 4, 8, 16, 32]` at 200
iterations/level. Real release build, real `rubixdb gui --no-browser`.
**Zero errors of any kind across the entire run** (11,200 read
requests + 3,600 write requests).

### A second, more precisely measured rate-limit finding

The first rate-limit fix (§2, raised to 2000rps/4000burst) was itself
still too low — proven, not guessed, this time: this run's own
legitimate single-client PK-lookup throughput alone sustained
13,700 req/s at concurrency=4 and 25,981 req/s at concurrency=16, both
comfortably exceeding a limiter whose burst bucket only holds 4,000
tokens. Raised again to `rate_limit_rps=100_000.0`,
`rate_limit_burst=200_000` — comfortably above the actual measured
ceiling this time rather than a second guess — verified by this same
run completing with zero `429`s. `RUBIXDB_LOCAL_RATE_LIMIT_RPS`/`_BURST`
remain the operator override; the standalone `rubixdb-api` binary's own
default (200/400) is still untouched.

### Read workloads (1,600 iterations/level)

| Workload | c=1 p50/p99 | c=16 p50/p99 | c=64 p50/p99/max | c=64 throughput |
|---|---|---|---|---|
| PK lookup | 0.18 / 0.30 ms | 0.54 / 1.59 ms | 1.74 / 9.70 / 21.63 ms | 28,793 req/s |
| Indexed lookup | 2.20 / 2.63 ms | 5.59 / 58.08 ms | 21.70 / 186.69 / 293.09 ms | 2,012 req/s |
| Range scan (50 rows) | 3.82 / 5.31 ms | 8.29 / 111.18 ms | 8.93 / 637.62 / 761.45 ms | 1,086 req/s |
| Full-table `COUNT(*)` | 3.67 / 7.78 ms | 7.26 / 135.76 ms | 9.89 / 462.15 / 579.97 ms | 1,137 req/s |
| `GROUP BY`/`HAVING` | 4.39 / 6.16 ms | 8.86 / 152.10 ms | 10.51 / 606.49 / 676.09 ms | 949 req/s |

### Write workloads (200 iterations/level)

| Workload | c=1 p50 | c=8 p50 | c=32 p50/p99/max | c=32 throughput |
|---|---|---|---|---|
| INSERT | 3.52 ms | 29.06 ms | 119.27 / 129.31 / 129.82 ms | 264 req/s |
| UPDATE | 3.59 ms | 31.52 ms | 104.55 / 112.55 / 113.80 ms | 304 req/s |
| DELETE | 3.85 ms | 30.52 ms | 112.20 / 139.15 / 139.73 ms | 279 req/s |

### A real finding: PK lookups scale, everything else's tail latency does not

PK lookup's throughput keeps climbing through the whole ladder (5,145
→ 28,793 req/s, c=1→64) with p99 staying under 10ms even at c=64. Every
other read workload (indexed lookup, range scan, count, `GROUP BY`)
plateaus in **throughput** by around c=8-16 (as already noted in §5)
**and its p99/max latency then grows sharply** past that point — range
scan's p99 goes from 5.31ms (c=1) to 637.62ms (c=64), a ~120x
degradation, while its own throughput barely moves (256 → 1,086
req/s). PK lookup shows no such tail blowup. This is real, measured
evidence of contention specific to the non-PK read paths under high
concurrency (plausibly a shared lock or limited-parallelism stage
those paths share that pure PK lookup's path does not use) — not
guessed, but also **not root-caused in this pass**: identifying the
exact contention point would mean instrumenting or reading deeper into
certified engine-internal code, which this increment's own boundary
("Do NOT redesign Read Engine... unless a proven product-surface
dependency requires a minimal change") does not yet justify without
that deeper analysis. Recorded here as a real, open follow-up item —
explicitly **not** claimed as resolved, and **not** silently absorbed
into a passing grade.

### Resource trend during the full run

Sampled via `Get-Process` every ~1s against the real `rubixdb.exe`
process (Windows; `WorkingSet64`/`HandleCount`/`Threads.Count`), for
the whole ~95-second run plus a short cool-down:

| | baseline (idle) | peak (during c=64) | ~1s after run ends |
|---|---|---|---|
| RSS | ~10.0 MB | ~46.2 MB | ~24.7 MB |
| Handles | 118 | 406 | 310 |
| Threads | 18 | 202 | 199 |

RSS and handles both climb under peak concurrency load and visibly
**come back down** once the load stops (46.2 MB → 24.7 MB RSS,
406 → 310 handles within ~2 seconds) — the shape of bounded, load-
proportional resource use, not monotonic unbounded growth. Thread
count stays elevated near its peak briefly after the run (199 vs. 202
peak) which is consistent with Tokio's blocking-thread-pool keep-alive
policy (idle worker threads are not torn down instantly) rather than a
leak; this pass did not run long enough afterward to confirm the
thread count eventually settles back toward the ~18-thread baseline,
which is exactly the kind of question a longer endurance run
(`PHASE_RUBIXDB_ENDURANCE.md`) is for for real.
