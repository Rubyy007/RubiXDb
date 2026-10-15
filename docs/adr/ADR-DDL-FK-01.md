# ADR-DDL-FK-01: ALTER TABLE and foreign keys

Status: IMPLEMENTED in the working tree, uncommitted (SQL Phase 2b + 3, 2026-10-15).
Scope: `src/catalog`, `src/relational`, `sql/`; `api/` for additive error mapping and statement dispatch only.
Not touched: `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`. **No WAL, MANIFEST or SSTable format changed.**
Evidence: `docs/SNOWFLAKE_SQL_COMPATIBILITY.md` section 17.

## 1. Problem

The Phase 0 audit (`docs/SNOWFLAKE_SQL_COMPATIBILITY.md` section 7) named three blockers:

* **AB-1** a stored row is decoded *positionally against the current column list*; the row's own `schema_version` was read and discarded, so a column added after a row was written would mis-size or mis-read it;
* **AB-2** snapshot isolation has no read-set: commit validates the keys a transaction *wrote*, so a parent check done at statement time is invisible to a concurrent commit that deletes the parent;
* **AB-3** `system.constraints` cannot express a foreign key.

## 2. Decisions

### D1. Catalog format (additive, readable by the old reader)

* **`system.foreign_keys` is a new system table (id 8)**, one row per key: child table, name, child columns, parent table, parent columns, resolved `UNIQUE` index (NULL = parent primary key), `ON DELETE`, `ON UPDATE`, creation time. It is a new key range: a catalog written before it exists simply has no rows in it, and no existing row's encoding changes. Ids come from the existing `Constraint` counter.
* **`system.columns` rows gain one trailing field, `backfill_value`, and are written with envelope `schema_version` 2.** The decoder reads the envelope version first: version 1 (every row written before this change) has six fields and decodes exactly as before, with `backfill_value = None`. The null bitmap is one byte for both shapes (six and seven fields), so a *previous* binary reading a version-2 row still decodes its first six fields correctly and ignores the seventh. Tested: `schema::tests::column_row_version_one_decodes_and_version_two_round_trips_backfill`, `fk_tests::versioned_decode::an_old_version_one_catalog_row_still_decodes`.
* `system.tables.schema_version` (a field that always existed and was always 1) is now the table's schema version: **every `ALTER TABLE` and every foreign-key change to the child bumps it**, in the same `write_batch` as the change.
* **Downgrade**: an older binary opens a catalog that contains foreign keys without error (it never reads table 8) but would not enforce them, and would mis-read rows of a table that had a column added (it ignores the row version). A catalog that was never altered and holds no foreign key is fully compatible both ways. This is recorded as a limitation, not a supported path.

### D2. ADD COLUMN: append-only ordinals, versioned decode

* A column is **appended** at the next ordinal. No existing ordinal changes, so the primary key, every index, every foreign key and every stored row keep their meaning.
* Each `ColumnRow` records `added_in_schema_version`. `decode_full_row` reads the row's own `schema_version` from its envelope and decodes against exactly the non-key columns with `added_in_schema_version <= row_version` (a prefix, because columns are only appended). A column added later is **not in the row's bytes**; it reads as the column's `backfill_value` — the literal `DEFAULT` the column was added with, or `NULL`.
* `backfill_value` is fixed at `ADD COLUMN`; `SET DEFAULT` / `DROP DEFAULT` change only what *new* rows get. (PostgreSQL's `attmissingval` has the same shape.) Tested.
* A row is rewritten at the newest version whenever it is updated; nothing rewrites old rows in bulk. `NOT NULL` without a `DEFAULT` is accepted only on an empty table.
* `resolve_table` re-reads a (table, columns) pair that straddles an `ADD COLUMN` (a column newer than the table row), so no reader pairs the old table row with the new column list.
* Direct `TableStore` writers resolve the schema **after** taking the table's epoch lock.

### D3. DROP COLUMN is BLOCKED (not implemented)

A column's identity is its ordinal: `ColumnRow` is keyed `(table_id, ordinal)`, the primary key, index and foreign-key definitions store ordinals, and `decode_full_row` and the SQL layer (13 000 lines of binder, planner and executor) treat a row's position as the column. Hiding a dropped column would need a logical-to-physical ordinal map threaded through all of them; getting one place wrong corrupts reads or index maintenance, which the mission forbids. The statement is refused at parse time with the reason. The smallest sound design (tombstoned column rows + a visible-column map in `resolve_table`) is recorded in `OPEN_ITEMS.md` for a mission that can audit every ordinal consumer.

### D4. Every ALTER is: lock, validate, one atomic batch

`IndexBuilder::alter_*` (the existing home of online DDL needing the epoch locks): take the **epoch write-lock of every table involved, in sorted order** (the lock every commit takes), validate the definition and — for `SET NOT NULL`, `ADD FOREIGN KEY`, `ADD UNIQUE` — **every existing row**, then apply the change as **one catalog `write_batch`** that also bumps the schema version. Atomicity, recovery and durability are therefore exactly those of the certified `write_batch`: a failure or crash before the batch leaves the catalog unchanged; after it, the change is complete. There is no half-applied ALTER to recover.

A transaction that buffered a row write before an ALTER and commits after it fails with a `Conflict` (`Transaction::table_versions` compared with the table's current version under the commit locks): it can neither write a row of the old shape nor slip a row past a rule added meanwhile.

`ALTER` is **one operation per statement**: several in a row would not be atomic together.

### D5. Foreign-key concurrency: commit-time validation under the existing epoch locks (not parent-row locking)

**Decision: validate at commit, against the latest committed state plus the transaction's own writes, with the affected tables' epoch locks held.**

Why not parent-row locking: this engine has no row locks. Snapshot isolation buffers writes and validates written keys at commit under per-*table* epoch locks (`txn.rs` header). A row-lock manager would be a second concurrency mechanism beside the certified one, with its own deadlock and recovery story, on the hot path. The per-table epoch lock already exists, is already taken in sorted order by every commit, and is the lock index builds are certified against.

The rule (`Transaction::commit`):

1. **Lock plan.** The tables the commit writes are locked *exclusively*. For each foreign key, the *other* table whose rows the validation reads is locked *shared* (a child write reads the parent table; a parent write reads the child table). Shared locks exclude every writer of that table (they hold the exclusive side) while letting commits that only *reference* one parent run side by side. All locks are acquired in ascending table id, so two commits cannot deadlock whichever side of a key a table is on (tested with keys in both directions).
2. **Generation check.** The foreign-key set can change (`ADD FOREIGN KEY`) between reading it and holding the locks. `CatalogService::foreign_key_generation` is read before the lock plan is built and again after the locks are taken; if it moved, release and rebuild (bounded, 8 attempts). A foreign-key change takes both tables' exclusive locks and bumps the generation under them, so any commit either ran wholly before it (and was scanned by its validation) or sees it.
3. **Validation, under the locks, after the freshness check** (so a written parent row's snapshot value equals its current value):
   * *referencing side*: every written child row with a fully non-`NULL` key must find its parent in (latest committed state + own writes);
   * *referenced side*: every parent key that a written row changed or removed, and that no other row provides, must be referenced by no child row in (latest committed state + own writes) — so children the same transaction deletes or re-points do not count, and children committed by others do.
4. `write_batch` follows under the same locks. Nothing can change either table between the check and the write.

Both orderings are tested, at the relational layer and through SQL and HTTP:

| Ordering | Result |
|---|---|
| child-before-parent: child txn open, parent deleted and committed, child commits | child commit refused (`ForeignKeyViolation`), nothing written |
| parent-before-child: parent-delete txn open, child inserted and committed, parent delete commits | parent delete refused, parent and child intact |
| `ON DELETE CASCADE` parent delete racing a new child | the delete (which could not see the child) is refused; retrying cascades it |
| 60 barrier-started rounds (relational) and a 3-writer SQL stress | exactly one of the two commits; no orphan, ever |
| `ADD FOREIGN KEY` racing child inserts (15 + 6 rounds) | if the key exists afterwards there is no orphan; otherwise it was refused and nothing recorded |

**Isolation model**: this is sound for foreign keys because both checks run *serialised against all writers of both tables*. It does **not** make the database serializable in general (write skew on other constraints remains the documented D10 property). The cost is table-granularity: a commit that writes a parent blocks commits to its children for the duration of the validation scan, and the validation scan of a child table with no usable access path (below) is linear in the child table, under the lock.

**Statement-time enforcement (immediate semantics).** The write executor also calls `Transaction::enforce_foreign_keys` after buffering each DML statement's rows, against the transaction's *own view* (snapshot + own writes), so a violation is reported by the statement that causes it and `ON DELETE` / `ON UPDATE` actions are carried out. It is not the authority — commit is — but without it an explicit transaction would report violations only at `COMMIT`.

### D6. Semantics

* `MATCH SIMPLE`: a row with any `NULL` referencing column references nothing and is not checked. `MATCH FULL` / `PARTIAL` and `DEFERRABLE` are refused.
* `NO ACTION` and `RESTRICT` behave identically (enforcement is always immediate). Both refuse a change that leaves a child unreferenced *at the end of the statement* (so a statement that deletes parents and their children together succeeds).
* `CASCADE`, `SET NULL`, `SET DEFAULT` for both `ON DELETE` and `ON UPDATE`. `SET NULL` needs every referencing column nullable and outside the primary key; `SET DEFAULT` needs a nullable column or a `DEFAULT` — both checked at definition. `SET DEFAULT` to a default that names no parent fails the statement.
* Referenced columns must be the parent's primary key or a **Ready** `UNIQUE` index covering exactly the same column *set* (any order; values are paired positionally with the referencing columns). Types must be identical (including `DECIMAL` precision and scale). Composite keys up to 16 columns. A unique index a key resolves to cannot be dropped; a table another table references cannot be dropped; dropping a child table removes its keys.
* Cascades are applied by buffering the cascaded changes into the same transaction, recursively (a cascaded child can itself be a referenced parent), bounded by **depth 32** and by the transaction write-set cap (10 000 operations / 16 MiB); either limit fails the statement closed.
* **Statement atomicity.** Every DML statement runs inside a `Savepoint` (a journal of the write-set that `rollback_to` undoes, including the size accounting), so a statement that fails part-way — a violated key, a cascade past a limit — leaves the transaction exactly as it was before the statement. This also closes the Phase 2a caveat that a failure in the buffering step could leave earlier rows of the statement buffered.
* **Row lookup for checks** (`relational::lookup`): primary-key point read, primary-key prefix scan (child keyed `(parent_id, seq)`), `Ready` secondary index, or table scan, in that order of preference. Index the referencing columns of a large child table; without one every parent delete scans it (once per statement, batched).
* Error messages name the constraint, tables and columns — never a row value.

### D7. UPDATE of a primary key (matrix G-8)

`ON UPDATE CASCADE` needs it, so `UPDATE` may now assign primary-key columns (the old refusal is superseded; its two pinning tests are updated and say why). A key-changing row is a delete of the old key and an insert of the new; all old keys of a statement are removed before any new key is inserted, so `SET id = id + 1` works; a collision with a row the statement does not move fails the statement (`Conflict`). `INSERT ... ON CONFLICT DO UPDATE` still refuses to assign a key column.

### D8. CHECK constraints (existing catalog row format)

`system.constraints` already had a `Check` kind with a text expression that nothing evaluated; **its row format is unchanged**. A constraint is stored as the expression's SQL text plus the ordinals of the columns it reads. Whenever a statement writes rows of the table, its constraints are re-parsed through the normal `parse_statement` front door and bound against the table's *current* columns, then evaluated per written row with the engine's own evaluator: `FALSE` violates, `TRUE` and `NULL` pass (SQL standard). An expression may use the table's columns, literals, operators and scalar functions; subqueries, aggregates, window functions and parameters are refused when the constraint is defined. Checked on `INSERT`, `INSERT ... SELECT`, both `ON CONFLICT` paths, `UPDATE`, and on the child rows a foreign-key action rewrites (`SET NULL`, `SET DEFAULT`, `ON UPDATE CASCADE`: `enforce_foreign_keys` returns the rows it wrote and the executor checks them). `ADD CONSTRAINT ... CHECK` evaluates every existing row with the table locked, then records the constraint and bumps the schema version in one batch; `DROP CONSTRAINT` removes it likewise. A `CHECK` is row-local, so it needs no commit-time validation; an `ALTER` racing in-flight writers is covered by the schema-version conflict (D4). `RENAME COLUMN` of a column a `CHECK` reads is refused (the stored text names columns). Violations are `SqlError::CheckViolation` (HTTP 409 `CHECK_VIOLATION`), naming the table and constraint, never a value.

The foreign-key and `CHECK` caches live in the `CatalogService` instance: a process must hold exactly one per engine (the server does; the test harness was changed to use the same instance for binding and execution).

### D9. `CREATE UNIQUE INDEX` over existing duplicates (defect found and fixed)

`IndexBuilder::create_index_online` with `Unique` did not check the rows that already existed: an index over repeated values was built and marked `Ready`. A `UNIQUE` index now runs a bounded-memory verification pass (index entries are adjacent by indexed value) before becoming `Ready`; on a duplicate the build fails and the index is `Failed`. `ALTER TABLE ... ADD CONSTRAINT UNIQUE` then drops the `Failed` row so a refused ALTER leaves no trace. `CREATE UNIQUE INDEX` keeps the `Failed` row (existing behaviour, for operator visibility).

## 3. Not implemented

`DROP COLUMN` (BLOCKED, D3), `ALTER COLUMN ... TYPE`, `RENAME TO`, `ADD/DROP PRIMARY KEY`, `NOT VALID` / `VALIDATE`, `CHECK ... NOT ENFORCED`, `DEFERRABLE`, `MATCH FULL|PARTIAL`, several operations per `ALTER`, `ADD COLUMN ... REFERENCES` (add the column, then the key).

## 4. Consequences and risks

* The commit path now reads the foreign-key set and builds a lock plan on every commit. For tables with no key this is a lock-free `Arc` clone and an empty loop; measured in `docs/SNOWFLAKE_SQL_COMPATIBILITY.md` section 17.
* A transaction open across an `ALTER TABLE` (or an `ADD`/`DROP FOREIGN KEY`) on a table it wrote fails at commit with a conflict: intended, and documented.
* Writers other than the SQL executor (`TableStore::put_row` and `put_rows`, the non-transactional test/maintenance entry points) are **not** foreign-key aware. `Transaction`, and therefore `autocommit_put_row`, is. The only production writer is the SQL executor.
* Crash behaviour: a cascade is buffered in memory and reaches the engine as the single `write_batch` of the commit, so it is whole or absent by the certified batch atomicity. Process-kill and power-loss runs of a cascade are **NOT TESTED** here (a clean engine restart after commit, and after an abandoned transaction, are tested).
