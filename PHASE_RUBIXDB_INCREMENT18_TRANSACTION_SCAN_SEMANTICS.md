# Increment 18 (Part 3): Transaction Scan Semantics

Companion to `PHASE_RUBIXDB_INCREMENT18_ACCESS_PATH_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT18_MATERIALIZATION_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT18_PERFORMANCE.md` and
`PHASE_RUBIXDB_INCREMENT18_RESULTS.md`. Append-only: the Increment 9, 11, 16
and 17 records that describe the previous behaviour as a "scope boundary" are
unchanged historical evidence; this document supersedes them.

## 1. The question

Increment 17 listed as open: *SeqScan / IndexScan do not overlay a
transaction's own uncommitted writes, while PK lookup does.* The mandate
asked first for the **authoritative contract**, not an assumption that this
is a bug.

## 2. What the project's own documents say

| Source | Statement |
|---|---|
| `PHASE_RELATIONAL_DATABASE_ADR.md` D10 | Snapshot Isolation, client-buffered write set. "`BEGIN` -> `snapshot()`; reads resolve against that snapshot's seq; writes buffer in-memory." |
| `PHASE_RELATIONAL_TRANSACTION_ARCHITECTURE.md` section 2 | The transaction's read path is "local overlay first, snapshot-pinned engine read second. **Every read a transaction performs** -- including the ones commit-time validation itself issues internally -- resolves against this same snapshot seq or the current committed state, never anything in between." (the section describes `Transaction::get_row`, the only read primitive that existed) |
| Increment 7 / Increment 9 executor architecture | Item 36: read-your-own-writes is a required property; "do not implement a second write overlay inside the SQL executor. Use `Transaction`." The tested example is a PK lookup. |
| `PHASE_RELATIONAL_AGGREGATION_ARCHITECTURE.md` section 12 | Records that scans do **not** see own writes as a *"known limitation... pre-existing, architecture-documented scope boundary"*, not as an intended semantic, and notes `ExecCtx`'s doc comment as its source. |

No document states that scans are *intended* to exclude a transaction's own
writes. The exclusion arose because scan primitives (`scan_table_rows_as_of`,
`index_lookup_as_of`) were added later for the executor with a snapshot
sequence and no write-set parameter, and was then recorded as a boundary.
The authoritative contract (D10 + section 2) is Snapshot Isolation where a
transaction sees its own writes in *every* read. The current behaviour
contradicts the project's own transaction model.

## 3. Reproduction on HEAD (`bc80c79`), before any change

`sql/src/txn_scan_tests.rs`, one transaction over a 6-row table:

| Scenario | Observed on HEAD |
|---|---|
| `INSERT` then scan (PK range, seq scan, index equality, index range, composite, PK range + index) | the new row **absent** from every scan; present in a PK lookup |
| `UPDATE` an existing row (indexed and unindexed columns) then scan | scans return the **old** row version |
| `DELETE` a base row then scan | scans still return the deleted row |
| `INSERT -> UPDATE -> DELETE` | the intermediate states never visible to scans |
| **`UPDATE ... WHERE a = 1` after two inserts of `a = 1` rows in the same transaction** | `rows_affected` **3 instead of 5**: the statement silently skipped the transaction's own rows |

The last row is why this is a correctness problem and not a cosmetic one:
`UPDATE`/`DELETE` find their target rows through the same scan operator, so
in a multi-statement transaction they silently skipped rows the transaction
had inserted (demonstrated by the 3-of-5 result above). By the same
mechanism -- the scans in the first four rows demonstrably return stale base
rows -- they would also target rows the transaction had already deleted and
evaluate predicates against pre-update values; those two consequences follow
from the scan results and were not run as separate DML reproductions.
Five of six
reproductions failed; the sixth (other transactions and autocommit never see
uncommitted writes) correctly passed.

## 4. Decision

**Implement the overlay.** The authoritative model requires it, the
current behaviour produces silently wrong DML in multi-statement
transactions, and the fix is bounded. Nothing about the *snapshot* contract
changes: reads still resolve against the transaction's pinned snapshot; the
overlay only adds the transaction's own buffered writes, and never exposes
any other transaction's.

## 5. Semantics

For a transaction with base snapshot `S` and write set `W`, a read of table
`T` observes exactly:

```
visible(T) = ( base(T, S)  minus  keys written by the transaction )
             union  ( the transaction's latest Put for every key it wrote )
```

with last-write-wins per key (the write set already collapses
`INSERT -> UPDATE -> DELETE` to the final op), each key appearing at most
once, and the *complete* access predicate applied to every row regardless of
origin. Consequences, all tested:

- inserted row: visible iff it satisfies the predicate;
- updated row: only the new version is visible; a row whose update moves it
  out of (or into) a predicate leaves (enters) the result accordingly,
  including for **index** predicates on the changed column;
- deleted row: invisible; delete-then-reinsert shows the new row;
- PK order is preserved for scans that deliver it; an `ORDER BY` an index
  order was eliminating is re-established with the same comparator;
- no other transaction, and no autocommit statement, sees any of it;
- `UPDATE`/`DELETE` target-finding sees it too (and is Halloween-safe: the
  operator is drained into a bounded target list before any mutation, and the
  overlay is an immutable snapshot taken when the operator is built).

## 6. Design

`Transaction::overlay_for(table_id) -> Option<Arc<TableOverlay>>`
(`src/relational/txn.rs`): an immutable, **key-ordered** snapshot of one
table's slice of the write set (`OverlayEntry { encoded_pk, pk_values,
row: Option<Row> }`, `None` = buffered delete). Built lazily on first use and
**cached until the transaction next writes** (`write_version`, bumped at the
one choke point `record_op`), so a correlated join pays for it once, not per
outer row. Returns `None` -- one hash probe -- when the transaction has not
written the table, which is the common case. Bounded by
`TxnLimits::max_write_set_ops` (one entry per distinct key touched). It never
reads the engine.

`sql/src/exec/access_op.rs` applies it in the one operator every access
shares:

| Access | How the overlay is applied |
|---|---|
| `PkLookup` | unchanged: `Transaction::get_row` already overlays |
| `SeqScan`, `PkRangeScan`, table-scan fallback (lazy) | one-pass merge of the PK-ordered base scan with the key-ordered overlay (`compare_pks`: the key encoding is order-preserving by design, so decoded-value order equals physical order); overlay rows are tested against the complete predicate |
| `IndexScan` (lazy fetch) | base entries whose key the transaction rewrote are dropped **before any row is fetched**; the transaction's own Put rows are visited after the base fetch is exhausted and tested against the complete predicate |
| ordered `IndexScan` / ordered fallback (a `Sort` was eliminated) | materialized (bounded), merged, and sorted ascending/`NULLS FIRST` with ties by primary key -- the physical index order |

To make "tested against the complete predicate" possible, `PkRangeScan` now
carries the complete predicate (`IndexFallback`) like `IndexScan` does (its
range conjuncts are consumed by the bounds). Row-origin matters: base rows
are filtered by the access's residual, overlay rows by the complete
predicate.

Cost: O(w) per scan for a transaction holding w writes to the table (one
clone and predicate evaluation per overlay row), O(1) otherwise. Measured in
the performance document: within 4% for every query shape up to w = 100;
at w = 1,000 an index lookup of 100 rows goes 1.49 -> 2.26ms; a full
sequential scan is unaffected (<= 3%). The state is bounded by the existing
write-set limit; nothing is added to `commit` or to the write path (one
integer increment per buffered write).

## 7. Evidence

- `txn_scan_tests.rs`: the six deterministic scenarios (5 failed on HEAD),
  each checking 11 query shapes under `Auto`, `ForceIndex` and `ForceSeq`
  plus COUNT and PK lookups against an independent `BTreeMap` model.
- A randomized property (16 proptest cases x 90 steps + 6 seeds x 250
  steps): a long-lived transaction performs random inserts, updates and
  deletes by PK, DML through index/scan predicates with **exact target
  counts**, while other writers commit changes it must not see and the index
  is dropped and rebuilt underneath it (F-2 combined with the overlay), and
  7 automatic Compactions overlap. Every scan shape under every access path
  must equal the model; at the end a fresh transaction must see exactly the
  outside world.
- **Mutation check:** making `overlay_for` return `None` fails 8 of the 9
  transaction tests (the ninth asserts that *other* transactions never see
  uncommitted writes, which correctly keeps passing); the original code is
  restored.
- `materialization_tests::limit_composes_with_the_transaction_overlay`.

## 8. What did not change

Isolation level (Snapshot Isolation, with its documented write-skew
limitation), conflict detection at commit, the write path, durability,
recovery, MVCC, and every other transaction's view.
