# Increment 18 (Part 2): Index Result Materialization

Companion to `PHASE_RUBIXDB_INCREMENT18_ACCESS_PATH_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT18_TRANSACTION_SCAN_SEMANTICS.md`,
`PHASE_RUBIXDB_INCREMENT18_PERFORMANCE.md` and
`PHASE_RUBIXDB_INCREMENT18_RESULTS.md`.

Increment 17 deferred "eager index-result materialization" on the strength of
one measurement (+13% resident memory over the result at 25,000 matches). The
mandate here said *do not force streaming*: reproduce, measure, and keep the
eager design if it is not a production risk. It **is** a risk, but not the one
Increment 17 measured.

## 1. Baseline (the Increment 17 tree; N = 100,000, forced index, compaction on)

The eager path built `Vec<(pk_values, Row)>` for every match before the first
row was returned.

| K matches | full query p50 | RSS growth with result alive | `LIMIT 10` p50 | `LIMIT 10` / full |
|---|---|---|---|---|
| 10 | 0.243ms | 0.0MB | 0.242ms | 1.0x |
| 100 | 1.656ms | 0.0MB | 1.502ms | 0.9x |
| 1,000 | 16.54ms | 0.3MB | 14.70ms | 0.9x |
| 10,000 | 168.96ms | 7.0MB | 149.60ms | 0.9x |
| 25,000 | 414.28ms | 17.0MB | 373.19ms | 0.9x |

- **Memory is not the problem.** Resident growth is ~0.7KB per matched row, the
  same as the result's own rows, and `max_index_scan_rows` already bounds it.
  (Increment 17's +13% over the streaming scan at 25,000 is consistent.)
- **Latency is.** `LIMIT 10` cost 90% of the full query: the executor fetched
  and decoded all K rows (~14.5us each) and discarded all but ten. At K = 25,000
  that is 373ms to return ten rows, against a sub-millisecond ideal. A paged
  query UI, `EXISTS`-shaped probes and any `LIMIT` over an indexed predicate
  pay for the whole match set.
- It also defeats cancellation and deadlines (an expired deadline is noticed
  only after the eager fetch completes) and holds the whole match set resident
  while a slow consumer drains it.

## 2. Candidate designs

| | Design | Verdict |
|---|---|---|
| A | eager `Vec` of rows (current) | rejected: the LIMIT/cancel/deadline cost above |
| B | lazy index iterator that also enumerates entries lazily | rejected for now: the cost decision needs the *exact* match count, which requires enumerating all entries up to the break-even anyway; enumeration is the cheap part (~0.6us per entry vs ~14.5us per row) |
| C | bounded chunked batches | a possible refinement of B; not needed to remove the dominant cost |
| D | a streaming executor operator for the whole query | not needed: `SeqScan`/`PkRangeScan` are already lazy and every operator above `AccessOp` already pulls row by row |
| **E** | **hybrid: enumerate entries up front (key-only, bounded), fetch rows on demand** | **chosen** |

E keeps what makes the cost model possible (an exact `K` found cheaply, with a
hard resource bound) and removes what was expensive (fetching and decoding
rows nobody asked for).

## 3. Design

`IndexBuilder::lazy_row_fetcher(entries, as_of)` -> `IndexRowFetcher`: an
iterator over the enumerated entries that performs the point read and decode of
one row per `next()`, at the **same snapshot** as the entry probe, skipping an
entry whose row is not visible at the snapshot (exactly as the eager path did).
Per scan it holds only the primary-key entry list (a few tens of bytes per
match), never the rows. `AccessSource::IndexFetch` in `AccessOp` wraps it.

What this preserves, and how it is tested
(`sql/src/materialization_tests.rs`, counting rows with the relational
layer's own `index_rows_fetched` counter rather than timing):

| Property | Mechanism | Test |
|---|---|---|
| snapshot / MVCC | rows fetched at the transaction's pinned `as_of`; entries probed at the same | differential and F-2 suites, unchanged and passing |
| `LIMIT` / `OFFSET` | `LimitOp` simply stops pulling | `LIMIT 5` over 1,000 matches fetches exactly 5 rows |
| backpressure | no read-ahead; the only per-scan buffer is the entry list, bounded by `max_index_scan_rows` | a consumer pulling one row at a time sees exactly one fetch per pull for 40 pulls; building the operator fetches none |
| cancellation | `AccessOp::next` calls `ExecCtx::check()` each row | start, pull 10, cancel -> `Cancelled`, stays cancelled, **no row fetched after cancellation**; the transaction still reads correctly and commits |
| deadline | same check | an expired deadline fetches nothing |
| resource limits | `max_index_scan_rows` is checked while enumerating, before any fetch | a limit one below the match count fails closed with zero rows fetched |
| transaction overlay | base entries the transaction rewrote are dropped before fetch; its own rows are visited after the fetch is exhausted (a `LIMIT` that stops early never pays for them) | `limit_composes_with_the_transaction_overlay` |
| `ORDER BY` | an eliminated `Sort` relies on index order, which entry order preserves; with local writes it is materialized and sorted (bounded) | the F-2 ordered tests, unchanged |
| JOIN / aggregation / DISTINCT / ORDER BY above | consume `AccessOp` through the same `Operator` interface | full SQL suite, unchanged |
| `UPDATE` / `DELETE` | `find_target_pks` drains the operator into a bounded target list (`max_dml_target_rows`) before any mutation | transaction property tests with exact target counts |

**Writes are not pretended to stream.** `UPDATE`/`DELETE` must collect their
targets before mutating (Halloween safety, and the write set is bounded
anyway); they keep bounded materialization. Only the *target-finding read* is
lazy.

## 4. Result

(`ForceIndex`, same dataset, same machine.)

| K | full query p50 | RSS growth | `LIMIT 10` p50 before -> after |
|---|---|---|---|
| 10 | 0.252ms | 0.0MB | 0.242 -> 0.249ms |
| 100 | 1.662ms | 0.0MB | 1.502 -> **0.269ms** |
| 1,000 | 16.53ms | 0.1MB | 14.70 -> **0.794ms** |
| 10,000 | 169.56ms | 6.4MB | 149.60 -> **6.60ms** (22.7x) |
| 25,000 | 421.39ms | 15.7MB | 373.19 -> **16.22ms** (23.0x) |

Full-result latency is unchanged (within noise: the same 14.5us per row), and
resident growth is slightly lower (17.0 -> 15.7MB at 25,000: the eager `Vec`
and the result rows no longer coexist).

## 5. What remains (stated plainly)

`LIMIT 10` still costs O(K) because the *entry enumeration* is eager: 6.6ms at
K = 10,000, 16.2ms at 25,000 (~0.65us per entry). Removing it would need
chunked enumeration, and the cost decision currently needs the exact `K`
before choosing a path. That trade was weighed and not made: the remaining cost
is 4.4% of the full-fetch cost, bounded by `max_index_scan_rows`, and a
chunked enumerator would change when the abandon-the-index decision can be
made. Recorded as a possible future refinement, with the measurement above as
its baseline.
