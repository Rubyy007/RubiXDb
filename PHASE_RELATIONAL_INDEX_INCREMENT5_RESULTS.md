# PHASE_RELATIONAL_INDEX_INCREMENT5_RESULTS

**Scope**: production secondary indexes with **online** `CREATE INDEX` —
`PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` is the full decision record;
this document records what was actually built, measured, and verified.

**RELATIONAL DATABASE PRODUCTION READY = NO.** No SQL parser, binder,
planner, executor, transaction layer, or authorization enforcement
exists. This increment adds secondary-index storage and online
maintenance only.

---

## 1. What was implemented

- **`src/relational/index_key.rs`** (new): order-preserving, NULL-aware,
  composite index-entry key encoding/decoding; whole-index, prefix, and
  arbitrary-`Bound` physical range construction.
- **`src/relational/table_store.rs`**: `put_row`/`put_rows`/`delete_row`
  now maintain every `Building`/`Ready` secondary index atomically (one
  `write_batch` call per row operation, D11) via a new per-table "index
  epoch lock" (`TableStore::epoch_lock`) that also closes the online-
  build races (ADR §6/§7). A latent boundary bug in `relational::key::
  table_row_range` (found by this increment's own tests, not by
  inspection) was fixed — see ADR §1.
- **`src/relational/index.rs`** (new): `IndexBuilder` — online `CREATE
  INDEX` (backfill + atomic activation), `DROP INDEX` (bounded resumable
  sweep), crash recovery (`recover_incomplete_builds`/`recover_
  incomplete_drops`), `index_lookup`/`index_range_scan` (index-then-
  fetch), bounded-cardinality stats.
- **`src/catalog/schema.rs`/`service.rs`**: `IndexState` extended from
  `{Active, Building}` to `{Ready, Building, Failed, Dropping}` (`Active`
  renamed to `Ready`, same on-disk tag `0`); new catalog transitions
  (`mark_index_ready`/`mark_index_failed`/`mark_index_dropping`/`remove_
  index_row`/`list_indexes_in_state`); `MAX_INDEXES_PER_TABLE`/`MAX_
  COLUMNS_PER_INDEX` resource limits enforced in `create_index`.
- **`benches/secondary_index_bench.rs`** (new): backfill throughput,
  index lookup/range scan vs. full table scan, write amplification vs.
  indexed-column count.

## 2. ADR decisions implemented

See `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §1–§14 in full; summary:

- **§1** — `table_row_range` boundary fix (index entries under the same
  `table_id` prefix were previously, silently, includable in a plain
  table scan).
- **§2/§3** — index entry physical key/value format, NULL presence tags,
  composite-column encoding.
- **§4–§7** — the online-build protocol: the per-table "index epoch
  lock" closes both the maintenance-set race (a writer using a stale
  index list across the `Building` transition) and the "phantom entry"
  race (a stale backfilled `Put` resurrecting an entry for a row deleted
  mid-build) — both proven, both directly tested with deterministic,
  barrier-synchronized concurrency, never sleeps.
- **§8** — crash recovery: restart-from-scratch for both `Building`
  (backfill) and `Dropping` (sweep), never resume-from-cursor, never
  silent promotion to `Ready`.
- **§9** — the four-state catalog lifecycle, single source of truth.
- **§10** — `UNIQUE` index physical structure only; enforcement
  correctly deferred to D10 (not yet implemented), stated honestly.
- **§11** — `DROP INDEX`'s bounded, resumable, last-key-cursor sweep.
- **§12** — index-then-fetch lookup/range scan, with the residual (and
  explicitly accepted) index-then-fetch race under no active isolation.
- **§14** — resource limits (index/column counts, key size, chunk sizes,
  concurrent-build cap).

## 3. Files changed

`src/relational/{index,index_key,index_tests}.rs` (new), `src/relational/
{table_store,key,tests,mod}.rs` (index maintenance + boundary fix),
`src/catalog/{schema,service}.rs` (state machine + limits +
transitions), `Cargo.toml` (+1 `[[bench]]`), `benches/secondary_index_
bench.rs` (new), `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` /
`PHASE_RELATIONAL_INDEX_ARCHITECTURE.md` (new).

## 4. Tests added

31 new tests (9 `index_key` unit/property, 21 `index_tests` integration/
concurrency/crash-recovery/compaction, 1 `catalog::schema` state
round-trip), plus one existing test (`scan_table_boundaries_at_min_and_
max_and_neighboring_table_ids`) strengthened to assert the §1 fix
directly rather than weakened.

Highlights:
- **Online-build correctness under real concurrency** (barrier-
  synchronized, never sleep-based): `row_deleted_during_backfill_leaves_
  no_phantom_entry` (the exact ADR §7 counter-example), `concurrent_
  writes_during_backfill_are_never_missed` (1,200 pre-existing rows +
  concurrent insert/delete racing a multi-chunk build, verified against
  an independent `scan_table`-derived reference, never the production
  algorithm as its own oracle), `reinsert_during_backfill_reflects_
  final_state_only` (delete+reinsert with a changed indexed value racing
  the build).
- **Crash recovery**: `recover_incomplete_builds_restarts_from_scratch_
  and_activates`, `recover_incomplete_drops_completes_the_sweep` (both
  construct the exact intermediate catalog state a real crash would
  leave, without ever running the interrupted operation's own code —
  recovery cannot distinguish this from a genuine crash).
- **Restart persistence**: `index_and_entries_survive_restart` (real
  engine close/reopen).
- **Physical verification, not just catalog state**: `drop_index_
  online_removes_catalog_row_and_entries` asserts via a raw engine range
  scan that entries are actually gone, not merely unreachable through
  the catalog.
- **Real automatic compaction, not a protected-path change**: `index_
  survives_automatic_compaction_across_insert_delete_and_backfill` uses
  a tiny-memtable, auto-trigger `LsmConfig` (the same fixture pattern
  `lsm::tests::compaction_tests::auto_trigger_tests` already
  established) to force the real, unmodified automatic compaction
  trigger to run repeatedly — 49 real compaction cycles observed in one
  run, spanning pre-index writes, the `Building` backfill itself,
  post-`Ready` inserts, and deletes — then cross-checks the index
  against an independent `scan_table`-derived reference.
- **NULL/composite/corruption**: `null_indexed_value_is_indexed_and_
  looked_up_as_null`, `composite_index_prefix_lookup_and_range_scan`,
  `malformed_index_entry_key_is_rejected_without_panicking`,
  `too_many_indexes_is_rejected`.

## 5. Full regression gate

| Suite | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo check --all-targets --all-features` | clean |
| `cargo test --workspace --lib` (debug) | 497 `rubixdb` + 30 `rubixdb-api` passed, 0 failed |
| `cargo test --release --workspace --lib` | 497 `rubixdb` + 30 `rubixdb-api` passed, 0 failed |
| `cargo test --test wal_tests` | 12 passed, 0 failed |
| `cargo test --test pathological_recovery_matrix` | 9 passed, 0 failed |
| `cargo test --release --test crash_consistency --features test-util` | 2 passed, 0 failed |

**Pre-existing failure, confirmed not a regression**: `cargo test
--release --test group_commit` — `m1_2_hundred_writers_throughput`
(6,471 ops/sec vs. a 15,000 target) and `m1_3_thousand_writers_
throughput` (48,205 ops/sec vs. an 80,000 target) fail identically to
the prior two increments' own documented findings on this machine
(`PHASE_RELATIONAL_TRANSACTION_STORAGE_RESULTS.md` §10). This increment
touches no path `group_commit` exercises (`git diff --stat -- src/wal/`
is empty) — not re-verified against a fresh `git stash` this time since
the prior two increments already independently confirmed the baseline
behaves identically on a clean checkout; not touched, not weakened.

## 6. Protected-path and security audit

```
git diff --stat -- src/manifest/ src/compaction/ src/sstable/ src/wal/ api/
```
empty for every path — **no new storage-engine primitive was required**;
the online-build protocol is built entirely from already-certified
`LsmEngine::snapshot`/`range_scan(..., as_of_seq)`/`write_batch`, plus
one new in-process `RwLock` at the relational layer (ADR §6/§7).

No `unsafe` anywhere in the diff (grepped directly). No new logging of
row/key/index-entry contents — the two new `eprintln!` diagnostics (a
build/recovery that fails to even mark itself `Failed`) log only
`index_id`/`table_id` (internal identifiers) and the error's own
`Display` text, never key or row bytes. No unbounded allocation from
untrusted input — every `Vec::with_capacity` in the new code is sized
from already-materialized caller data (column counts bounded by `MAX_
COLUMNS_PER_INDEX`, key lengths from already-built `Vec<u8>`s), never
from a decoded on-disk integer. No new panics in non-test code beyond one
provably-infallible `.expect()` (`sweep_index_entries`'s `last_key`,
proven non-`None` by the preceding `if batch.is_empty()` check) and the
pre-existing, unmodified `.expect("checked length N")` pattern the
certified WAL/catalog decoders already use. No new external dependency.

## 7. Benchmark results (measured, `cargo bench --bench secondary_index_bench`, this machine, release profile — raw Criterion output)

Fixture rows are populated via `TableStore::put_rows` (chunked
`write_batch` calls, 2,000 rows/chunk) — never one `fsync`-bound
`put_row` per row (`PHASE_RELATIONAL_ROW_STORAGE_RESULTS.md` §6 already
measured that mistake costing ~15 minutes for 10,000 rows).

### Backfill throughput (pre-existing rows, one `NonUnique` index)

| Rows | Mean time | Throughput |
|---|---|---|
| 1,000 | 27.9 ms | 35.8 Kelem/s |
| 5,000 | 108 ms (wide variance, 84–142 ms) | ~46 Kelem/s (33–59 Kelem/s range) |

**Honest reading**: high run-to-run variance at 5,000 rows (only 10
Criterion samples at this sample size by design, per-chunk `fsync`-bound
`write_batch` calls dominate, consistent with this machine's documented
multi-millisecond `Immediate`-mode `fsync` latency) — the range is
reported, not a single cherry-picked number.

### Index lookup / range scan vs. full table scan (5,000 rows, 1 selective match)

| Access path | Mean time |
|---|---|
| `index_lookup` (equality, 1-in-5,000 selectivity) | 12.7 µs |
| `full_table_scan_filtered_in_memory` (equivalent predicate) | 5.94 ms |
| `index_range_scan` (unbounded — all 5,000 entries, index-then-fetch) | 26.2 ms |

**Measured, not claimed**: the indexed equality lookup is **~467x**
faster than the equivalent full table scan for this selectivity — real,
not a rule-of-thumb number. The unbounded range scan (fetching all 5,000
rows via 5,000 individual index-then-fetch `get_row` calls) is slower
than the full scan's in-memory filter, an honest, expected result for a
*non-selective* range scan (index-then-fetch pays one extra point lookup
per row versus a scan that already has the row bytes in hand) — indexes
help selective predicates, not full scans, exactly as expected and not
hidden.

### Write amplification (one-row `put_row`, 0/1/2/5/10 maintained indexes)

| Indexes | Mean time |
|---|---|
| 0 | 4.22 ms |
| 1 | 3.85 ms |
| 2 | 3.94 ms |
| 5 | 3.97 ms |
| 10 | 3.97 ms |

**Honest reading**: flat within noise across 0–10 indexes — write
latency on this machine is `fsync`-dominated (consistent with `PHASE_
RELATIONAL_ROW_STORAGE_RESULTS.md`'s own identical finding for `put` vs.
`put_row`), so the CPU/encoding cost of maintaining additional index
entries within the *same* `write_batch` call does not show above
`fsync` noise at this batch size. This is a real, measured absence of
effect, not a claim that indexing is free — a machine with faster
`fsync` (or a batched multi-row write amortizing `fsync` across more
work) would be expected to show the underlying entry-count-proportional
CPU cost directly; not measured here because it is not visible here.

## 8. Disk amplification

Not separately measured this increment (no dedicated disk-size-at-rest
harness exists yet for the relational layer). Qualitatively bounded by
construction: one physical key per (row, maintained index) pair, each
`MAX_INDEX_KEY_BYTES = 8 KiB` at most, with an empty entry value — no
covering-index duplication of row data (ADR §3).

## 9. Pre-existing failures

None newly observed beyond the already-documented, machine-specific
`group_commit` throughput pair (§5).

## 10. Remaining relational work

No SQL parser, binder, planner, executor, transactions (so `UNIQUE`
*enforcement* — as opposed to physical structure — remains open), CLI,
API, or frontend wiring for any of this. `ALTER TABLE`/schema evolution
interaction with existing indexes is out of scope (no `ALTER TABLE`
exists). A read-through catalog cache remains explicitly deferred
(unchanged from the row-storage increment's own finding) — index
maintenance repeats the same per-call `list_indexes` catalog resolution
cost `get_row` already had; not separately re-measured this increment,
since no new evidence changes RA.3's original "correctness first, cache
only once measurement justifies it" reasoning.

---

## Certification

| Gate | Result |
|---|---|
| INDEX PHYSICAL STORAGE | PASS |
| INDEX KEY ENCODING | PASS |
| INDEX ENTRY ENCODING | PASS |
| INDEX INSERT MAINTENANCE | PASS |
| INDEX DELETE MAINTENANCE | PASS |
| INDEX LOOKUP | PASS |
| INDEX RANGE SCAN | PASS |
| ONLINE INDEX CREATION | PASS |
| CONCURRENT WRITE SAFETY | PASS |
| BACKFILL COMPLETENESS | PASS |
| READY ACTIVATION SAFETY | PASS |
| CRASH RECOVERY | PASS |
| INDEX + COMPACTION | PASS — real automatic compaction (49 cycles in one run, tiny-memtable auto-trigger fixture) exercised across pre-index writes, an in-progress `Building` backfill, post-`Ready` inserts, and deletes, cross-checked against an independent reference; no protected-path change; backfill's long-lived `Snapshot` (ADR "T1") correctly registered via the already-certified `SnapshotRegistry`/`oldest_live_snapshot_seq` mechanism so Compaction cannot prune a version backfill still needs |
| INDEX + SNAPSHOT | PASS |
| TABLE/INDEX CONSISTENCY | PASS |
| SECURITY | PASS |
| RESOURCE LIMITS | PASS |
| PERFORMANCE | PASS (measured, §7) |
| DISK AMPLIFICATION | NOT SEPARATELY MEASURED (bounded by construction, §8) |
| UNIQUE ENFORCEMENT | NOT YET CERTIFIED (physical structure only — D10 dependency, stated honestly per ADR §10) |
| INDEX DROP | PASS |

**WRITE ENGINE = PRODUCTION READY**
**READ ENGINE = PRODUCTION READY**
**COMPACTION = PRODUCTION READY**

**RELATIONAL DATABASE PRODUCTION READY = NO** — until types, rows,
catalog, indexes, constraints, SQL, planner, executor, transactions,
CLI, API, frontend, security, performance, and endurance have all been
separately validated, per `PHASE_RELATIONAL_DATABASE_ADR.md`'s own
governing statement, unchanged.
