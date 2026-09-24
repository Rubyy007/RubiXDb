# PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE

**Scope**: production-grade write execution — `SQL write → Parser → AST
→ Binder → Plan → Transaction → Write Executor → TableStore/IndexStore
→ write_batch → durable committed state`. `INSERT`, `UPDATE`, `DELETE`,
and the DDL forms the current catalog/index architecture actually
supports (`CREATE SCHEMA`/`TABLE`, `DROP TABLE`, `CREATE`/`DROP INDEX`).
Out of scope: `GROUP BY`/`HAVING`/aggregates/window functions/
subqueries/CTEs/set operators (unbound at the binder, Increment 6's own
boundary, unchanged), parallel/distributed execution, CLI/HTTP API/
frontend.

This is a decision record, written alongside `sql/src/exec/write.rs`
and `sql/src/exec/write/metrics.rs` — every §  names the real source it
was decided from, never a guess.

---

## 1. Never a second write path

Every table-row mutation goes through `Transaction::put_row`/
`delete_row` (`src/relational/txn.rs`, certified in Increment 7) — row-
shape validation (`validate_row_shape`, NOT NULL/column-count/type),
freshness/`UNIQUE` conflict validation, and atomic table+index
`write_batch` construction all already live there and are reused
verbatim, never duplicated in `sql/`. Every catalog mutation goes
through `CatalogService`'s own already-atomic (`ddl_lock` +
`write_batch`) DDL methods, or — for `CREATE`/`DROP INDEX` — the already-
certified online `IndexBuilder::create_index_online`/`drop_index_online`
(never `CatalogService::create_index`/`drop_index` directly, which only
touch the catalog row without backfilling/deactivating — confirmed by
reading both call sites before choosing).

## 2. Execution context and the `Plan`/executor contract

`execute_write(plan, txn: &mut Transaction, table_store, catalog,
index_builder, params, limits, metrics, cancellation) -> Result<
WriteResult>` executes one write-shaped `Plan` into the *caller's own*
transaction, never committing it — matching item 4 exactly ("execute
into the caller's existing transaction without committing it
automatically"). `Plan::Query`/`Explain`/`Begin`/`Commit`/`Rollback` are
rejected with `SqlError::UnsupportedExecution`: reads go through
`crate::exec::execute` (even inside the same transaction — a write
statement never re-implements a read), and transaction-control
statements are the caller's own `TransactionManager`/`Transaction`
lifecycle, never a second one built here.

`execute_write_autocommit(plan, txm: &TransactionManager, ...)` is the
`BEGIN → execute → COMMIT` convenience wrapper for a single autocommit
statement, reusing D10's already-certified `begin`/`commit` verbatim.

## 3. DDL's deliberate independence from the SQL `Transaction`

`CatalogService`'s DDL methods and `IndexBuilder`'s online-build/drop
protocol already use their own internal `ddl_lock` + `write_batch`
atomic mechanism, entirely independent of the SQL-level `Transaction`/
snapshot-isolation machinery (an Increment 7 decision, re-applied here
rather than re-decided). `Plan::Ddl` is therefore executed as its own
atomic unit: `execute_write_autocommit` still begins a throwaway
`Transaction` (so the function keeps one uniform signature across every
`Plan` variant), but for `Plan::Ddl` that transaction is only ever
rolled back afterward, never committed — the DDL's own durability
already happened, independently, the instant `catalog.create_table(...)`
etc. returned `Ok`. This is item 80's own "inspect RubiXDB's actual
architecture, don't assume PostgreSQL" resolved by re-using an existing,
already-certified decision rather than inventing a new one.

## 4. INSERT

`execute_insert` runs in two phases. Phase 1 (read-only): every row's
bound `VALUES` expressions are evaluated up front against one shared
`ExecCtx` borrowing `txn` immutably (`INSERT`'s own grammar never binds
a `Column` reference, `crate::bind::dml::bind_insert`, so an empty
`RowContext` is always sufficient). Phase 2 (write): after that borrow
ends, each evaluated row is buffered into the transaction's write-set
via `txn.put_row`, incrementing `rows_affected`. A validation failure on
row *N* leaves rows before it buffered but nothing durable anywhere
until the statement's own, later, single commit — multi-row `INSERT`
atomicity (item 7) is a direct, unmodified consequence of D10's own
write-set-then-commit design, not a new mechanism.

### 4a. A real, pre-existing binder bug found and fixed

Inspecting the binder before designing execution (per the increment's
own "do not guess" rule) surfaced a genuine correctness bug in
`sql/src/bind/dml.rs::bind_insert`: an *omitted* `INSERT` column was
always bound to `Literal(None)` — structurally indistinguishable from a
caller explicitly writing `NULL` — even when the column had a declared
`DEFAULT`. `crate::bind::ddl::encode_default_literal`'s own Increment-6
doc comment said "any future executor decodes it with `decode_row`" —
a breadcrumb that was never followed until now, because no executor
existed to expose the bug. Fixed at the source of the ambiguity (bind
time, not execution time, since the ambiguity is already unrecoverable
by the time a `BoundInsert` exists): the second loop in `bind_insert`
(the one filling still-`None` slots after explicit `VALUES` are bound)
now decodes `column.default_value` via `rubixdb::relational::value::
decode_row` and binds the decoded value, only falling back to
`Literal(None)` when no default exists. An *explicit* `NULL` on a
nullable-with-a-default column is unaffected (bound by the *earlier*
loop, before this one ever runs) — matching standard SQL: `DEFAULT`
applies only when a column is omitted, never overridden by an explicit
`NULL`. Regression test: `bind_tests::insert_omitted_column_with_
default_binds_to_the_defaults_own_value`; end-to-end test: `write_
tests::insert_omitted_default_column_gets_its_declared_default_end_to_
end`.

### 4b. A real primary-key-uniqueness gap found by differential testing, and fixed

`Transaction::put_row` is a generic *upsert*-at-key primitive (confirmed
by reading `txn.rs`: it never checks whether the physical key it is
about to write already exists). `Transaction::commit`'s own freshness
check (`validate_and_build_ops`) only compares "the value at my
snapshot" against "the value right now," under the per-table epoch
write-lock — which correctly rejects two *concurrently overlapping*
transactions racing to insert the same `PRIMARY KEY` (both see `None` at
their own snapshot, so the second one's freshness check fails once the
first has committed), but does **not** reject a *later, non-overlapping*
transaction inserting a `PRIMARY KEY` some earlier, already-committed
transaction used: both "at my snapshot" and "right now" agree (nothing
changed since the second transaction's snapshot was taken), so no
conflict is raised, and the row is silently overwritten. `commit`'s own
`UNIQUE`-conflict machinery (`unique_checks`) only ever iterates
`IndexKind::Unique` *secondary* indexes — the `PRIMARY KEY` itself is
never modeled as one, so it gets no equivalent existence check.

This is exactly the gap `PRIMARY KEY` enforcement is supposed to close:
a plain `INSERT` of an already-used key must fail, full stop — it is not
`UPSERT`/`ON CONFLICT` (out of scope: "only if already bound grammar,"
and no such grammar exists). Found by `write_tests::differential`'s own
independent reference-model test (a third, duplicate-`PRIMARY-KEY`
`INSERT` was accepted by the real system but rejected by the reference
model), not by inspection.

**Fix**: `execute_insert` now calls `txn.get_row(insert.table_id, &pk)`
immediately before each row's `put_row`, and returns `SqlError::
Conflict` if a row already exists at that key. This is deliberately
*not* a second, independent conflict detector (item 11's own concern):
it reuses `Transaction::get_row`, the exact same snapshot- and write-
set-overlay-correct read every other statement kind already uses,
entirely *within* the same transaction (not a separate connection or
snapshot, so it is not the "check-then-write outside the transaction
protocol" item 12 forbids) — and the commit-time freshness check remains
the *sole* authority for the concurrent-overlap case, completely
unchanged. The two checks are complementary, not redundant: this one
closes the *sequential* gap freshness structurally cannot see; freshness
still closes the *concurrent* gap this one cannot see (a bare `get_row`
executed by two overlapping transactions can both return `None`).
Reported as `SqlError::Conflict` — the exact class `RelationalError::
Conflict` already maps to via the existing `From` impl — so a `PRIMARY
KEY` violation and a genuine concurrent write conflict present the same
error shape to a caller, the same way a `UNIQUE` violation already does.
`execute_write`'s own error-classification (§9) now routes a `Conflict`
from *either* origin to the same `write_conflict` metric, never a
generic `dml_error`.

**UPDATE never needs this check.** `UPDATE`'s own grammar rejects
`PRIMARY KEY` reassignment at bind time (D6, §5), so `execute_update`'s
`put_row` calls always target a key that provably already exists
(re-fetched fresh via `get_row` immediately before, §5) — `put_row`'s
upsert semantics are exactly correct there, unmodified.

Regression tests: `write_tests::differential::matches_reference_model_
for_a_generated_insert_update_delete_sequence` (the test that found the
bug), `write_tests::metrics_record_write_conflict_not_a_silent_success`
(two genuinely racing autocommit `INSERT`s of the same key, verifying
the loser is always classified as a conflict regardless of which of the
two mechanisms caught it).

## 5. UPDATE — the two-phase, bounded-memory design

`execute_update` and `execute_delete` share `find_target_pks`, which
drives the *exact same* read-side machinery `crate::exec::execute` uses
for `SELECT` (`crate::exec::operators::build_operator` over
`PhysicalPlan::Access` wrapping the plan's own already-chosen
`PhysicalAccess`) — every `PkLookup`/`IndexScan`/`SeqScan` correctness
property Increment 9 already certified, and the planner's residual-
predicate guarantee, apply completely unmodified, never reimplemented.

This function collects **only each matching row's own `PRIMARY KEY`
values**, never the full row — a `Vec` of PK tuples is a materially
smaller, still-bounded footprint than one of full rows — and fails
closed with `SqlError::ResourceLimit` the instant the new `ExecLimits::
max_dml_target_rows` (default `10_000`, matching `TxnLimits::max_write_
set_ops`'s own default) is exceeded **while collecting**, not only
after. This directly satisfies items 14/15 (reuse the certified read
path, evaluate the residual predicate exactly once) and 47/48 (never an
unbounded in-memory vector of every affected row).

The reason this is two *phases*, not one: an `ExecCtx` (used for both
the read pass and for evaluating `SET` expressions) holds an *immutable*
borrow of `Transaction`, while `put_row`/`delete_row` need a *mutable*
one — Rust's borrow checker cannot let one `ExecCtx` span both a read
pass and a write pass on the same `Transaction`. An earlier draft tried
exactly that and does not compile; the two-phase split (collect bounded
PK list, drop that borrow, then mutate) is not just a performance
choice, it is the only design the type system accepts — and it happens
to be exactly the bounded-memory shape item 47/48 require anyway.

For `UPDATE`, Phase 2 re-fetches each row *fresh* via `txn.get_row`
(item 14: never the row the earlier target-finding pass happened to
see), builds a `RowContext` over that one row, and evaluates every
`BoundAssignment` inside a scoped block so the `ExecCtx` used for
`eval()` is dropped before the subsequent `put_row` call needs a mutable
borrow in the same loop iteration.

**`SET`-expression evaluation semantics** (item 18's own "if the
representation does not define this, decide"): every assignment
evaluates against the *same*, unmodified, pre-update `RowContext` —
standard SQL simultaneous-assignment semantics (`UPDATE t SET a = b, b =
a` swaps using the old `a`/`b` for both, never a sequentially-updated
intermediate value). `BoundAssignment { ordinal, value }` carries no
marker suggesting otherwise, and this is the SQL-standard, most
defensible default given no more specific signal exists.

**`PRIMARY KEY` updates are structurally unreachable.** `bind_update`
already rejects them (`crate::bind::dml::bind_update`, D6 — verified via
source read and the pre-existing test `bind_tests::update_of_primary_
key_column_is_rejected`, not assumed). This means `new_row`'s own
primary-key columns are always identical to `old_row`'s, so `put_row`
upserting at the same key is always the complete, correct operation
(old/new secondary-index-entry delete+insert included atomically by
D11's own already-certified `index_maintenance_ops`) — the delete-old-
physical-row-plus-insert-new-physical-row fallback item 19 names for a
PK-changing `UPDATE` is not implemented, because it is unreachable.

**No unnecessary index rewrite for an unchanged row** (item 22): if
`new_row == old_row` after evaluating every assignment, `put_row` is
skipped entirely — still counted in `rows_affected` (the row was
matched and processed, item 123's "matched" semantics), but no physical
write, and therefore no index-entry churn, happens for a no-op `SET`.

## 6. DELETE

`execute_delete` calls `find_target_pks` (§5, shared with `UPDATE`),
then calls `txn.delete_row(table_id, &pk)` once per collected key — no
re-fetch is needed, since `DELETE` needs only the primary key, never the
row's other column values (item 14's own "authoritative through the
already-certified `delete_row`," item 16's "exact multiplicity," which
falls directly out of one `delete_row` call per matched PK, never a
scanned-row count).

## 7. DDL

`execute_ddl` matches each supported `BoundStatement` DDL variant and
calls the corresponding already-certified catalog/index primitive
(`CatalogService::create_schema`/`create_table`/`drop_table`,
`IndexBuilder::create_index_online`/`drop_index_online`). `IF (NOT)
EXISTS` is handled by catching `CatalogError::AlreadyExists` (or, for
`CREATE INDEX`, the wrapped `RelationalError::Catalog(CatalogError::
AlreadyExists{..})`, since `create_index_online` returns `rubixdb::
relational::Result`) *before* the blanket `From<...> for SqlError`
conversion collapses it to an unmatchable string — confirmed via grep
that `AlreadyExists` is the single uniform variant across schema/table/
index creation. `DROP TABLE`/`DROP INDEX`'s `IF EXISTS` no-op case is
already resolved at *bind* time (`BoundDropTable::table_id`/
`BoundDropIndex::index_id` are `None` only when genuinely missing), so
the executor's own handling is a plain, real no-op — not a swallowed
error.

**`CREATE DATABASE` has no execution primitive and is refused, not
faked.** `grep -n "pub fn create_database" src/catalog/service.rs`
returns zero matches — `bootstrap()` is the only current way a database
row is ever created. Adding one is a materially larger catalog-layer
primitive than this increment's own evidence justifies (item 35: "ONLY
execute the DDL forms the current architecture can safely support");
`execute_ddl` returns `SqlError::UnsupportedExecution` for `BoundStatement
::CreateDatabase` rather than inventing a workaround.

**No `BoundStatement`'s `Debug` output ever reaches an error message.**
The defensive catch-all arm for a non-DDL `BoundStatement` reaching
`execute_ddl` (structurally unreachable in practice — `execute_write_
inner` only calls `execute_ddl` for `Plan::Ddl`, which the planner only
ever builds from a DDL-shaped statement) prints only a hand-written
variant *name* (`bound_statement_kind_name`), never `{:?}` — a `Select`/
`Insert`/`Update`/`Delete` variant carries bound literal values from the
statement's own text, and item 50 forbids row values in error messages
regardless of how defensively unreachable the branch is.

## 8. CHECK constraints and NOT NULL — what is and is not enforced

`NOT NULL` is already enforced by `Transaction::put_row`'s own internal
`validate_row_shape` call (Increment 5/7) — the write executor does not
duplicate this, it relies entirely on `put_row`/`delete_row`'s existing
checks, propagating any `RelationalError` via the already-certified
`SqlError: From<RelationalError>` conversion.

`CHECK` is **not enforced**, because it is not reachable: `grep`-
confirmed, `ConstraintKind::Check` exists only in `src/catalog/{schema,
service}.rs` as inert catalog metadata (used by a lower-level, non-SQL
catalog API), but `BoundColumnDef`/`BoundCreateTable` have no field for
a `CHECK` expression at all — the current `CREATE TABLE` grammar/binder
has no path that could ever populate one. Documented here as item 71
asks: "only test constraints actually implemented end-to-end... if
catalog-only, document as not yet enforced."

## 9. Metrics accounting — an honest, documented tradeoff

`WriteMetrics` (`sql/src/exec/write/metrics.rs`) records `insert_
statements`/`update_statements`/`delete_statements`/`ddl_statements`/
`rows_inserted`/`rows_updated`/`rows_deleted`/`write_conflicts`/`dml_
errors`/`ddl_errors` — all bounded-cardinality counters, no table/
schema/index/SQL-text/principal label ever accepted by any recorder
method (structurally, not by convention — every method takes only a
count or nothing).

`rows_inserted`/`rows_updated`/`rows_deleted` are recorded at
**buffer-time**, inside `execute_write`, immediately after each `put_
row`/`delete_row` call succeeds — *not* deferred until the surrounding
transaction actually commits. For the primary path (autocommit), buffer
time and commit time are effectively simultaneous. For an explicit,
multi-statement transaction, or for autocommit's own subsequent
`commit()` call failing after `execute_write` already returned success,
this is a known, honestly-documented imprecision: a statement's rows are
counted the moment they are buffered, and if the surrounding transaction
is later rolled back or fails to commit, those counts are not retracted
(no counter here supports a safe concurrent subtraction). `execute_
write_autocommit`'s own `commit()` failure path additionally records
`write_conflict`/`dml_error` on top of the already-recorded buffer-time
counts specifically so that failure is still observable, rather than
silently leaving only an inflated `rows_inserted`. A fully reconciled,
commit-time-only accounting system was considered and rejected as out of
proportion to this increment's own evidence — no requirement or test
here currently depends on exact retroactive correction, and building one
speculatively would be exactly the kind of unjustified complexity this
project's own established convention avoids.

`write_conflicts` originates from two places, both classified
identically (§4b, §9-continued): `execute_insert`'s own pre-write
existence check (the sequential duplicate-`PRIMARY-KEY` case), and
`execute_write_autocommit`'s own `commit()` call (the concurrent-overlap
case). `execute_write`'s own error-classification match arm checks for
`SqlError::Conflict` *before* falling back to `ddl_error`/`dml_error`,
so neither origin is ever miscounted as a generic failure (item 118/
121).

## 10. Resource limits

`ExecLimits::max_dml_target_rows` (new, default `10_000`) bounds `find_
target_pks`'s own collection loop (§5) — the write-executor-specific,
earlier, more clearly-attributed limit. `Transaction`'s own pre-existing
`TxnLimits::max_write_set_ops`/`max_write_set_bytes` (Increment 7,
checked inside `record_op` before allocation) already bound the total
size of any transaction's write-set "for free," for any sequence of
`put_row`/`delete_row` calls this or any other statement makes — the new
limit exists to fail sooner and with a clearer message at roughly the
same boundary, not to impose a materially different one.

## 11. Security

No filesystem path, physical key, WAL-internal detail, or row *value*
ever appears in a `SqlError` this layer constructs (§7's `BoundStatement`
hardening is the one place this needed active attention beyond what
`RelationalError`'s own already-safe `Display` text provides). Every
table-row mutation is reached only through a bound, already-authorized
`Plan` (authorization is resolved once, at bind time, in the same pass
as identifier resolution — D25/D26, unchanged this increment); the write
executor itself never re-resolves an identifier, re-authorizes, or
accepts a raw table/index ID from anywhere outside an already-planned
`Plan`. `system.*` catalog tables are protected structurally, not by a
runtime check: the write executor's only path to a catalog row is
`CatalogService`'s own DDL methods, and there is no code path anywhere
in `sql/src/exec/write.rs` that accepts an arbitrary `INSERT`/`UPDATE`/
`DELETE` targeting a system table's physical storage the way an ordinary
table's `put_row`/`delete_row` calls do (a `system.*` object's `Bound
TableRef`/`table_id`, if a caller tried to `INSERT` into it directly,
still resolves and authorizes exactly like any other table at bind time
— no special-casing was needed or added, since D25's own grant model
already governs it the same way).

## 12. Write skew

Unchanged from Increment 7: this transaction model deliberately
preserves snapshot isolation, not Serializable — write skew (two
transactions each reading a value the other later writes, in a way
Serializable would reject but SI permits) remains possible and is not
newly introduced or newly closed by write execution existing. No new
anomaly class is introduced by `INSERT`/`UPDATE`/`DELETE` specifically;
they use the exact same commit-time validation every other write already
used.
