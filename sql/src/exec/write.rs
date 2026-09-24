//! The write executor — `PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE.md`.
//! `Bound write → Plan → Write Executor → Transaction → TableStore/
//! Catalog/IndexStore → write_batch → commit`. Never a second write
//! path: every table-row mutation goes through `Transaction::put_row`/
//! `delete_row` (D10, already certified — row-shape validation,
//! `PRIMARY KEY`/`UNIQUE` conflict detection, and atomic table+index
//! `write_batch` construction all already live there, not duplicated
//! here); every catalog mutation goes through `CatalogService`'s own
//! already-atomic (`ddl_lock` + `write_batch`) DDL methods, or — for
//! `CREATE`/`DROP INDEX` — the already-certified online `IndexBuilder`
//! (item 39/40: never a second index implementation).

use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::{CatalogError, CatalogService};
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::{RelationalError, RelationalValue, Transaction};

use crate::bound::{BoundAssignment, BoundColumnDef, BoundInsert, BoundStatement};
use crate::error::{Result, SqlError};
use crate::exec::expr_eval::eval;
use crate::exec::operators::build_operator;
use crate::exec::{CancellationToken, ExecCtx, ExecLimits, ExecMetrics, RowContext};
use crate::plan::access::PhysicalAccess;
use crate::plan::physical::PhysicalPlan;
use crate::plan::Plan;

pub mod metrics;
pub use metrics::WriteMetrics;

/// Item 56/86/123: the smallest result shape the currently-approved
/// architecture needs — statement kind, success, and a rows-affected
/// count. No `RETURNING` (item 124: the current grammar/binder has no
/// representation for it — inspected, not guessed: `sql/src/ast.rs`'s
/// `Insert`/`Update`/`Delete` carry no returning clause at all), so no
/// result rows are ever produced for a write statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStatementKind {
    Insert,
    Update,
    Delete,
    Ddl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteResult {
    pub kind: WriteStatementKind,
    pub rows_affected: u64,
}

/// Executes one write-shaped `Plan` into the caller's own `&mut
/// Transaction` — never committing it (item 4: "execute into the
/// caller's existing transaction without committing it automatically").
/// `Plan::Query`/`Explain`/`Begin`/`Commit`/`Rollback` are rejected with
/// `UnsupportedExecution` — reads go through `crate::exec::execute`
/// instead (even inside the same transaction, item 116); transaction-
/// control statements are the caller's own `TransactionManager`/
/// `Transaction` lifecycle, never a second one built here (item 93).
#[allow(clippy::too_many_arguments)]
pub fn execute_write(
    plan: &Plan,
    txn: &mut Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    metrics: &WriteMetrics,
    cancellation: &CancellationToken,
) -> Result<WriteResult> {
    let result = execute_write_inner(
        plan,
        txn,
        table_store,
        catalog,
        index_builder,
        params,
        limits,
        metrics,
        cancellation,
    );
    // `execute_write` never calls `Transaction::commit` (item 4), so
    // the *concurrent-overlap* conflict case (D10's own freshness/
    // `UNIQUE` validation) can only ever be detected later, inside
    // `execute_write_autocommit`'s own `commit()` call, which records
    // it there. `execute_insert`'s own pre-write existence check (see
    // its doc comment) is a second, *earlier* place a `SqlError::
    // Conflict` can legitimately originate from — the *sequential*
    // duplicate-`PRIMARY KEY` case `Transaction::commit` structurally
    // cannot see at all. Both are the same error class from a caller's
    // point of view (item 118: a conflict must not collapse into the
    // same generic signal as an ordinary constraint/storage failure),
    // so both are classified as a write conflict here, regardless of
    // which one produced it.
    match &result {
        Ok(_) => {}
        Err(SqlError::Conflict { .. }) => metrics.record_write_conflict(),
        Err(_) if matches!(plan, Plan::Ddl(_)) => metrics.record_ddl_error(),
        Err(_) => metrics.record_dml_error(),
    }
    result
}

/// Item 92's own "autocommit execution foundation": `BEGIN → execute →
/// COMMIT`, one defined commit boundary per statement, reusing D10's
/// already-certified snapshot/commit mechanism verbatim (never a second
/// one). `Plan::Ddl` is a special case: `CatalogService`'s/
/// `IndexBuilder`'s own DDL methods are *already* atomic and durable
/// the instant they return `Ok` (their own internal `ddl_lock` +
/// `write_batch`, D-level decision predating this increment, §3 of the
/// architecture doc has the full account) — there is no SQL-level
/// `Transaction` boundary for DDL to participate in at all, so no
/// `commit()` call is made for it (a `Transaction` is still begun and
/// dropped/rolled-back around it purely so this function has one
/// uniform signature; that transaction's own snapshot is never used for
/// anything).
#[allow(clippy::too_many_arguments)]
pub fn execute_write_autocommit(
    plan: &Plan,
    txm: &rubixdb::relational::TransactionManager,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    metrics: &WriteMetrics,
    cancellation: &CancellationToken,
) -> Result<WriteResult> {
    let mut txn = txm.begin()?;
    let result = execute_write(
        plan,
        &mut txn,
        table_store,
        catalog,
        index_builder,
        params,
        limits,
        metrics,
        cancellation,
    );
    match result {
        Ok(write_result) => {
            if matches!(plan, Plan::Ddl(_)) {
                // Already durable -- see this function's own doc
                // comment. Discard the throwaway transaction's
                // snapshot without committing (a no-op write-set commit
                // would be harmless too, but rollback states the truth
                // more plainly: nothing about this transaction's own
                // write-set is what made the DDL durable).
                let _ = txn.rollback();
            } else {
                // Item 55's `rows_inserted`/`rows_updated`/`rows_
                // deleted`/statement counters were already recorded
                // once, provisionally, inside `execute_write` itself
                // (buffer-time — `PHASE_RELATIONAL_WRITE_EXECUTOR_
                // ARCHITECTURE.md` §9 records the honest "buffered, not
                // necessarily committed" accounting boundary this
                // implies); `commit()` failing here is the one place
                // that provisional count could end up wrong, so a
                // conflict specifically is recorded to make that
                // visible, without attempting to retroactively
                // "un-count" the provisional one (no counter supports
                // subtraction safely under concurrent readers).
                if let Err(e) = txn.commit() {
                    let sql_err: SqlError = e.into();
                    if matches!(sql_err, SqlError::Conflict { .. }) {
                        metrics.record_write_conflict();
                    } else {
                        metrics.record_dml_error();
                    }
                    return Err(sql_err);
                }
            }
            Ok(write_result)
        }
        Err(e) => {
            // `execute_write` already recorded the error metric above;
            // the buffered write-set (if any) is simply dropped with
            // the transaction (implicit rollback, D10-certified) --
            // never committed.
            let _ = txn.rollback();
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_write_inner(
    plan: &Plan,
    txn: &mut Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    metrics: &WriteMetrics,
    cancellation: &CancellationToken,
) -> Result<WriteResult> {
    match plan {
        Plan::Insert(insert) => {
            metrics.record_insert_statement();
            let n = execute_insert(insert, txn, table_store, catalog, index_builder, params, limits, cancellation)?;
            metrics.record_rows_inserted(n);
            Ok(WriteResult {
                kind: WriteStatementKind::Insert,
                rows_affected: n,
            })
        }
        Plan::Update {
            table,
            assignments,
            access,
            ..
        } => {
            metrics.record_update_statement();
            let n = execute_update(
                table.table_id,
                assignments,
                access,
                txn,
                table_store,
                catalog,
                index_builder,
                params,
                limits,
                cancellation,
            )?;
            metrics.record_rows_updated(n);
            Ok(WriteResult {
                kind: WriteStatementKind::Update,
                rows_affected: n,
            })
        }
        Plan::Delete { table, access, .. } => {
            metrics.record_delete_statement();
            let n = execute_delete(table.table_id, access, txn, table_store, catalog, index_builder, params, limits, cancellation)?;
            metrics.record_rows_deleted(n);
            Ok(WriteResult {
                kind: WriteStatementKind::Delete,
                rows_affected: n,
            })
        }
        Plan::Ddl(bound) => {
            metrics.record_ddl_statement();
            execute_ddl(bound, catalog, index_builder)
        }
        Plan::Begin | Plan::Commit | Plan::Rollback | Plan::Query { .. } | Plan::Explain(_) => {
            Err(SqlError::UnsupportedExecution {
                detail: "this statement kind is not executed by crate::exec::write (transaction-control statements are the caller's own TransactionManager/Transaction lifecycle, item 5/92/93; SELECT uses crate::exec::execute)".to_string(),
            })
        }
    }
}

fn check_point(
    cancellation: &CancellationToken,
    deadline_at: Option<std::time::Instant>,
) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(SqlError::Cancelled);
    }
    if let Some(at) = deadline_at {
        if std::time::Instant::now() >= at {
            return Err(SqlError::DeadlineExceeded);
        }
    }
    Ok(())
}

fn deadline_at(limits: &ExecLimits) -> Option<std::time::Instant> {
    limits.deadline.map(|d| std::time::Instant::now() + d)
}

// =======================================================================
// INSERT (items 6-13)
// =======================================================================

#[allow(clippy::too_many_arguments)]
fn execute_insert(
    insert: &BoundInsert,
    txn: &mut Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
) -> Result<u64> {
    // `Transaction::put_row` is a generic upsert-at-key primitive
    // shared with `UPDATE` (see `execute_update`'s own doc comment) --
    // by itself it has no notion of "this key must not already exist",
    // and its commit-time freshness/`UNIQUE` validation only catches a
    // *concurrently racing* insert of the same `PRIMARY KEY` (the two
    // committed snapshots genuinely differ), never a plain, later,
    // non-overlapping `INSERT` of a `PRIMARY KEY` some earlier,
    // already-committed transaction used (both snapshots agree, so
    // nothing there looks like a conflict) -- confirmed by reading
    // `Transaction::commit`/`validate_and_build_ops` (src/relational/
    // txn.rs): `unique_checks` only ever iterates `IndexKind::Unique`
    // *secondary* indexes, never the table's own `PRIMARY KEY`, and a
    // differential test against an independent reference model (`sql/
    // src/write_tests.rs::differential`) caught the resulting silent-
    // overwrite gap directly. `INSERT`, unlike `UPDATE`, is the one
    // caller that actually needs "must be a new row" semantics, so this
    // existence check belongs here, not in the shared primitive. It is
    // not a second, independent conflict detector (item 11's own
    // concern): it reuses `Transaction::get_row` (the same snapshot-
    // and write-set-overlay-correct read every other statement kind
    // uses) entirely *within* this one transaction, and the commit-time
    // freshness check remains the sole authority for the concurrent
    // case (two transactions racing to insert the same key are still
    // resolved there, unchanged -- this check only closes the
    // sequential gap freshness cannot see). Reported as `SqlError::
    // Conflict`, the exact class `RelationalError::Conflict` already
    // maps to, so a `PRIMARY KEY` violation and a concurrent write
    // conflict are indistinguishable to a caller in the same way a
    // `UNIQUE` violation already is -- consistent, not a new error
    // class.
    let table = catalog.get_table(insert.table_id)?.ok_or_else(|| {
        SqlError::Storage("a target table no longer resolves in the catalog".to_string())
    })?;
    let at = deadline_at(limits);
    // Phase 1 (read-only): evaluate every row's `VALUES` expressions
    // up front, one shared `ExecCtx` borrowing `txn` immutably for
    // parameter resolution only (`INSERT`'s own grammar never binds a
    // `Column` reference or a correlated access, `crate::bind::dml::
    // bind_insert`, so an empty `RowContext` is always correct/
    // sufficient here). Bounded by `SqlLimits::max_values_rows`,
    // already enforced at bind time -- no new resource risk from
    // evaluating every row before writing any of them.
    let evaluated: Vec<Vec<Option<RelationalValue>>> = {
        let metrics = ExecMetrics::default();
        let ec = ExecCtx::new(
            txn,
            table_store,
            index_builder,
            params,
            limits,
            &metrics,
            cancellation,
        );
        let ctx = RowContext::new();
        let mut out = Vec::with_capacity(insert.rows.len());
        for row in &insert.rows {
            check_point(cancellation, at)?;
            let values: Vec<Option<RelationalValue>> = row
                .iter()
                .map(|expr| eval(expr, &ctx, &ec))
                .collect::<Result<_>>()?;
            out.push(values);
        }
        out
    };

    // Phase 2 (write): the borrow above has ended -- `txn` is free to
    // mutate. Item 7: every row of this statement is buffered into the
    // *same* transaction's write-set, never committed per row
    // (`execute_write`'s own caller decides the commit boundary) -- a
    // validation failure on row N leaves rows before it buffered but
    // nothing durable anywhere until that one, later commit.
    // `PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE.md` §4 has the full
    // atomicity argument, including the one honestly-documented
    // residual gap for an explicit multi-statement transaction.
    let mut inserted = 0u64;
    for values in evaluated {
        check_point(cancellation, at)?;
        let pk = pk_values_of(&values, &table.pk_ordinals)?;
        if txn.get_row(insert.table_id, &pk)?.is_some() {
            return Err(SqlError::Conflict {
                detail: "duplicate key value violates primary key constraint".to_string(),
            });
        }
        txn.put_row(insert.table_id, &values)?;
        inserted += 1;
    }
    Ok(inserted)
}

/// Shared by `execute_insert`'s own existence check and `find_target_pks`:
/// a row's `PRIMARY KEY` values, in `pk_ordinals` order. `PRIMARY KEY`
/// columns are never `NULL` (D6, enforced at `Transaction::put_row`'s own
/// `validate_row_shape`), so a missing value here means this crate's own
/// column-ordinal bookkeeping is inconsistent with the catalog -- a
/// defensive `Storage` error, not a user-facing one.
fn pk_values_of(
    row: &[Option<RelationalValue>],
    pk_ordinals: &[u16],
) -> Result<Vec<RelationalValue>> {
    pk_ordinals
        .iter()
        .map(|&ord| {
            row.get(ord as usize).cloned().flatten().ok_or_else(|| {
                SqlError::Storage(
                    "a row is missing its own primary-key value (D6: PK columns are never NULL)"
                        .to_string(),
                )
            })
        })
        .collect()
}

// =======================================================================
// UPDATE (items 17-22)
// =======================================================================

#[allow(clippy::too_many_arguments)]
fn execute_update(
    table_id: u32,
    assignments: &[BoundAssignment],
    access: &PhysicalAccess,
    txn: &mut Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
) -> Result<u64> {
    let table = catalog.get_table(table_id)?.ok_or_else(|| {
        SqlError::Storage("a target table no longer resolves in the catalog".to_string())
    })?;
    let table_ref = access_table_ref(access);
    let target_pks = find_target_pks(
        access,
        &table.pk_ordinals,
        txn,
        table_store,
        index_builder,
        params,
        limits,
        cancellation,
    )?;
    let at = deadline_at(limits);
    let metrics = ExecMetrics::default();

    let mut updated = 0u64;
    for pk in target_pks {
        check_point(cancellation, at)?;
        // Re-fetched fresh, through the same certified, snapshot- and
        // write-set-overlay-correct `Transaction::get_row` the read
        // executor itself uses (item 14) -- never the row this
        // statement's own earlier target-finding pass happened to see,
        // which could in principle be a different physical read.
        let old_row = txn.get_row(table_id, &pk)?.ok_or_else(|| {
            SqlError::Storage(
                "a target row disappeared between being matched and being updated".to_string(),
            )
        })?;
        let row_ctx = RowContext::single(table_ref, old_row.clone());
        let mut new_row = old_row;
        {
            // Item 18: every `SET` expression evaluates against the
            // *same* pre-update row state (`row_ctx`, built once above,
            // never mutated mid-loop) -- standard SQL simultaneous-
            // assignment semantics (`UPDATE t SET a = b, b = a` swaps
            // using the old `a`/`b` for both, never a sequentially-
            // updated intermediate value). The bound representation
            // (`BoundAssignment { ordinal, value }`) carries no marker
            // suggesting otherwise, and this is the strongest, most
            // standard-consistent choice available where the
            // architecture does not already define one
            // (`PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE.md` §5
            // records this as the decision item 18 asked for). Scoped
            // so `ec`'s own immutable borrow of `txn` ends before the
            // `put_row` call below needs a mutable one.
            let ec = ExecCtx::new(
                txn,
                table_store,
                index_builder,
                params,
                limits,
                &metrics,
                cancellation,
            );
            for assignment in assignments {
                let value = eval(&assignment.value, &row_ctx, &ec)?;
                new_row[assignment.ordinal as usize] = value;
            }
        }
        // `PRIMARY KEY` updates are already rejected at bind time
        // (`crate::bind::dml::bind_update`, D6 — verified, not assumed:
        // `bind_tests::update_of_primary_key_column_is_rejected`), so
        // `new_row`'s own primary-key columns are always identical to
        // `old_row`'s -- `Transaction::put_row` upserting at the same
        // key is always the correct, complete operation (old/new
        // secondary-index-entry delete+insert included atomically,
        // D11, already certified); the delete-old-physical-row-plus-
        // insert-new-physical-row path item 19 describes as the
        // fallback for a PK-changing UPDATE is structurally unreachable
        // here and is not implemented.
        if row_ctx.row_for(table_ref) == Some(&new_row) {
            // Item 22: nothing actually changed -- `put_row` would
            // still perform a real (if no-op-shaped) write; skipping it
            // entirely avoids the unnecessary index-entry rewrite this
            // item names as a production performance requirement.
            // `rows_affected` still counts it (item 123: "number of
            // rows actually changed or matched" -- matched, since this
            // row was indeed matched by the predicate and processed).
            updated += 1;
            continue;
        }
        txn.put_row(table_id, &new_row)?;
        updated += 1;
    }
    Ok(updated)
}

// =======================================================================
// DELETE (items 14-16, 23)
// =======================================================================

#[allow(clippy::too_many_arguments)]
fn execute_delete(
    table_id: u32,
    access: &PhysicalAccess,
    txn: &mut Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
) -> Result<u64> {
    let table = catalog.get_table(table_id)?.ok_or_else(|| {
        SqlError::Storage("a target table no longer resolves in the catalog".to_string())
    })?;
    let target_pks = find_target_pks(
        access,
        &table.pk_ordinals,
        txn,
        table_store,
        index_builder,
        params,
        limits,
        cancellation,
    )?;
    let at = deadline_at(limits);

    let mut deleted = 0u64;
    for pk in target_pks {
        check_point(cancellation, at)?;
        // Item 14: the row's own current existence is authoritative
        // through `delete_row` itself (D10-certified); no re-fetch is
        // needed here since a `DELETE` needs only the already-collected
        // primary key, never the row's other column values.
        txn.delete_row(table_id, &pk)?;
        deleted += 1;
    }
    Ok(deleted)
}

/// Shared by `UPDATE`/`DELETE` (item 14/15/47/48): drives the *exact
/// same* read-side access machinery `crate::exec::execute` uses for
/// `SELECT` (`crate::exec::operators::build_operator` over a
/// `PhysicalPlan::Access` wrapping the plan's own already-chosen
/// `PhysicalAccess`) — the planner's residual-predicate guarantee (item
/// 15) and every `PkLookup`/`IndexScan`/`SeqScan` correctness property
/// Increment 9 already certified apply here completely unmodified,
/// never reimplemented; the residual predicate is evaluated **exactly
/// once**, during this pass.
///
/// Collects only each matching row's own `PRIMARY KEY` values, never
/// the full row (item 47/48: "must NOT create an unbounded in-memory
/// vector of every affected row" — a `Vec` of PK tuples is a materially
/// smaller, still-bounded footprint than one of full rows), and fails
/// closed with a controlled `ResourceLimit` the instant `ExecLimits::
/// max_dml_target_rows` is exceeded **while collecting**, not only
/// after — a predicate matching millions of rows can never grow this
/// `Vec` past that bound.
#[allow(clippy::too_many_arguments)]
fn find_target_pks(
    access: &PhysicalAccess,
    pk_ordinals: &[u16],
    txn: &Transaction,
    table_store: &TableStore,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
) -> Result<Vec<Vec<RelationalValue>>> {
    let table_ref = access_table_ref(access);
    let metrics = ExecMetrics::default();
    let ec = ExecCtx::new(
        txn,
        table_store,
        index_builder,
        params,
        limits,
        &metrics,
        cancellation,
    );
    let mut operator = build_operator(
        &PhysicalPlan::Access(access.clone()),
        &RowContext::new(),
        &ec,
    )?;
    let mut out = Vec::new();
    loop {
        ec.check()?;
        match operator.next(&ec)? {
            None => break,
            Some(tuple) => {
                if out.len() >= limits.max_dml_target_rows {
                    return Err(SqlError::ResourceLimit {
                        detail: format!(
                            "statement matches more than max_dml_target_rows ({}) rows",
                            limits.max_dml_target_rows
                        ),
                    });
                }
                let row =
                    tuple
                        .ctx
                        .row_for(table_ref)
                        .ok_or_else(|| SqlError::UnsupportedExecution {
                            detail: "internal: target access did not bind its own table_ref"
                                .to_string(),
                        })?;
                out.push(pk_values_of(row, pk_ordinals)?);
            }
        }
    }
    Ok(out)
}

fn access_table_ref(access: &PhysicalAccess) -> u32 {
    match access {
        PhysicalAccess::PkLookup { table_ref, .. }
        | PhysicalAccess::IndexScan { table_ref, .. }
        | PhysicalAccess::SeqScan { table_ref, .. } => *table_ref,
    }
}

// =======================================================================
// DDL (items 35-45, 96)
// =======================================================================

fn execute_ddl(
    bound: &BoundStatement,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
) -> Result<WriteResult> {
    let n = match bound {
        BoundStatement::CreateSchema(cs) => match catalog.create_schema(cs.database_id, &cs.name) {
            Ok(_) => 1,
            Err(CatalogError::AlreadyExists { .. }) if cs.if_not_exists => 0,
            Err(e) => return Err(e.into()),
        },
        BoundStatement::CreateTable(ct) => {
            let columns: Vec<ColumnDef> = ct.columns.iter().map(bound_column_to_catalog).collect();
            match catalog.create_table(ct.schema_id, &ct.name, &columns, &ct.pk_ordinals) {
                Ok(_) => 1,
                Err(CatalogError::AlreadyExists { .. }) if ct.if_not_exists => 0,
                Err(e) => return Err(e.into()),
            }
        }
        BoundStatement::DropTable(dt) => match dt.table_id {
            // `None` only when `IF EXISTS` was given and the table
            // genuinely does not exist -- already resolved at bind
            // time (`BoundDropTable::table_id`'s own doc comment); a
            // real no-op, not an error.
            None => 0,
            Some(table_id) => {
                catalog.drop_table(table_id)?;
                1
            }
        },
        BoundStatement::CreateIndex(ci) => {
            // Item 39: reuse the already-certified online-build
            // protocol verbatim -- never `CatalogService::create_index`
            // directly, which only inserts the catalog row without
            // backfilling or activating it.
            match index_builder.create_index_online(
                ci.table_id,
                &ci.name,
                ci.kind,
                &ci.column_ordinals,
            ) {
                Ok(_) => 1,
                Err(RelationalError::Catalog(CatalogError::AlreadyExists { .. }))
                    if ci.if_not_exists =>
                {
                    0
                }
                Err(e) => return Err(e.into()),
            }
        }
        BoundStatement::DropIndex(di) => match di.index_id {
            None => 0,
            Some(index_id) => {
                index_builder.drop_index_online(index_id)?;
                1
            }
        },
        // No `CatalogService::create_database` primitive exists
        // (inspected, not guessed: `grep -n "pub fn create_database"
        // src/catalog/service.rs` has zero matches) -- `bootstrap()` is
        // the only current way a database row is ever created. Adding
        // one is a materially larger catalog-layer primitive than this
        // increment's own scope justifies without evidence a real
        // multi-database workload needs it; refused with a controlled
        // error rather than faked (item 35: "ONLY execute the DDL forms
        // that the current architecture and catalog/index
        // implementations can safely support").
        BoundStatement::CreateDatabase(_) => {
            return Err(SqlError::UnsupportedExecution {
                detail:
                    "CREATE DATABASE has no catalog execution primitive in the current architecture"
                        .to_string(),
            })
        }
        // Structurally unreachable in practice (`execute_write_inner`
        // only ever calls `execute_ddl` for `Plan::Ddl`, which the
        // planner only ever builds from a DDL-shaped `BoundStatement`),
        // but defensive nonetheless: `other`'s `Debug` output is never
        // printed here even so -- a `BoundStatement::Insert`/`Update`
        // carries bound literal *values* from the statement's own text,
        // and item 50 forbids row values in error messages regardless
        // of how a caller reached this branch.
        other => {
            return Err(SqlError::UnsupportedExecution {
                detail: format!(
                    "{} is not a DDL statement this write executor executes",
                    bound_statement_kind_name(other)
                ),
            })
        }
    };
    Ok(WriteResult {
        kind: WriteStatementKind::Ddl,
        rows_affected: n,
    })
}

/// The statement's own variant name only -- never its contents (item
/// 50: a `Select`/`Insert`/`Update`/`Delete` variant carries bound
/// literal values from the statement's own text, which must never
/// reach an error message).
fn bound_statement_kind_name(s: &BoundStatement) -> &'static str {
    match s {
        BoundStatement::Select(_) => "SELECT",
        BoundStatement::Insert(_) => "INSERT",
        BoundStatement::Update(_) => "UPDATE",
        BoundStatement::Delete(_) => "DELETE",
        BoundStatement::CreateDatabase(_) => "CREATE DATABASE",
        BoundStatement::CreateSchema(_) => "CREATE SCHEMA",
        BoundStatement::CreateTable(_) => "CREATE TABLE",
        BoundStatement::DropTable(_) => "DROP TABLE",
        BoundStatement::CreateIndex(_) => "CREATE INDEX",
        BoundStatement::DropIndex(_) => "DROP INDEX",
        BoundStatement::Explain(_) => "EXPLAIN",
        BoundStatement::Begin => "BEGIN",
        BoundStatement::Commit => "COMMIT",
        BoundStatement::Rollback => "ROLLBACK",
    }
}

fn bound_column_to_catalog(c: &BoundColumnDef) -> ColumnDef {
    ColumnDef {
        name: c.name.clone(),
        data_type: c.data_type,
        nullable: c.nullable,
        default_value: c.default_value.clone(),
        type_params: c.type_params.clone(),
    }
}
