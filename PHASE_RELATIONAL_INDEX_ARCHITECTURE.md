# RubiXDB — Secondary Index Architecture (Increment 5)

**Date:** 2026-09-23. Structural map only — reasoning and proofs live in
`PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md`; this document is the map, not
the record, mirroring `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md`'s own
relationship to its ADR.

## Module layout

```
src/relational/
  index_key.rs   physical key encoding/decoding for index entries
                 (order-preserving, NULL-aware, composite) — pure
                 functions, no engine dependency
  table_store.rs put_row/put_rows/delete_row gain index maintenance
                 (D11, one write_batch per row op); owns the per-table
                 "index epoch lock" (TableStore::epoch_lock)
  index.rs       IndexBuilder — online CREATE INDEX, DROP INDEX, crash
                 recovery, index_lookup/index_range_scan, bounded-
                 cardinality stats
```

## Physical layout

```
Table row key   := 0x01 || table_id:u32 BE || 0x00000000 || encoded_pk
Index entry key := 0x01 || table_id:u32 BE || index_id:u32 BE(>0)
                     || encoded_indexed_columns || encoded_pk
```

`encoded_indexed_columns` is a NULL-aware, order-preserving tuple
encoding: each column is a 1-byte presence tag (`0x00` = NULL, `0x01` =
present) followed by `relational::key::encode_key_value`'s ordinary
output. An index entry's *value* is empty — every entry is a pure key,
looked up then fetched from the table (index-then-fetch), never a
covering index.

## Index lifecycle

```
system.indexes.state:  Building --(backfill completes)--> Ready
                            |
                            v (drop)
                        Dropping --(sweep completes)--> [row removed]
                            ^
                            |
                          Failed  (a build that errored; terminal)
```

Only `Ready` is query-usable. `Building` and `Ready` are both
*maintained* (every `INSERT`/`DELETE` updates them) — the distinction is
purely about read eligibility, not write eligibility.

## Online `CREATE INDEX`

```
T0  catalog row inserted Building        (table epoch write-lock, brief)
T1  engine.snapshot() captured            (immediately after T0)
T2  ordinary DML maintenance now covers this index for every write from
    here on, proven by the epoch lock (ADR §6)
T3  backfill reads the table as of T1, in 500-row chunks
T4  each chunk is re-validated against *current* truth under a brief
    table epoch write-lock, then committed (ADR §7 — this is what
    prevents a stale, already-superseded entry from ever landing)
T5  catalog row -> Ready                  (one write_batch call)
```

The table is writable at every point in this timeline; the only
exclusive sections are T0's one `write_batch` call and, per backfill
chunk, the brief re-validate-and-commit step — never the whole build.

## `DROP INDEX`

```
mark Dropping (table epoch write-lock, brief; stops all new maintenance
  writes for this index immediately)
  -> bounded, resumable-by-restart physical sweep (1,000-entry chunks,
     each chunk resuming from the last deleted key — no re-scan of
     already-tombstoned prefix)
  -> catalog row removed
```

## Crash recovery

A `Building` row found at startup: backfill restarts from scratch (a
fresh snapshot, a fresh full pass — never resumed, never silently
promoted). A `Dropping` row found at startup: the sweep restarts from the
beginning of the index's key range (idempotent — deleting an already-
tombstoned entry is a no-op).

## What this increment does not build

SQL parser/binder/planner/executor, transactions (so `UNIQUE`
*enforcement* is not certified — the physical structure is), CLI/API/
frontend wiring. See `PHASE_RELATIONAL_INDEX_INCREMENT5_RESULTS.md` for
the full status matrix.
