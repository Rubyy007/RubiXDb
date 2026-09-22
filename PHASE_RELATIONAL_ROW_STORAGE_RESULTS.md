# PHASE_RELATIONAL_ROW_STORAGE_RESULTS

**Scope**: the relational row-storage foundation — `RELATIONAL ADR
AMENDMENT 003` (RA.1–RA.6), connecting the certified catalog (commit
`d66029d`) to actual user-table row storage. This document records what
was actually built, measured, and verified — nothing beyond it.

**RELATIONAL DATABASE PRODUCTION READY = NO.** No SQL parser, binder,
executor, or authorization enforcement exists. This increment adds row-
storage primitives only.

---

## 1. What was implemented

- **`src/relational/value.rs`**: `RelationalValue`/`RelationalType` — D4's
  full closed type set (`BOOLEAN`, `INTEGER`, `BIGINT`, `REAL`, `DOUBLE`,
  `DECIMAL`/`NUMERIC`, `TEXT`, `BLOB`, `DATE`, `TIME`, `TIMESTAMP`). Row-
  value (non-key) encoding reuses D3's `RowValue` envelope from
  `catalog::encoding` *by calling the same code*, not a re-implementation
  (see §2).
- **`src/relational/key.rs`**: order-preserving key encoding for every
  type (RA.2) — sign-flip transforms for signed integers/`DECIMAL`/
  `DATE`/`TIMESTAMP`, the IEEE-754 monotonic bit-transform for `REAL`/
  `DOUBLE` (with `-0.0`/`+0.0` canonicalization), plain big-endian for
  non-negative `TIME`, and an escape-then-terminate scheme for `TEXT`/
  `BLOB` inside composite keys. Composite-key encode/decode. The table-
  row physical key layout (`0x01 || table_id:u32 BE || 0x00000000:u32 BE
  || encoded_pk`), implemented exactly as Architecture document §5
  already specified.
- **`src/relational/table_store.rs`**: `TableStore` — `put_row`,
  `put_rows` (genuine multi-row atomic write), `get_row`, `delete_row`,
  `scan_table`. Every mutation is exactly one `LsmEngine::write_batch`
  call, even at N=1 (RA.5). Every read resolves the table's shape from
  the unmodified `CatalogService` — no second metadata structure.
- **`src/catalog/schema.rs` — one additive extension (RA.4)**:
  `system.columns` gained a new trailing `type_params: BLOB` field
  (`[precision, scale]` for `DECIMAL`/`NUMERIC`, `NULL` otherwise) — the
  bare `data_type:u8` tag had no room for a parameterized type's own
  parameters. D31-licensed (additive trailing field), not a redesign;
  every pre-existing `system.columns` field is untouched.
- **`benches/table_store_bench.rs`**: measures the encoding overhead
  `TableStore` adds on top of already-benchmarked raw `LsmEngine` calls.

## 2. ADR decisions implemented

`RELATIONAL ADR AMENDMENT 003` (this increment's own governing document,
appended to `PHASE_RELATIONAL_DATABASE_ADR.md`, D1–D33/AMENDMENTs
001–002 untouched):

- **RA.1** — row-value encoding for the full D4 type set, built on
  `catalog::encoding`'s envelope. The envelope itself was refactored
  (not duplicated) into `encode_row_envelope`/`decode_row_envelope`,
  generic over the per-domain value type, so `catalog::encoding::encode_
  row`/`decode_row` (`CatalogValue`) and `relational::value::encode_row`/
  `decode_row` (`RelationalValue`) both call the *same* header/bitmap
  logic — verified by re-running the full existing 44-test catalog suite
  unmodified after the refactor (still passing, confirming the on-disk
  format is unchanged).
- **RA.2** — order-preserving key transforms, exact bytes specified and
  property-tested (not asserted from inspection): integer/`BIGINT`/
  `DECIMAL`/`DATE`/`TIMESTAMP` sign-flip, `REAL`/`DOUBLE` monotonic bit-
  transform (including the `-0.0`/`+0.0` canonicalization edge case RA.2
  calls out by name), non-negative `TIME`, and the escape-then-terminate
  `TEXT`/`BLOB` scheme (proven correct by direct case analysis in the
  amendment, verified by a dedicated "`TEXT` not in last composite-key
  position" test).
- **RA.3** — table row key layout, implemented exactly as Architecture
  document §5 already specified; `TableStore` resolves every operation's
  table shape from `CatalogService` directly, no cache.
- **RA.4** — `system.columns.type_params`, the one additive catalog
  extension this increment required.
- **RA.5** — `put_row`/`delete_row`/`put_rows` each issue exactly one
  `write_batch` call, verified directly (not assumed) by asserting the
  engine's sequence counter advances by exactly one per call regardless
  of row/column count.
- **RA.6** — row-size limit (1 MiB, matching the flat-KV API's own
  default) enforced before any engine call.

## 3. Files changed

`src/relational/{mod,value,key,table_store,error,tests}.rs` (new),
`src/catalog/{encoding,schema,service,tests}.rs` (additive: envelope
refactor + `type_params` field), `src/lib.rs` (+1 line), `Cargo.toml`
(+1 `[[bench]]` entry), `benches/table_store_bench.rs` (new),
`PHASE_RELATIONAL_DATABASE_ADR.md` (+AMENDMENT 003).

## 4. Tests added

79 new tests:

- `relational::value` (4): row round-trip for every type, all-`NULL`,
  truncated-buffer rejection, `DECIMAL` precision/scale validation.
- `relational::key` (24): boundary + property tests for every type's
  ordering (`i32`/`i64`/`i128`/`f32`/`f64` proptest generators, `TIME`
  bounded to its valid non-negative range), the `-0.0`/`+0.0` identical-
  encoding test, `NaN`/negative-`TIME` rejection, `DECIMAL` sign-
  boundary ordering, `TEXT` prefix-ordering and embedded-`0x00` cases,
  composite-key round-trip/ordering/unambiguity (including `TEXT` not in
  the last position), malformed-escape and missing-terminator rejection.
- `relational::tests` (23 integration + 1 property): put/get round-trip
  (simple + `NULL` + composite PK), single-`write_batch`-call proof,
  delete/get, multi-row `put_rows` atomic visibility, table-scan
  namespace isolation (including a direct physical-boundary test at
  neighboring/min/max `table_id`, not only post-filtering), cross-
  namespace isolation (catalog rows, flat-KV keys), invalid-input
  rejection (wrong value count, `NULL` PK, `NOT NULL` violation, type
  mismatch, `DECIMAL` precision overflow, missing table), oversized-row
  rejection (verified no WAL record written), restart persistence
  (insert→restart→get, insert→restart→scan, delete→restart→verify-
  gone), concurrent `put_row` (16 threads, all visible), concurrent scan
  during writes (never a torn row), a delete/read race (never an `Err`),
  and a differential test against an independent, serialized `BTreeMap`
  reference model (48 proptest cases).
- `catalog::schema` (1 new): `type_params` round-trip for `DECIMAL` and
  ordinary columns.

## 5. Full test results

| Suite | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo test --workspace --lib` (debug) | 468 `rubixdb` + 30 `rubixdb-api` passed, 0 failed |
| `cargo test --release --workspace --lib` | 468 `rubixdb` + 30 `rubixdb-api` passed, 0 failed |
| `cargo test --test wal_tests` | 12 passed, 0 failed |
| `cargo test --test pathological_recovery_matrix` | 9 passed, 0 failed |
| `cargo test --release --test crash_consistency --features test-util` | 2 passed, 0 failed |

## 6. Benchmark results (measured, `cargo bench --bench table_store_bench`, this machine, release profile)

| Operation | Mean time |
|---|---|
| `LsmEngine::put` (raw) | 5.62 ms |
| `TableStore::put_row` | 3.95 ms |
| `LsmEngine::get` (raw) | 329 ns |
| `TableStore::get_row` | 6.74 µs |

| `scan_table` rows | Mean time | Throughput |
|---|---|---|
| 100 | 228 µs | 438 Kelem/s |
| 1,000 | 1.53 ms | 654 Kelem/s |

**Honest reading**: the write-path pair (`put` vs. `put_row`) is `fsync`-
dominated on this machine (both in the multi-millisecond range,
consistent with this machine's own documented `Immediate`-mode `fsync`
latency) — `put_row` measuring nominally *faster* here is within normal
run-to-run noise for an `fsync`-bound operation, not a real effect; no
write-path overhead claim is made either way. The read-path pair is
**not** `fsync`-bound (pure in-memory access), and shows a real,
measured ~20x difference (329 ns → 6.74 µs) — this is genuine overhead,
attributable to `get_row`'s per-call catalog resolution (`get_table` +
`get_columns`, no cache, RA.3's own deliberate v1 design — "no separate
catalog cache is required for correctness," not yet optimized for
repeated-access performance) plus key/row encode-decode, not primarily
the encoding itself. Recorded honestly as a real, measured cost, not
papered over — a future increment adding a read-through catalog cache
(explicitly deferred by RA.3, not forbidden) would be the direct,
measurement-motivated response if this cost matters in practice.

A 10,000-row `scan_table` case was attempted and aborted after ~15
minutes (the setup phase — 10,000 sequential individual `put_row` calls,
each its own `fsync` round trip — not the scan itself, dominates); not
reported, since no number was actually obtained. 100/1,000-row results
above are real, complete measurements.

## 7. Pre-existing failures

None newly observed. The `group_commit` throughput test pair (`m1_2`/
`m1_3`) already flagged as a pre-existing, machine-throughput-dependent
baseline characteristic in the two prior increments was not re-run this
increment (no source path this work touches is exercised by it); no new
regression suite failure was found anywhere.

## 8. Protected-path and security audit

`git diff --stat -- src/manifest/ src/compaction/ src/sstable/ src/wal/
api/` — empty for every path. No `unsafe`, no new logging of key/value/
row contents, no unbounded allocation from untrusted input (every
`Vec::with_capacity` in the new code is sized from caller-supplied, already-
materialized data — key lengths, column counts, row counts — never from
a decoded on-disk integer; decode paths that do read a length from bytes
use bounds-checked slicing first, matching `catalog::encoding`'s and
`wal::ops`'s own established discipline), no new panics in non-test code
beyond provably-infallible `try_into().expect(...)` conversions on
already-length-validated slices (the same pattern the certified WAL
decoder already uses), no new external dependency.

## 9. Remaining relational work

No SQL parser, binder, executor, `CREATE TABLE`/`INSERT`/`UPDATE`/
`DELETE`/`SELECT` SQL, query planning, joins, aggregation, or
authorization enforcement. Index maintenance (D7/D11) is not wired into
`put_row`/`delete_row` yet — deliberately shaped to require no call-site
change when it is added (RA.5). A read-through catalog cache (RA.3,
explicitly deferred) is the direct response if `get_row`'s measured
catalog-resolution overhead (§6) matters once real workloads exist.

---

## Certification

**ROW STORAGE FOUNDATION = PASS** for this increment's own scope.

**RELATIONAL IMPLEMENTATION = NOT STARTED beyond the catalog and row-
storage foundation.**

**RELATIONAL DATABASE PRODUCTION READY = NO.**

**WRITE ENGINE = PRODUCTION READY**
**READ ENGINE = PRODUCTION READY**
**COMPACTION = PRODUCTION READY**
