# PHASE_RELATIONAL_TRANSACTION_ARCHITECTURE

**Scope**: this document is the decision record for Increment 7 — a
production-grade **Snapshot Isolation transaction engine**: transaction
lifecycle, snapshot capture, read-your-own-writes, the in-memory
write-set, commit-time conflict validation, `PRIMARY KEY`/`UNIQUE`
enforcement, atomic table+index commit, rollback, crash recovery,
Compaction interaction, resource limits, security, and performance. It
implements D10 (`PHASE_RELATIONAL_DATABASE_ADR.md`) exactly as already
approved; it does not re-litigate D10, it executes it. No SQL parser,
binder, planner, optimizer, executor, `SELECT`/`INSERT`/`UPDATE`/
`DELETE`/`JOIN` execution, `GROUP BY`, aggregation, CLI, HTTP API, or
frontend SQL console is added. `RELATIONAL DATABASE PRODUCTION READY =
NO` after this increment — no SQL execution path exists yet.

**Not to be confused with** `PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md`
(a much earlier increment; that document specifies `LsmEngine::
write_batch`, the storage primitive this increment builds *on top of*
and never modifies).

---

## §1. Transaction lifecycle

```rust
pub enum TxnState { Active, Committed, RolledBack, Aborted }
```

`Committing` is deliberately **not** a state a caller can ever observe.
The commit critical section (§4) is a single, uninterruptible,
lock-held sequence with no yield point a concurrent reader of `state()`
could observe mid-way; modeling a fourth state would suggest an
external observability hook this design does not have and does not
need.

**Illegal transitions never panic** — and, for the two most important
cases, cannot even compile:

```rust
impl Transaction {
    pub fn commit(mut self) -> Result<u64>;   // consumes self
    pub fn rollback(mut self) -> Result<()>;  // consumes self
}
```

`commit`/`rollback` take `self` **by value**. Calling either a second
time, or calling `put_row`/`get_row`/`delete_row` after either, is a
**Rust compile error**, not a runtime state check — the strongest
possible implementation of "do not panic on an illegal transition."
`RelationalError::InvalidTransactionState` exists defensively for the
one case the type system cannot prevent (`&mut self` methods called
after a state transition that does not consume `self` — there are
none today, since every state-changing method already consumes `self`;
the variant is kept because a defensive `require_active()` check costs
nothing and documents intent for any future method that does not
consume `self`).

Dropping an `Active` transaction without calling `commit`/`rollback`
(`panic` unwinding, an early `return`, simply going out of scope) is an
**implicit rollback** — `Transaction`'s `Drop` impl decrements the
active-transaction count and records a `transactions_rolled_back`
metric; nothing was ever sent to the engine, so there is nothing to
undo.

## §2. Snapshot capture and read-your-own-writes

`TransactionManager::begin()`:

1. Bounds-checks `active_transactions` against `TxnLimits::
   max_concurrent_transactions` (rejected **before** any allocation —
   item 32).
2. Calls the already-certified `LsmEngine::snapshot()` **exactly
   once**, pinning `Snapshot::seq()` for the transaction's entire
   lifetime.
3. Allocates a monotonic `u64` transaction id (`AtomicU64`, process-
   local, `Relaxed` ordering — see §3 for why this is sufficient and
   why it is never persisted).

`Transaction::get_row`:

```
1. Check the local write-set for this table_id/encoded_pk.
     Found  -> return its buffered value (Some(row) or None for a
               buffered delete) without touching the engine at all.
     Absent -> fall through.
2. engine.get_as_of(table_row_key(table_id, encoded_pk), snapshot.seq())
```

This is the entire read path: local overlay first, snapshot-pinned
engine read second. Every read a transaction performs — including the
ones commit-time validation itself issues internally — resolves
against this same snapshot seq or the current committed state, never
anything in between.

## §3. Transaction ID strategy

A process-local, in-memory, monotonically increasing `u64`
(`AtomicU64`, starting at 1), **never persisted**. Justification, not
an arbitrary choice:

- D10 does not require transaction IDs to survive a restart — there is
  no "resume an in-flight transaction after a crash" requirement
  anywhere in scope (crash recovery, §9, only needs to distinguish
  "committed" from "never committed," which the underlying `write_batch`
  boundary already does without any transaction ID at all).
- The engine's own durability unit is the `write_batch`'s assigned
  `seq`, already durable and already the thing crash recovery replays
  by. A second, redundant durable transaction-id log would duplicate
  that mechanism for no behavioral gain — directly the kind of
  unnecessary persisted metadata the spec says not to add.
- `id()` exists on `Transaction` purely for **diagnostics/logging**
  correlation within a single process's lifetime (e.g. an operator
  tracing "transaction 4821 conflicted") — it is never compared across
  restarts, never part of any on-disk key, and never influences commit
  outcome.

## §4. Write-set representation and commit

```rust
type WriteSet = HashMap<u32, HashMap<Vec<u8>, (Vec<RelationalValue>, RowOp)>>;
//              table_id -> encoded_pk -> (pk_values, latest RowOp)
enum RowOp { Put(Row), Delete }
```

Last-write-wins per key within one transaction (`put` then `put` then
`delete` on the same PK collapses to one buffered `Delete`) — exactly
what a single physical key's history should look like once applied.
`op_count`/`byte_count` are tracked incrementally as writes are
buffered and checked against `TxnLimits` **before** the corresponding
`HashMap` entry is inserted (item 32: reject before allocation, not
after).

`Transaction::commit(self)`:

1. Short-circuit: an empty write-set (a read-only transaction) commits
   trivially — returns the snapshot's own seq, calls `write_batch`
   zero times.
2. Collect the set of touched `table_id`s, **sort** them, and acquire
   each table's `TableStore::epoch_lock` (write side) in that sorted
   order. This is the single serialization mechanism protecting the
   whole validate-then-apply critical section (§5); sorting is what
   makes it deadlock-free across concurrently committing multi-table
   transactions (the classic "acquire locks in a global total order"
   argument).
3. `validate_and_build_ops` (§5/§6): re-validates every touched key's
   freshness, enforces `UNIQUE` (§6), and builds the exact physical
   `Vec<WriteOp>` (row + every affected index entry) the write-set
   implies.
4. On success: one `LsmEngine::write_batch(&physical_ops)` call —
   table row and every affected secondary-index entry, atomically,
   under the already-certified D9/D11 guarantee. Locks release, state
   becomes `Committed`, metrics record.
5. On failure (`Conflict` or any other error): locks release, state
   becomes `Aborted`, nothing was ever sent to the engine.

**Exactly one `write_batch` call per commit, regardless of write-set
size** — directly verified (`commit_issues_exactly_one_write_batch_
regardless_of_row_count`, §"Testing" below) by observing the engine's
own sequence counter advances by exactly one across a 10-row commit,
the same verification technique `RELATIONAL ADR AMENDMENT 003` RA.5
already established for `put_row`.

## §5. Conflict detection

**Value-based, not seq-based.** The engine's public read API
(`GetResult = Option<Vec<u8>>` from `get`/`get_as_of`) does not expose
a per-key sequence number, and D10's own text speaks in value-
comparison terms ("re-reads the current value of every physical key
the write-set touches, compares against what the transaction's
snapshot saw"). For every physical row key the write-set touches:

```
at_snapshot = engine.get_as_of(row_key, transaction.snapshot.seq())
now         = engine.get_as_of(row_key, u64::MAX)
at_snapshot != now  =>  Conflict
```

This single check, applied uniformly, is sufficient for every named
conflict category without special-casing:

- **Write-write on the same key**: two transactions buffer different
  values for the same PK; whichever commits first changes `now` out
  from under the second transaction's `at_snapshot`.
- **`PRIMARY KEY` conflicts** (two concurrent `INSERT`s of the same
  new PK): both transactions' snapshots saw `None` at that key; the
  first commit changes `now` to `Some(row)`, so the second transaction's
  `at_snapshot (None) != now (Some)` — caught by the exact same check,
  no special-casing required. Verified directly (`primary_key_
  conflict_exactly_one_insert_wins_both_orderings`, both commit
  orderings).

The check runs **after** the exclusive per-table lock is held (§4 step
2) and **before** `write_batch` is called, so there is no TOCTOU window
between "validated fresh" and "applied" — any other transaction that
could have raced this table is blocked on the same lock for the whole
critical section.

**Cost scales with write-set size, not table size** — each check is
one or two point `get_as_of` calls per touched key, never a scan.
Measured directly (§"Performance," `txn_commit_vs_table_size_fixed_
write_set`): commit latency for a fixed 4-row write-set stays flat
(~3.3–3.7 ms) across tables from 100 to 50,000 rows.

## §6. `UNIQUE` enforcement (CRITICAL PRODUCTION GATE)

No physical structure existed for `UNIQUE` enforcement before this
increment — Increment 5's `IndexBuilder` built the *storage* for
`IndexKind::Unique` indexes but performed no existence check anywhere
(`grep`-confirmed: no `Unique`/`duplicate`/`Conflict` handling in
`src/relational/index.rs`). This increment is the **first** place
`UNIQUE` is actually enforced, and it is enforced as part of the same
commit-time conflict validation described in §5 — never a separate
"check, then write" sequence with a race window in between.

For every `Put` in the write-set whose row changes a `UNIQUE`-indexed
column's encoded value (compared against the row's own pre-image, so
an update that leaves the unique column unchanged costs nothing extra):

1. Compute the indexed-column prefix (`index_key::encode_indexed_
   columns`) the new row would occupy.
2. **Intra-transaction duplicate check**: a `HashMap<(index_id, prefix),
   our_pk>` built once across every pending `UniqueCheck` in this
   transaction's own write-set. Two *different* new rows in the *same*
   transaction claiming the same unique value conflict here — the
   engine-state scan below cannot see either row yet, so this step is
   required, not redundant with it.
3. **Physical existence scan**: `index_entry_prefix_range` (the exact
   physical prefix range `IndexBuilder`'s own lookups already use) at
   `u64::MAX` (current committed state — the same "now" the row-level
   check in §5 uses, for the same reason: the exclusive per-table lock
   already makes this race-free against any other concurrent commit).
   Any entry found whose primary key differs from the row being
   written is a conflict, **unless** it is "self-vacated" (see below).
4. **Self-vacated exclusion**: a delete-then-reinsert-the-same-unique-
   value *within one transaction* (e.g. `DELETE ... ; INSERT ...` re-
   using a value the deleted row held) must succeed. `entry_is_self_
   vacated` checks whether the physically-found conflicting entry's PK
   belongs to a row *this same transaction* is either deleting, or
   moving away from that value — if so, it is excluded from the
   conflict, exactly matching what the committed state will look like
   once this transaction's own write-set applies.

**Standard SQL `NULL` semantics** (ISO/IEC 9075, matched by PostgreSQL
and SQLite): `NULL` is never equal to another `NULL`, so a `UNIQUE`
index never rejects a row because every one of its indexed columns is
`NULL` — arbitrarily many all-`NULL` rows are permitted. This is a
choice this increment makes (no prior enforcement existed to inherit a
rule from; `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §3 specifies only
the physical *encoding* of `NULL`, not `UNIQUE`-vs-`NULL` semantics). A
composite `UNIQUE(a, b)` with one `NULL` and one present column still
enforces uniqueness on the present part — only an *all-NULL* indexed
tuple is exempt.

This reuses the exact physical `UNIQUE` index structure `IndexBuilder`
already built in Increment 5 (`indexed_entry_key`/`index_entry_prefix_
range`) — **no second unique-index structure was built**.

## §7. Table/index atomic commit

`validate_and_build_ops` reuses `table_store`'s own `pub(crate)` helper
functions — `build_put_op`, `index_maintenance_ops` — the exact same
op-computation logic `TableStore::put_row`/`delete_row` already use, so
there is one algorithm for "what physical writes does this row change
imply," not two independently-maintained copies. Every table-row
`WriteOp` and every affected index-entry `WriteOp` for the whole
transaction's write-set are collected into one `Vec<WriteOp>` and
applied via one `write_batch` call — atomic by construction, under the
same D9/D11 guarantee `TableStore` itself relies on.

`TableStore::put_row`/`put_rows`/`delete_row` were **deliberately left
unmodified** — not refactored to route through `Transaction` — to avoid
any regression risk to Increment 5's already-certified autocommit path.
Code reuse is achieved via the shared `pub(crate)` helpers, not by
unifying the call paths; both consumers now call the same four
functions (`validate_row_shape`, `extract_pk_values`, `index_
maintenance_ops`, `build_put_op`), promoted from private to
`pub(crate)` for exactly this purpose.

## §8. Rollback

`Transaction::rollback(self)` discards the write-set and transitions to
`RolledBack`. Nothing was ever sent to the engine before commit, so
there is nothing to undo — rollback is O(1) relative to write-set size
(dropping a `HashMap`), issues zero engine calls, and cannot fail for
any reason related to the write-set's contents (its `Result` return
exists only for the `require_active()` precondition, which the type
system already makes unreachable in the two-call-in-a-row case).

## §9. Crash recovery

No new crash-recovery *mechanism* was built — `commit` never issues
more than the one `write_batch` call whose all-or-nothing durability is
already certified at the WAL/engine layer (D9, `PHASE_RELATIONAL_
TRANSACTION_STORAGE_ADR.md`, protected and unmodified this increment).
What this increment adds is the guarantee that the transaction layer's
own commit boundary sits *exactly* on top of that primitive with no
intermediate durable state of its own:

- **Crash before commit**: the write-set lived only in process memory;
  a crash (or an ordinary process exit without calling `commit`) loses
  it exactly like an implicit rollback does — there is nothing on disk
  to clean up, because nothing was ever written. Verified with a real
  engine restart (`crash_before_commit_leaves_nothing_visible_after_
  restart`): a transaction buffers writes, the process "crashes"
  (engine `shutdown()` without commit), a fresh `LsmEngine::open` at
  the same directory confirms the buffered rows are absent and a prior,
  actually-committed row survives.
- **Crash during/after commit**: covered by `write_batch`'s own
  certified all-or-nothing semantics — not re-proven at the engine
  level here (that would duplicate `wal`/`manifest`'s own protected
  test suites). What *is* verified here is that the transaction layer
  correctly produces one atomic batch containing every row and index
  write and that the whole batch survives a real restart together
  (`committed_multi_row_indexed_transaction_survives_restart_
  atomically`): a 5-row, indexed transaction commits, the process
  restarts, all 5 rows and all 5 index entries are present.

## §10. Compaction interaction

Transaction snapshots are `LsmEngine::Snapshot` values from the already-
certified `SnapshotRegistry`/`oldest_live_snapshot_seq` machinery —
Compaction already respects live snapshots as a first-class citizen
(pre-existing, unmodified guarantee). `transaction_snapshot_remains_
valid_across_automatic_compaction` forces real, unmodified automatic
compaction to run several cycles (tiny-memtable, auto-trigger
`LsmConfig`, the same fixture pattern Increment 5's own compaction test
established) while a transaction's snapshot is held open, and confirms
the transaction's own read still returns its pre-compaction snapshot
value throughout.

## §11. Concurrency control: what is and is not fine-grained

Commit validation and apply run under the **exclusive** write side of
each touched table's `epoch_lock` — the same per-table `RwLock<()>`
Increment 5 built to serialize online index builds against ordinary
writers, reused here (write side) as the transaction commit critical
section's own serialization mechanism. This is a **table-level**, not
key-level, exclusion: two transactions committing to *disjoint keys in
the same table* still serialize against each other for the full
validate+apply critical section, including the `write_batch` call's own
`fsync`-bound latency.

This is a real, measured limitation, not a claim of fine-grained
concurrency the implementation does not have — see §"Performance,"
`txn_concurrent_commits_disjoint_keys`: end-to-end commit throughput on
one table stays flat (~250–290 commits/sec on this machine, `fsync`-
bound) from 1 to 32 concurrent committing threads, because every commit
against that table serializes on the same lock regardless of key
disjointness. **Different tables commit fully in parallel** (locks are
acquired per touched table, sorted, and a transaction touching only
table A never contends with one touching only table B).

This was a deliberate choice, not an oversight: item's own instruction
to "avoid global locks unless absolutely required" is satisfied (the
lock is per-table, not global), and building genuine key-range
concurrency control (e.g. per-key locking, or a true optimistic
validation pass that only re-checks under a short-lived lock rather
than holding one across the whole `write_batch` call) is a materially
larger, separately-scoped piece of work with its own correctness
surface — not attempted here without evidence it is required, matching
this project's own established "correctness first, optimize only once
measurement justifies it" convention (`PHASE_RELATIONAL_INDEX_
BACKFILL_ADR.md`, `PHASE_RELATIONAL_ROW_STORAGE_RESULTS.md`).

## §12. Write skew is possible under Snapshot Isolation

**This is Snapshot Isolation, not Serializable isolation, and this
module never claims otherwise.** Two disjoint-write-set transactions
can each read a consistent snapshot, each independently decide (based
on that read) to make a change that is individually valid but jointly
violates an invariant neither transaction's own write-set could see,
and both commit successfully — SI's own well-known, accepted limitation
(the classic "two on-call doctors" scenario). Directly demonstrated,
not asserted, by `write_skew_is_possible_under_snapshot_isolation`:
two rows both start "on call" (`active = true`); each of two concurrent
transactions reads both rows, confirms at least one *other* row is
still on call, and takes only *itself* off call; both write-sets are
disjoint (transaction 1 writes only row 1, transaction 2 writes only
row 2), so §5's own conflict check — which only ever looks at each
transaction's own write-set — permits both to commit, and the combined
result leaves *both* rows off call, violating the "at least one on
call" invariant. This is not "fixed" by this increment, because fixing
it would require Serializable isolation (e.g. predicate locking or
serializable-snapshot-isolation's read/write conflict graph), which is
explicitly not what D10 specifies.

## §13. Autocommit primitive

```rust
impl TransactionManager {
    pub fn autocommit_put_row(&self, table_id: u32, values: &[Option<RelationalValue>]) -> Result<u64>;
    pub fn autocommit_delete_row(&self, table_id: u32, pk_values: &[RelationalValue]) -> Result<u64>;
}
```

`begin()` → one buffered op → `commit()`, nothing more — the reusable
primitive a future SQL executor's implicit-transaction statements
(a bare `INSERT` outside an explicit `BEGIN`) can call directly without
reimplementing transaction bookkeeping.

## §14. Resource limits

```rust
pub struct TxnLimits {
    pub max_write_set_ops: usize,        // default 10_000  (D27, verbatim)
    pub max_write_set_bytes: usize,      // default 16 MiB  (this increment's own addition)
    pub max_concurrent_transactions: usize, // default 1_000 (D27, verbatim)
}
```

`max_write_set_bytes` is not specified by D27's own text — a smallest-
necessary addition, defense in depth alongside the op-count limit (a
small number of enormous `TEXT`/`BLOB` values could otherwise exhaust
memory within the op-count budget alone). All three are checked
**before** the corresponding allocation (`begin()` rejects over the
concurrent-transaction cap before constructing a `Transaction`;
`record_op` rejects before inserting into the write-set `HashMap`).

## §15. Security

No `unsafe` anywhere in `txn.rs`. No `.unwrap()`/`.expect()`/`panic!`
on any caller-controlled or on-disk-derived input — every fallible
operation propagates a typed `Result`; the sole `.unwrap_or_else(|p|
p.into_inner())` pattern is lock-poison recovery, not a panic path.
Every numeric cast is a widening cast on already-in-memory, already-
bounded values (`usize -> u64`, `u16 -> usize`), never a narrowing
cast on untrusted input. Metrics are a fixed set of atomics —
structurally incapable of holding a key, value, principal, or SQL
text, so there is no sensitive-data-in-observability surface to audit.
No new logging was added.

**Authorization boundary unchanged**: this layer's public API accepts
only already-resolved `table_id`s and already-typed `RelationalValue`s
— there is no method that accepts a raw physical key, index id, or
catalog id from an external caller, so it provides no privilege-
escalation surface around whatever authorization a future SQL executor
performs before ever calling `put_row`/`delete_row` (D25's binder-level
authorization is the sole enforcement point, unchanged and undupli-
cated here — directly matching the already-established "authorization
lives at the binder" boundary).

**Deliberately out of scope this increment**: transactional DDL and
catalog-key conflict detection. Increment 6 established SQL DDL
*binding* (`BoundStatement`) with no execution path; `CatalogService`'s
own DDL methods (`create_table`, `create_index`, etc.) already use
their own `ddl_lock` + `write_batch` atomic mechanism, independent of
`Transaction`. Nothing in the Increment 7 spec requires routing DDL
through the new transaction type, and doing so speculatively — with no
SQL executor yet to drive it — would be exactly the kind of
unrequested scope expansion this project's own conventions reject.
Stated here explicitly as a scope boundary, not a silent omission.

## §16. Performance

See `PHASE_RELATIONAL_TRANSACTION_INCREMENT7_RESULTS.md` for the full
measured numbers (all from `cargo bench --bench transaction_bench`,
this machine, release profile). Headline, honest findings:

- `BEGIN` costs ~148 ns (snapshot capture + registry bookkeeping only).
- Snapshot-pinned reads (`get_as_of`) cost the same as unpinned reads
  (`get`) — no meaningful overhead from pinning a seq.
- `Transaction::get_row` costs ~4–5 µs regardless of whether it hits
  the engine or is served entirely from the local write-set overlay —
  because `resolve_table`'s own catalog-lookup cost (§"Performance,"
  `txn_resolve_table_vs_catalog_size`) dominates both paths; the local-
  overlay saving (avoiding one `get_as_of` call) is real but small next
  to the ~2.5–2.7 µs uncached catalog resolution every read still pays.
  No caching is added by this increment (matching the SQL binder's own
  prior "no automatic caching" finding, reused one layer lower) — this
  is reported as a finding, not fixed here.
- Commit latency is `fsync`-dominated (~3.4–5.1 ms across write-set
  sizes 1–128) — consistent with every other write path this project
  has measured on this machine.
- Conflict-validation cost is flat against table size at a fixed
  write-set size — confirms it is not a table scan (§5).
- Concurrent commit throughput on **one** table does not scale past
  ~1 (§11) — an honestly-reported, deliberate table-level-lock
  tradeoff, not a bug.

---

## Testing summary

42 new tests in `src/relational/txn_tests.rs`: lifecycle, read-your-own-
writes, snapshot consistency, conflict detection (same-key, `PRIMARY
KEY` both orderings, `UNIQUE` both orderings, intra-transaction
duplicate `UNIQUE`, delete-then-reinsert self-vacated `UNIQUE`,
multiple independent `UNIQUE` indexes, `NULL`-vs-`UNIQUE` semantics),
table/index atomic-commit verification, autocommit, write-skew
demonstration, five deterministic barrier-synchronized concurrency
tests (never sleep-based), resource-limit boundaries (×3), snapshot-
registry lifecycle, real automatic-compaction interaction, two real-
restart crash-recovery tests, a metrics-accounting test, an
authorization-boundary-unaffected test, and two `differential` tests —
a fixed scripted scenario and a **randomized, interleaved, multi-slot
`proptest`** scenario (`matches_reference_model_for_randomized_
interleaved_transactions`, 12 cases × up to 20 randomly-interleaved
BEGIN/PUT/DELETE/COMMIT/ROLLBACK operations across 3 simultaneously-
open transaction slots) — both checked against an independently
implemented Snapshot Isolation reference model (`ReferenceDb`/`RefTxn`,
built from scratch against a plain `BTreeMap` history, never calling
into `crate::relational::txn`), comparing every commit/abort decision
and final committed state, never using the production code as its own
oracle.
