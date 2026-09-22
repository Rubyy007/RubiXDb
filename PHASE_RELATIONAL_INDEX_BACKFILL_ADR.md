# PHASE_RELATIONAL_INDEX_BACKFILL_ADR — Production Secondary Indexes with Online `CREATE INDEX`

**Date:** 2026-09-23

**Status:** Implemented (Increment 5). Builds on the certified write engine,
read engine, and compaction (`PHASE_COMPACTION_CERTIFICATION.md`), the
certified catalog (`PHASE_RELATIONAL_CATALOG` work, commit `d66029d`), the
certified atomic `write_batch` primitive (`PHASE_RELATIONAL_TRANSACTION_
STORAGE_RESULTS.md`), and the certified row-storage foundation
(`PHASE_RELATIONAL_ROW_STORAGE_RESULTS.md`). Every decision below was
checked against the source actually in the repository at the time this
increment began (`src/catalog/`, `src/relational/`, `src/lsm/mod.rs`), not
against pasted historical text — see §0.

**Scope**: physical secondary-index storage, index-entry key encoding,
insert/delete maintenance, **online** `CREATE INDEX` (the table remains
writable throughout backfill), `DROP INDEX`, crash recovery, index
lookup/range scan. **Not in scope**: SQL parser/binder/planner/executor,
transactions, `UNIQUE` constraint *enforcement* (the physical structure is
built; enforcement requires D10's not-yet-implemented conflict detection,
per `PHASE_RELATIONAL_DATABASE_ADR.md` D7), CLI/API/frontend wiring.
**RELATIONAL DATABASE PRODUCTION READY = NO** after this increment (see
§15).

---

## §0. Source audit (what the increment actually found)

- `src/catalog/schema.rs`/`service.rs`: `IndexRow`/`IndexKind` already
  existed (`Primary`/`Unique`/`NonUnique`), and `create_index` already
  inserted a row with `state: Building` — but `IndexState` had only two
  variants (`Active`/`Building`) and no backfill ever ran; a `Building`
  index was permanently stuck (`PHASE_RELATIONAL_CATALOG_RESULTS`-era
  code, its own doc comment: "a future increment... is responsible for
  actually running D7's bounded backfill and flipping it to `Active`").
- `src/relational/table_store.rs`: `put_row`/`put_rows`/`delete_row` did
  **no** index maintenance at all — `RELATIONAL ADR AMENDMENT 003`'s own
  §9 stated this explicitly: "Index maintenance (D7/D11) is not wired
  into `put_row`/`delete_row` yet — deliberately shaped to require no
  call-site change when it is added."
- `src/lsm/mod.rs`: `LsmEngine::range_scan(start, end, as_of_seq)` already
  supports an explicit `as_of_seq` bound (not just `u64::MAX`), and
  `LsmEngine::snapshot()`/`Snapshot::seq()` already provide a registered,
  compaction-safe read-view handle (`SnapshotRegistry`, consulted by
  `compaction::merge` via `oldest_live_snapshot_seq()`). This is the exact
  primitive pair the online-build protocol below is built on — **no new
  storage-engine primitive was required or added** (verified: `git diff
  --stat -- src/wal/ src/manifest/ src/compaction/ src/sstable/` is empty
  for this increment, §13).
- **A latent, pre-existing bug found and fixed by this increment**:
  `relational::key::table_row_range(table_id)` bounded a table scan by
  `[table_row_key(table_id, []), table_row_key(table_id + 1, []))` — the
  *entire* `table_id` prefix. Before this increment, nothing else lived
  under that prefix besides `index_id = 0` (table) rows, so the bound was
  harmlessly over-wide. The instant a secondary index's entries
  (`index_id > 0`, same `table_id` prefix) exist, that bound silently
  includes them in a plain table scan — a real correctness defect this
  increment's own tests caught (see §1, §13).

---

## §1. Physical key layout — table-row range fix

**Decision**: fix `relational::key::table_row_range` to bound exactly
`[table_id||0, table_id||1)` (the `index_id = 0` slot only), never
`[table_id||0, (table_id+1)||0)`. Index-entry keys (`§2`) live at
`table_id||index_id` for `index_id > 0`, under the *same* `table_id`
prefix `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §5 already specifies.

**Reason**: this is the direct, structural consequence of index entries
sharing the table's own `table_id` prefix (by design — it is what makes
"drop this table" and "this table's indexes" both single contiguous
scans). The old bound's correctness depended on an invariant ("nothing
else lives under this `table_id` prefix") that secondary indexes
structurally break. Caught by `scan_table_boundaries_at_min_and_max_and_
neighboring_table_ids` failing once index-entry keys existed in the same
process — not found by inspection first, found by a test that constructs
the actual boundary key and checks it, exactly the kind of "verify
against the real implementation" the governing directive requires.

**Alternatives rejected**: *Leave `table_row_range` unchanged and instead
give indexes a disjoint top-level namespace byte* (e.g., `0x02` for
indexes instead of sharing `0x01`). Rejected: contradicts the already-
committed Architecture doc §5/§7 layout table, which is not this
increment's to relitigate, and loses the "one table's data + all its
indexes = one contiguous key region" property useful for a future `DROP
TABLE` sweep (D13) to reclaim both in one pass.

**Correctness impact**: `TableStore::scan_table`/`get_row`/`put_row`/
`delete_row` (all of which resolve a table's key range via this
function) can never again observe or corrupt another physical structure
(a secondary index) sharing the same `table_id`. **Testing
requirements**: a dedicated regression assertion that a constructed index
entry key for the same `table_id` sorts at/after the table range's own
`end` bound (added directly to `relational::tests::scan_table_boundaries_
at_min_and_max_and_neighboring_table_ids`); `u32::MAX`-table_id boundary
behavior changed from a `Bound::Unbounded` fallback to a precise
`Bound::Excluded` (strictly stronger — verified by an updated assertion,
not a weakened one).

---

## §2. Index entry physical key

**Decision**: `0x01 || table_id:u32 BE || index_id:u32 BE (>0) ||
encoded_indexed_columns || encoded_pk`, exactly as `PHASE_RELATIONAL_
DATABASE_ARCHITECTURE.md` §5 already specified and `PHASE_RELATIONAL_
DATABASE_ADR.md` D2/D7 already committed to. `index_id = 0` remains
reserved for the table's own row key (`relational::key::table_row_key`);
this module (`relational::index_key`) never constructs it.

**Reason**: this is not a new decision — it is the already-authoritative
layout from Increment 2's architecture document, implemented for the
first time now that indexes physically exist. Verified against the real
source (`relational::key::table_row_key`'s own layout) before writing a
single line of the new module, per the governing "verify the current
source and ADR" instruction.

**Correctness/Testing impact**: see §1 (the interaction this layout has
with `table_row_range`) and §3 (NULL/composite-column encoding within the
`encoded_indexed_columns` segment).

---

## §3. Index entry value / NULL semantics / composite indexes

**Decision**: the entry's **value** is an empty marker (`Vec::new()`) —
every piece of information an entry carries lives in its **key**
(indexed columns + primary key). Indexed (non-PK) columns may be `NULL`;
each is encoded with a 1-byte presence tag prefixing its own
`relational::key::encode_key_value` output: `0x00` = NULL (sorts before
every real value in that column position), `0x01 || <value bytes>` =
present. A composite index (`INDEX(a, b, c)`) is the concatenation of
each column's tagged encoding, in declared order, followed by the
already-existing `relational::key::encode_composite_key` primary-key
suffix (PK columns are never `NULL`, D5, so no tag is needed there).

**Reason**: "do not duplicate full row data in the index without
evidence the current ADR requires covering indexes" (`PHASE_RELATIONAL_
DATABASE_ADR.md` D7) — no such evidence exists; every read path is
index-then-fetch (§12). NULL ordering ("a reserved lowest-sorting marker
byte, consistently") is Architecture doc §5's own explicit text,
implemented literally: a presence tag is the simplest mechanism that
keeps every column position self-delimiting (matching the existing
TEXT/BLOB escape-terminate scheme's own "every field is unambiguous
regardless of position" property) while adding exactly one byte per
indexed column.

**Alternatives rejected**: *A single sentinel byte value reused from the
column's own type range to mean NULL* (e.g., treating `i32::MIN` as
"NULL" for an `INTEGER` column). Rejected outright — collides with a
real value, exactly the ambiguity D4's own row-value design already
rejected ("`NULL` is structural..., never a sentinel value inside a
typed column's own encoding") for the row-value envelope; the same
reasoning applies to the key encoding.

**Testing requirements** (all implemented, `relational::index_key::
tests`): NULL sorts before every real value (direct assertion, not
inference); NULL/present round-trip for every type; composite round-trip
with a NULL in a non-trailing position; a property test asserting
`encode` then byte-compare matches numeric compare for `INTEGER`;
truncated-buffer and invalid-presence-tag rejection without panicking;
an oversized-key rejection test (`MAX_INDEX_KEY_BYTES`, §14).

---

## §4–7. Online `CREATE INDEX` protocol

### §4. Timeline

```
T0  Building-state catalog row committed (one write_batch call)
T1  Backfill snapshot captured (engine.snapshot(), immediately after T0)
T2  Ordinary DML maintenance is active for this index from T0 onward
    (see §6 for the precise mechanism and proof)
T3  Backfill begins reading (as of T1) in bounded chunks
T4  Each chunk is re-validated against *current* truth and committed
    (see §7's proof — this is where backfill and concurrent maintenance
    are reconciled, not a separate pass)
T5  Building -> Ready, one atomic catalog write_batch call
```

### §5. Physical maintenance path (ordinary DML)

**Decision**: `TableStore::put_row`/`put_rows`/`delete_row` compute the
complete set of index-entry `WriteOp`s (one delete for the superseded
entry, unless byte-identical to the new one; one put for the new entry)
for every `Building`/`Ready`, non-`Primary` index, and include them in
the **same** `write_batch` call as the table row itself — D11, applied
for the first time now that indexes exist. A `Building` index is
maintained identically to a `Ready` one (never a separate code path) —
this is what makes backfill's own job bounded to "rows that existed
before maintenance started," never "every row for the rest of time."

**Reason**: D11's atomicity requirement is unconditional on index state —
an index a build is in progress for is exactly as entitled to "table and
index never durably disagree" as a finished one, or the backfill
protocol below has no sound boundary to reason about at all.

### §6. The "index epoch lock" — closing the maintenance-set race

**Decision**: one `RwLock<()>` per table (`TableStore::epoch_lock`,
lazily created, never held for a whole backfill/sweep). Ordinary writers
hold its **read** side for the critical section `{resolve maintained
indexes -> compute ops -> write_batch}`. Any operation that **changes**
which indexes must be maintained — `create_index`'s `Building`-row insert
(T0) and `mark_index_dropping` (§11) — holds its **write** side for just
that one `write_batch` call.

**Reason**: without this, a writer whose `list_indexes` call happens to
read the catalog *before* T0 commits, but whose own `write_batch` call
lands *after* T1 (the backfill snapshot boundary), would neither be
covered by backfill (its row's write happened after T1) nor by
maintenance (its own catalog read was stale) — a genuinely missed write,
exactly the race the governing directive names ("writer at T1/T2,
backfill around T1/T3... must mathematically prevent a missed write").
`RwLock` mutual exclusion (the *same* proof technique already used and
tested for `write_batch`'s own atomicity, `RELATIONAL ADR AMENDMENT 001`
AA.3) makes this structurally impossible: any writer's full critical
section either completes entirely before T0's write-lock section, or
starts entirely after it — never straddling it. A writer that completes
before T0 has a row seq `< T0 <= T1`, so it is inside backfill's own
scan (§7). A writer that starts after T0 performs its own `list_indexes`
call under the read lock, released only after T0's write-lock section
ends — so it is guaranteed to observe the `Building` row in the catalog
(catalog reads are always "current," never a stale cache) and performs
maintenance itself.

**Alternatives rejected**: *A global engine-wide lock around index
creation.* Rejected outright — explicitly forbidden ("do not add an
unnecessary global lock… do not serialize the whole table"). *No
synchronization, relying on catalog-read timing alone.* Rejected — proven
insufficient by the race above; this is exactly the missing primitive the
governing directive anticipates needing to add, and the smallest one that
closes it is an in-process lock, not a new storage-engine feature.

### §7. Backfill re-validation — closing the "phantom entry" race

**Decision**: backfill enumerates candidate primary keys from a *single*
snapshot (T1), in bounded chunks (`BACKFILL_CHUNK_ROWS = 500`, never the
whole table in memory). For **each chunk**, it acquires the table epoch
lock's **write** side, re-reads each candidate row's **current** (not
T1-snapshotted) state, and writes an entry only for a row still present
— skipping (never writing) a row that is now gone. The chunk's
`write_batch` commits, then the write lock releases, before the next
chunk is read.

**Reason — the race this closes**: a naive design that writes each
candidate row's *T1-snapshotted* value, with no further synchronization,
admits this counter-example: row R exists at T1 with value V (backfill
observes it); before backfill's chunk containing R is flushed, a
concurrent `DELETE` removes R — under §6's proof, this delete's own
maintenance correctly tombstones R's index entry, at some seq `S_del >
T1`. Backfill's chunk flush later writes a **stale** `Put` for R's T1
value, at some seq `S_backfill`, also `> T1`. Nothing orders `S_del`
before `S_backfill` in general (they are two independent, concurrently
submitted `write_batch` calls) — if `S_backfill > S_del`, the stale `Put`
wins the engine's ordinary last-write-wins resolution, **resurrecting an
entry for a row that no longer exists**. This is exactly the governing
directive's item 5 requirement ("No committed row is indexed incorrectly
because of the activation boundary") stated as a concrete failure, not an
abstraction.

Re-validating under the epoch **write** lock closes it completely, not
merely narrows it: while the write lock is held, no `put_row`/
`delete_row` critical section (which holds the *read* side) can be
mid-flight — by `RwLock` mutual exclusion, every maintenance write either
finished strictly before this chunk's re-validation began (so its effect
is exactly what the re-validating `get` observes) or has not yet started
(so it cannot race this chunk's commit at all; it will run after the
write lock releases, against whatever this chunk just wrote, using the
same last-write-wins rule — safe, since a real subsequent DML change is
*supposed* to win over an older backfilled entry). There is no window in
which a maintenance write's effect on a row already in the current chunk
is neither reflected in the re-validation read nor deferred until after
this chunk's commit.

**Alternatives rejected**: *Write backfill's entries directly from the
T1-snapshotted scan, with no re-validation.* Rejected — proven unsound
above. *Multiple successive re-scan passes, converging until a pass finds
nothing new (a "hot backup"-style convergence loop).* Rejected: still
admits the same race on its own final pass (an arbitrarily late delete
can still race an arbitrarily late final-pass flush), and is strictly
more complex and slower than a bounded, exact mechanism that has no such
residual window. *A new engine-level conditional/compare-and-swap write
primitive*, so backfill could commit "only if nothing changed since T1."
Considered and rejected as unnecessary: the epoch lock achieves the same
end (backfill's write always reflects genuinely current truth) using a
purely in-process synchronization primitive already justified for §6,
with no new storage-engine surface — the "smallest necessary primitive"
the directive asks for turned out to already exist in a form this
protocol could reuse, once the *scope* of exclusion was narrowed to one
chunk rather than the whole table.

**Cost, measured, not assumed**: re-validation doubles per-row read work
during backfill (one read from the T1-snapshotted scan iterator, one
live `get` at commit time) — see `PHASE_RELATIONAL_INDEX_INCREMENT5_
RESULTS.md` for the measured backfill throughput this costs.

**Testing requirements** (all implemented, `relational::index_tests`):
`row_deleted_during_backfill_leaves_no_phantom_entry` (the exact §7
counter-example, deterministic); `concurrent_writes_during_backfill_are_
never_missed` (barrier-synchronized concurrent insert/delete racing a
1,200-row, multi-chunk build, verified against an independent reference
— a fresh `scan_table`, never the production algorithm as its own
oracle); `reinsert_during_backfill_reflects_final_state_only` (delete +
reinsert with a different indexed value, racing the build — every
rewritten PK must appear under exactly one of the two values, never
both, never neither).

---

## §8. Crash recovery

**Decision**: any index found `Building` at process startup is
**restarted from scratch** — a fresh T1 snapshot, a fresh full backfill
pass — never resumed from a partial cursor, never silently promoted to
`Ready`. Any index found `Dropping` has its physical sweep restarted from
the beginning of its key range (§11) and its catalog row removed once
complete.

**Reason**: restart is idempotent and requires no new durable state
beyond the catalog row's own `state` field — a restarted backfill
re-derives every entry from current truth (§7's same mechanism), so
entries a prior, interrupted attempt already wrote are simply overwritten
with identical values (same key, same empty marker value); no
"was this row already backfilled" bookkeeping is needed or trusted.
Resume-from-a-cursor was considered and rejected as unjustified
complexity for this increment: it requires its own durable checkpoint
protocol (itself needing the same atomicity/ordering care as backfill
itself) for a benefit — avoiding re-scanning already-processed rows after
a crash — that is real but secondary to correctness, and this project's
own established discipline ("ship a simpler, documented v1 and defer the
harder general case," `PHASE_RELATIONAL_DATABASE_ADR.md` D4's own stated
precedent) applies directly. The known cost (a crash-prone workload
re-pays full backfill cost on every restart) is stated plainly here, not
hidden.

**Testing requirements**: `recover_incomplete_builds_restarts_from_
scratch_and_activates` (a `Building` row with zero prior backfill
activity — the worst case — recovers to `Ready` with every row
correctly indexed); `recover_incomplete_drops_completes_the_sweep`
(a `Dropping` row with entries still present recovers to fully removed).
Both simulate "the process crashed mid-operation" by constructing the
exact intermediate catalog state directly (bypassing `IndexBuilder`, so
no backfill/sweep code ever actually ran before recovery), which is
indistinguishable, from recovery's point of view, from a real crash at
that point — recovery has no way to observe *how* a `Building`/`Dropping`
row came to exist, only that it does.

---

## §9. Catalog state machine

**Decision**: `IndexState::{Ready, Building, Failed, Dropping}` (§0: the
prior two-state `{Active, Building}` renamed/extended — `Active`
renamed to `Ready`, same numeric tag `0`, so a `PRIMARY`-kind index row
persisted by a prior increment decodes unchanged). Legal transitions:
`Building -> Ready` (T5, `mark_index_ready`), `Building -> Failed`
(`mark_index_failed`, a build that errors), `{Building, Ready, Failed} ->
Dropping` (`mark_index_dropping`, never from `Primary` — guarded), and
final catalog-row removal only from `Dropping` (`remove_index_row`).
Every transition is one `write_batch` Put of the row with only `state`
changed. Only a `Ready` index is ever query-usable (`IndexBuilder::
ready_index`, enforced before any lookup/range scan) — a `Building`
index must never be used by normal reads (governing directive item 8),
and a `Failed`/`Dropping` one is never usable either (both excluded from
`TableStore::maintained_indexes`, so neither receives new writes, and
neither is `ready_index`-eligible).

**Reason**: the catalog is the sole authoritative registry (governing
directive item 9: "Do not duplicate index state in another authoritative
registry") — no second in-memory "which indexes are ready" table exists
anywhere; every check (`maintained_indexes`, `ready_index`) is a fresh
catalog read, exactly `RELATIONAL ADR AMENDMENT 002`'s own "no separate
catalog cache is required for correctness" precedent, applied to index
state specifically.

**Testing requirements**: `index_row_round_trips_every_state` (all four
states persist/decode correctly, `catalog::schema::tests`);
`building_index_is_not_query_usable`; `dropping_index_stops_receiving_
new_writes`.

---

## §10. `UNIQUE` index physical representation

**Decision**: `IndexKind::Unique` produces the **identical** physical
entry format as `NonUnique` (indexed columns + PK, no deduplication
structure). No uniqueness *enforcement* is implemented in this increment.

**Reason**: `PHASE_RELATIONAL_DATABASE_ADR.md` D7 states this precisely —
"if current transaction/conflict infrastructure is not yet present, do
NOT pretend concurrent UNIQUE enforcement is solved... full UNIQUE
enforcement remains a hard requirement for the later transaction layer."
D10 (Snapshot Isolation + commit-time conflict detection) does not exist
yet; building a "check-then-write" uniqueness path on top of `write_batch`
alone would be exactly the race-prone pattern the governing directive
explicitly forbids ("Do not create a race-prone check -> then write
uniqueness implementation").

**Status**: `UNIQUE ENFORCEMENT = NOT YET CERTIFIED` (see §15's final
status table) — physical structure only, stated honestly, not oversold.

---

## §11. `DROP INDEX` — bounded, resumable physical sweep

**Decision**: mirrors D13's `DROPPING`-table precedent. `mark_index_
dropping` commits under the table epoch's **write** lock (§6 — the
maintenance-set change needs the identical exclusion argument as `T0`'s
activation, mirrored). After that commit, `sweep_index_entries` deletes
every physical entry in bounded chunks (`SWEEP_CHUNK_ROWS = 1,000`),
resuming each chunk from just past the previous chunk's last deleted key
(never re-scanning an already-tombstoned prefix — `O(entry_count)` total
work, not `O(entry_count² / chunk_size)`). No further synchronization is
needed during the sweep: by construction (§6's mutual-exclusion argument)
no writer can add a new entry for a `Dropping` index once the transition
has committed. Once the sweep finds nothing left, `remove_index_row`
deletes the catalog row.

**Reason**: an unbounded single `write_batch` deleting a large index's
entire keyspace would violate `write_batch`'s own `max_batch_ops` ceiling
and the "avoid unbounded memory/resource paths" principle this project
applies everywhere else (D13's own reasoning for `DROP TABLE`, reused
verbatim here for `DROP INDEX`). The resume-from-last-key mechanism
(rather than always restarting the scan from the beginning) was chosen
specifically because index removal can be large and this is a purely
local, essentially free optimization — unlike backfill's restart-only
recovery policy (§8), there is no correctness reason to prefer restart
here, so the strictly cheaper mechanism is used.

**Testing requirements**: `drop_index_online_removes_catalog_row_and_
entries` (verified against a **raw engine range scan**, not merely "the
catalog says it's gone" — physical presence is checked directly);
`dropping_index_stops_receiving_new_writes`; `recover_incomplete_drops_
completes_the_sweep` (§8).

---

## §12. Index lookup / range scan (index-then-fetch)

**Decision**: `IndexBuilder::index_lookup` (equality/prefix — 1..=N
leading indexed columns) and `index_range_scan` (arbitrary `Bound`s over
the leading indexed columns) both resolve via the certified `LsmEngine::
range_scan` over the index's own physical key range (§2), decode each
entry's primary key, then fetch the full row via `TableStore::get_row` —
never a table-wide scan for an indexed predicate.

**Reason**: this is the direct, mechanical implementation of D7's own
"index-then-fetch" access-path decision — no new access-path design
needed, just the concrete encode/decode/range-bound plumbing (§2/§3)
connecting a caller's typed predicate to the certified range-scan
primitive.

**A known, accepted residual gap, stated plainly**: a row can be deleted
between an index entry being read and the corresponding `get_row` fetch
(no active transaction/snapshot pins the read across both steps in this
increment) — `scan_entries` handles this by silently omitting that
result, not erroring; this is the ordinary, expected index-then-fetch
race under no isolation, and is removed only once D10's transaction
layer exists (exactly as `PHASE_RELATIONAL_DATABASE_ADR.md` D7's own
Testing requirements note).

**Testing requirements**: `create_index_online_backfills_pre_existing_
rows`, `ordinary_insert_after_ready_is_maintained_atomically`,
`delete_removes_index_entry_atomically`, `upsert_changing_indexed_value_
moves_the_entry`, `null_indexed_value_is_indexed_and_looked_up_as_null`,
`composite_index_prefix_lookup_and_range_scan` — see `PHASE_RELATIONAL_
INDEX_INCREMENT5_RESULTS.md` for the full list and measured lookup/scan
performance vs. a full table scan.

---

## §13. Protected-path audit

```
git diff --stat -- src/wal/ src/manifest/ src/compaction/ src/sstable/ api/
```
empty for every path (verified at the end of this increment). No new
storage-engine primitive was required — `LsmEngine::snapshot`/`range_scan
(..., as_of_seq)`/`write_batch` (all already certified) are the complete
set this protocol is built from.

---

## §14. Resource limits

| Limit | Value | Enforced |
|---|---|---|
| Max indexes per table | `catalog::service::MAX_INDEXES_PER_TABLE = 64` | `CatalogService::create_index`, before allocation |
| Max columns per index | `catalog::service::MAX_COLUMNS_PER_INDEX = 16` | `CatalogService::create_index`, before allocation |
| Max index entry key size | `index_key::MAX_INDEX_KEY_BYTES = 8 KiB` | `index_key::index_entry_key`, before the key is returned to any caller |
| Backfill chunk size | `index::BACKFILL_CHUNK_ROWS = 500` | streaming, never materializes the whole table |
| Sweep chunk size | `index::SWEEP_CHUNK_ROWS = 1,000` | streaming, never materializes the whole index |
| Max concurrent index builds (this process) | `index::MAX_CONCURRENT_INDEX_BUILDS = 4` | `IndexBuilder::acquire_build_slot`, an `AtomicUsize`-backed bounded slot, released on `Drop` even if the build errors |

None of these depends on a decoded on-disk integer for its own
allocation sizing (governing directive's resource-exhaustion-security
requirement) — every `Vec::with_capacity` in the new code is sized from
already-materialized, caller-supplied data (column counts bounded by the
table above, key lengths from already-built `Vec<u8>`s), never from an
unvalidated length read off disk.

---

## §15. Final status

Preserved unchanged: **WRITE ENGINE = PRODUCTION READY**, **READ ENGINE =
PRODUCTION READY**, **COMPACTION = PRODUCTION READY**.

**RELATIONAL DATABASE PRODUCTION READY = NO** — no SQL parser, binder,
planner, executor, transactions, CLI, or authorization enforcement exists
yet; this increment adds secondary-index storage and online maintenance
only. Full per-item PASS/FAIL matrix: `PHASE_RELATIONAL_INDEX_
INCREMENT5_RESULTS.md`.
