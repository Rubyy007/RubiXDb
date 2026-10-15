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

use rubixdb::catalog::service::{ColumnDef, NewTableCheck, NewTableForeignKey};
use rubixdb::catalog::{CatalogError, CatalogService};
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::{RelationalError, RelationalValue, RowChange, Transaction};

use std::collections::HashMap;

use rubixdb::catalog::schema::ColumnRow;

use crate::aggregate::GroupingKey;
use crate::bound::{
    BoundAlterOp, BoundArbiter, BoundAssignment, BoundColumnDef, BoundConflictAction, BoundExpr,
    BoundExprKind, BoundInsert, BoundOnConflict, BoundReturning, BoundStatement, ColumnRef,
    TableRefId,
};
use crate::check::{self, CompiledCheck};
use crate::error::{Result, SqlError};
use crate::exec::expr_eval::{eval, eval_predicate};
use crate::exec::operators::build_operator;
use crate::exec::{
    CancellationToken, CorrelatedUsage, ExecCtx, ExecLimits, ExecMetrics, ResultSchema, RowContext,
};
use crate::limits::SqlLimits;
use crate::plan::access::PhysicalAccess;
use crate::plan::physical::PhysicalPlan;
use crate::plan::Plan;

pub mod metrics;
pub use metrics::WriteMetrics;

/// Which kind of statement a `WriteResult` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStatementKind {
    Insert,
    Update,
    Delete,
    Ddl,
}

/// The rows a `RETURNING` clause handed back, in the order the statement changed
/// them. Produced by the change itself (the very row values that were written or
/// deleted), never by a second query. For a statement that commits on its own
/// (`execute_write_autocommit`) they are returned only after the commit succeeded.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnedRows {
    pub schema: ResultSchema,
    pub rows: Vec<Vec<Option<RelationalValue>>>,
}

/// What a write statement reports: its kind, the number of rows it changed and
/// -- when it has a `RETURNING` clause -- the rows themselves.
///
/// `rows_affected` is exactly the number of rows the statement inserted, updated
/// (a row `ON CONFLICT DO UPDATE` or `UPDATE` matched counts, whether or not its
/// values changed) or deleted. Rows an `ON CONFLICT` clause skipped are not
/// counted. With `RETURNING`, `returning.rows.len() == rows_affected` always.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteResult {
    pub kind: WriteStatementKind,
    pub rows_affected: u64,
    pub returning: Option<ReturnedRows>,
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
/// Runs `plan` as its own transaction: begin, execute, commit.
///
/// An `INSERT ... ON CONFLICT` whose commit loses a race to a concurrent writer
/// of the same key (a `Conflict`) is run again, from a fresh snapshot, up to
/// `ExecLimits::max_upsert_attempts` times: the retry sees the winner's row and
/// arbitrates on it (`DO NOTHING` skips, `DO UPDATE` updates), which is what
/// "atomic upsert" means to a caller -- two concurrent upserts of one key end as
/// one row and no lost update, rather than one of them failing with a conflict it
/// could not have avoided. Any other statement is run once. Each attempt is the
/// whole statement against a new snapshot: nothing of a failed attempt survives
/// (its write-set is dropped with its transaction).
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
    let attempts = match plan {
        Plan::Insert(insert) if insert.on_conflict.is_some() => limits.max_upsert_attempts.max(1),
        _ => 1,
    };
    let mut attempt = 1u32;
    loop {
        let result = execute_write_autocommit_once(
            plan,
            txm,
            table_store,
            catalog,
            index_builder,
            params,
            limits,
            metrics,
            cancellation,
        );
        match result {
            Err(SqlError::Conflict { .. }) if attempt < attempts => {
                if cancellation.is_cancelled() {
                    return Err(SqlError::Cancelled);
                }
                metrics.record_upsert_retry();
                // A short, growing, jittered pause so racing writers do not retry in
                // lock step.
                let jitter = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| u64::from(d.subsec_nanos()))
                    .unwrap_or(0)
                    % 97;
                let micros = (50u64 << attempt.min(8)) + jitter * 10;
                std::thread::sleep(std::time::Duration::from_micros(micros));
                attempt += 1;
            }
            other => return other,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_write_autocommit_once(
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
    let _ = crate::exec::check_relation_limits(plan, limits)?;
    match plan {
        Plan::Insert(insert) => {
            metrics.record_insert_statement();
            let out = within_savepoint(txn, |txn| {
                execute_insert(
                    insert,
                    txn,
                    table_store,
                    catalog,
                    index_builder,
                    params,
                    limits,
                    cancellation,
                )
            })?;
            metrics.record_rows_inserted(out.inserted);
            metrics.record_rows_updated(out.updated);
            metrics.record_upsert_conflict_rows_skipped(out.skipped);
            if let Some(r) = &out.returning {
                metrics.record_rows_returned(r.rows.len() as u64);
            }
            Ok(WriteResult {
                kind: WriteStatementKind::Insert,
                rows_affected: out.inserted + out.updated,
                returning: out.returning,
            })
        }
        Plan::Update {
            table,
            assignments,
            access,
            returning,
            ..
        } => {
            metrics.record_update_statement();
            let (n, returned) = within_savepoint(txn, |txn| {
                execute_update(
                    table.table_id,
                    assignments,
                    access,
                    returning.as_ref(),
                    txn,
                    table_store,
                    catalog,
                    index_builder,
                    params,
                    limits,
                    cancellation,
                )
            })?;
            metrics.record_rows_updated(n);
            if let Some(r) = &returned {
                metrics.record_rows_returned(r.rows.len() as u64);
            }
            Ok(WriteResult {
                kind: WriteStatementKind::Update,
                rows_affected: n,
                returning: returned,
            })
        }
        Plan::Delete {
            table,
            access,
            returning,
            ..
        } => {
            metrics.record_delete_statement();
            let (n, returned) = within_savepoint(txn, |txn| {
                execute_delete(
                    table.table_id,
                    access,
                    returning.as_ref(),
                    txn,
                    table_store,
                    catalog,
                    index_builder,
                    params,
                    limits,
                    cancellation,
                )
            })?;
            metrics.record_rows_deleted(n);
            if let Some(r) = &returned {
                metrics.record_rows_returned(r.rows.len() as u64);
            }
            Ok(WriteResult {
                kind: WriteStatementKind::Delete,
                rows_affected: n,
                returning: returned,
            })
        }
        Plan::Ddl(bound) => {
            metrics.record_ddl_statement();
            execute_ddl(
                bound,
                txn,
                table_store,
                catalog,
                index_builder,
                params,
                limits,
                cancellation,
            )
        }
        Plan::Begin | Plan::Commit | Plan::Rollback | Plan::Query { .. } | Plan::Explain(_) => {
            Err(SqlError::UnsupportedExecution {
                detail: "this statement kind is not executed by crate::exec::write (transaction-control statements are the caller's own TransactionManager/Transaction lifecycle, item 5/92/93; SELECT uses crate::exec::execute)".to_string(),
            })
        }
    }
}

/// Checks `rows` (rows a statement is about to write, or has cascaded into) against
/// the `CHECK` constraints of `table_id`. The error names the table and the
/// constraint, never a value.
#[allow(clippy::too_many_arguments)]
fn check_rows<'a>(
    txn: &Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
    table_id: u32,
    rows: impl IntoIterator<Item = &'a Row>,
) -> Result<()> {
    let checks = check::compile_table_checks(catalog, table_id, &SqlLimits::default())?;
    check_rows_against(
        txn,
        table_store,
        catalog,
        index_builder,
        params,
        limits,
        cancellation,
        table_id,
        &checks,
        rows,
    )
}

#[allow(clippy::too_many_arguments)]
fn check_rows_against<'a>(
    txn: &Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
    table_id: u32,
    checks: &[CompiledCheck],
    rows: impl IntoIterator<Item = &'a Row>,
) -> Result<()> {
    if checks.is_empty() {
        return Ok(());
    }
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
    for row in rows {
        if let Some(failed) = check::first_violation(checks, row, &ec)? {
            let table = catalog
                .get_table(table_id)?
                .map(|t| t.name)
                .unwrap_or_else(|| format!("#{table_id}"));
            return Err(check::violation(&table, failed));
        }
    }
    Ok(())
}

/// Checks the child rows a cascade wrote against their own tables' constraints.
#[allow(clippy::too_many_arguments)]
fn check_cascaded(
    txn: &Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
    cascaded: &[rubixdb::relational::CascadedWrite],
) -> Result<()> {
    let mut by_table: Vec<u32> = cascaded.iter().map(|c| c.table_id).collect();
    by_table.sort_unstable();
    by_table.dedup();
    for table_id in by_table {
        check_rows(
            txn,
            table_store,
            catalog,
            index_builder,
            params,
            limits,
            cancellation,
            table_id,
            cascaded
                .iter()
                .filter(|c| c.table_id == table_id)
                .map(|c| &c.row),
        )?;
    }
    Ok(())
}

/// Runs one DML statement so that it either buffers all of its writes into the
/// transaction or none: on failure -- a violated constraint, a cascade that hits a
/// limit, a failing expression half way through -- everything the statement
/// buffered is discarded and the transaction's earlier statements are untouched.
fn within_savepoint<T>(
    txn: &mut Transaction,
    run: impl FnOnce(&mut Transaction) -> Result<T>,
) -> Result<T> {
    let savepoint = txn.savepoint();
    match run(txn) {
        Ok(v) => {
            txn.release_savepoint(savepoint);
            Ok(v)
        }
        Err(e) => {
            txn.rollback_to(savepoint);
            Err(e)
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
// RETURNING (SQL Phase 2a)
// =======================================================================

type Row = Vec<Option<RelationalValue>>;

/// Collects the rows of a `RETURNING` clause as the statement changes them. Every
/// row goes in as the very value the statement wrote (or, for `DELETE`, removed);
/// the clause's expressions are evaluated against it with the engine's own
/// expression evaluator. The rows are bounded by `max_result_rows` and by
/// `max_materialized_bytes`, failing closed.
struct ReturningSink<'a> {
    spec: &'a BoundReturning,
    rows: Vec<Row>,
    bytes: usize,
}

impl<'a> ReturningSink<'a> {
    fn new(spec: Option<&'a BoundReturning>) -> Option<Self> {
        spec.map(|spec| ReturningSink {
            spec,
            rows: Vec::new(),
            bytes: 0,
        })
    }

    fn push(
        &mut self,
        row: &[Option<RelationalValue>],
        ec: &ExecCtx<'_>,
        limits: &ExecLimits,
    ) -> Result<()> {
        if self.rows.len() >= limits.max_result_rows {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "RETURNING would return more than max_result_rows ({}) rows",
                    limits.max_result_rows
                ),
            });
        }
        let ctx = RowContext::single(self.spec.table_ref, row.to_vec());
        let values = self
            .spec
            .items
            .iter()
            .map(|item| eval(&item.expr, &ctx, ec))
            .collect::<Result<Vec<_>>>()?;
        self.bytes = self
            .bytes
            .saturating_add(crate::exec::setop::row_bytes(&values));
        if self.bytes > limits.max_materialized_bytes {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "RETURNING would hold more than max_materialized_bytes ({}) of rows",
                    limits.max_materialized_bytes
                ),
            });
        }
        self.rows.push(values);
        Ok(())
    }

    fn finish(self) -> ReturnedRows {
        ReturnedRows {
            schema: ResultSchema::from_projection(&self.spec.items),
            rows: self.rows,
        }
    }
}

// =======================================================================
// INSERT [... SELECT] [ON CONFLICT ...] [RETURNING ...]
// =======================================================================

struct InsertOutcome {
    inserted: u64,
    updated: u64,
    skipped: u64,
    returning: Option<ReturnedRows>,
}

/// A row the statement has decided to write, and whether it differs from what the
/// transaction already holds.
struct PendingRow {
    row: Row,
    /// The row this one replaces (an `ON CONFLICT DO UPDATE`), `None` for a new row.
    old: Option<Row>,
    write: bool,
}

/// What the statement has decided so far, by primary key and -- for each unique
/// index it arbitrates on -- by the index's value. A later proposed row is checked
/// against this *and* against the transaction, so a conflict with a row the same
/// statement inserted or updated earlier is seen exactly like any other.
struct Pending {
    rows: Vec<PendingRow>,
    by_pk: HashMap<GroupingKey, usize>,
    by_arbiter: Vec<HashMap<GroupingKey, usize>>,
}

fn pk_key(pk: &[RelationalValue]) -> GroupingKey {
    GroupingKey::from_values(&pk.iter().cloned().map(Some).collect::<Vec<_>>())
}

/// The key of a row under a unique index, or `None` when any indexed value is
/// `NULL` (a `NULL` never equals another, so such a row conflicts with nothing).
fn arbiter_key(row: &[Option<RelationalValue>], ordinals: &[u16]) -> Option<GroupingKey> {
    let vals: Vec<Option<RelationalValue>> = ordinals
        .iter()
        .map(|&o| row.get(o as usize).cloned().flatten())
        .collect();
    if vals.iter().any(Option::is_none) {
        None
    } else {
        Some(GroupingKey::from_values(&vals))
    }
}

impl Pending {
    fn new(arbiters: &[BoundArbiter]) -> Self {
        Pending {
            rows: Vec::new(),
            by_pk: HashMap::new(),
            by_arbiter: vec![HashMap::new(); arbiters.len()],
        }
    }

    fn add(
        &mut self,
        pk: Vec<RelationalValue>,
        row: Row,
        old: Option<Row>,
        write: bool,
        arbiters: &[BoundArbiter],
    ) {
        let idx = self.rows.len();
        self.by_pk.insert(pk_key(&pk), idx);
        for (ai, arb) in arbiters.iter().enumerate() {
            if arb.index_id.is_some() {
                if let Some(k) = arbiter_key(&row, &arb.ordinals) {
                    self.by_arbiter[ai].insert(k, idx);
                }
            }
        }
        self.rows.push(PendingRow { row, old, write });
    }
}

/// The row a proposed row conflicts with.
enum Hit {
    /// One this statement has already inserted or updated.
    Pending,
    /// One the transaction can see (its snapshot plus its own earlier writes).
    Committed { pk: Vec<RelationalValue>, row: Row },
}

/// Everything conflict detection needs to read the table as the transaction sees it.
struct Lookup<'a> {
    txn: &'a Transaction,
    table_store: &'a TableStore,
    catalog: &'a CatalogService,
    index_builder: &'a IndexBuilder,
    params: &'a [Option<RelationalValue>],
    limits: &'a ExecLimits,
    cancellation: &'a CancellationToken,
    table_id: u32,
    pk_ordinals: &'a [u16],
    columns: &'a [ColumnRow],
}

impl Lookup<'_> {
    /// The primary keys of the rows the transaction sees with `ordinals = vals`,
    /// found by the same access machinery `UPDATE` / `DELETE` use (an equality
    /// lookup on the unique index when one is ready, a scan otherwise), so the
    /// transaction's own earlier writes are part of what is seen.
    fn pks_by_columns(
        &self,
        ordinals: &[u16],
        vals: &[Option<RelationalValue>],
    ) -> Result<Vec<Vec<RelationalValue>>> {
        let mut predicate: Option<BoundExpr> = None;
        for (&ordinal, value) in ordinals.iter().zip(vals) {
            let column = self
                .columns
                .iter()
                .find(|c| c.ordinal == ordinal)
                .ok_or_else(|| {
                    SqlError::Storage("an ON CONFLICT column no longer resolves".to_string())
                })?;
            let ty = crate::bind::ddl::relational_type_of(column)?;
            let eq = BoundExpr {
                kind: BoundExprKind::BinaryOp {
                    left: Box::new(BoundExpr {
                        kind: BoundExprKind::Column(ColumnRef {
                            table_ref: TableRefId(0),
                            table_id: self.table_id,
                            ordinal,
                        }),
                        ty: Some(ty),
                        nullable: column.nullable,
                    }),
                    op: crate::ast::BinaryOp::Eq,
                    right: Box::new(BoundExpr {
                        kind: BoundExprKind::Literal(value.clone()),
                        ty: Some(ty),
                        nullable: false,
                    }),
                },
                ty: Some(rubixdb::relational::RelationalType::Boolean),
                nullable: false,
            };
            predicate = Some(match predicate {
                Some(p) => crate::plan::expr_util::and_all(p, eq),
                None => eq,
            });
        }
        let access = crate::plan::access::plan_table_access(
            self.table_id,
            0,
            predicate.as_ref(),
            self.catalog,
            &crate::plan::metrics::PlannerMetrics::default(),
        )?;
        let (pks, _) = find_target_pks(
            &access,
            self.pk_ordinals,
            self.txn,
            self.table_store,
            self.index_builder,
            self.params,
            self.limits,
            self.cancellation,
        )?;
        Ok(pks)
    }

    /// The first conflict of `proposed` with a row of the statement or of the
    /// transaction, checking the arbiters in order.
    fn find_hit(
        &self,
        pending: &Pending,
        arbiters: &[BoundArbiter],
        proposed: &[Option<RelationalValue>],
    ) -> Result<Option<Hit>> {
        for (ai, arbiter) in arbiters.iter().enumerate() {
            match arbiter.index_id {
                None => {
                    let pk = pk_values_of(proposed, self.pk_ordinals)?;
                    if pending.by_pk.contains_key(&pk_key(&pk)) {
                        return Ok(Some(Hit::Pending));
                    }
                    if let Some(row) = self.txn.get_row(self.table_id, &pk)? {
                        return Ok(Some(Hit::Committed { pk, row }));
                    }
                }
                Some(_) => {
                    let Some(key) = arbiter_key(proposed, &arbiter.ordinals) else {
                        continue;
                    };
                    if pending.by_arbiter[ai].contains_key(&key) {
                        return Ok(Some(Hit::Pending));
                    }
                    let vals: Vec<Option<RelationalValue>> = arbiter
                        .ordinals
                        .iter()
                        .map(|&o| proposed[o as usize].clone())
                        .collect();
                    for pk in self.pks_by_columns(&arbiter.ordinals, &vals)? {
                        if let Some(&i) = pending.by_pk.get(&pk_key(&pk)) {
                            // The statement already changed this row: it conflicts
                            // only if it still holds these values (a `Pending` hit
                            // through `by_arbiter` was handled above).
                            let still = arbiter_key(&pending.rows[i].row, &arbiter.ordinals);
                            if still.as_ref() == Some(&key) {
                                return Ok(Some(Hit::Pending));
                            }
                            continue;
                        }
                        if let Some(row) = self.txn.get_row(self.table_id, &pk)? {
                            return Ok(Some(Hit::Committed { pk, row }));
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}

/// A value that must be present was `NULL`: the statement is refused before it
/// writes anything.
fn check_not_null(columns: &[ColumnRow], row: &[Option<RelationalValue>]) -> Result<()> {
    for column in columns {
        if !column.nullable && row.get(column.ordinal as usize).is_none_or(Option::is_none) {
            return Err(SqlError::ExecutionParameter {
                detail: format!("column {:?} is NOT NULL", column.name),
            });
        }
    }
    Ok(())
}

/// Store assignment of one value of an `INSERT ... SELECT`: a lossless widening
/// (`exec::setop::widen_value`), or `BIGINT` into `INTEGER` when the value fits.
fn convert_for_insert(
    value: RelationalValue,
    to: rubixdb::relational::RelationalType,
) -> Result<RelationalValue> {
    use rubixdb::relational::RelationalType;
    match (&value, to) {
        (RelationalValue::Bigint(n), RelationalType::Integer) => i32::try_from(*n)
            .map(RelationalValue::Integer)
            .map_err(|_| SqlError::ExecutionParameter {
                detail: format!("the value {n} is out of range for an INTEGER column"),
            }),
        _ => crate::exec::setop::widen_value(value, to),
    }
}

/// The rows an `INSERT` proposes: its `VALUES` tuples evaluated, or the rows of its
/// source query (read in full, against the transaction's view, *before* any row is
/// written, so a statement that reads the table it inserts into sees the table as
/// it was). Bounded by `max_dml_target_rows`.
fn proposed_rows(
    insert: &BoundInsert,
    columns: &[ColumnRow],
    ec: &ExecCtx<'_>,
    limits: &ExecLimits,
    cancellation: &CancellationToken,
    at: Option<std::time::Instant>,
) -> Result<Vec<Row>> {
    let mut out: Vec<Row> = Vec::new();
    match &insert.select {
        None => {
            // `INSERT`'s `VALUES` never bind a column reference or a correlated
            // access, so an empty `RowContext` is always sufficient.
            let ctx = RowContext::new();
            for row in &insert.rows {
                check_point(cancellation, at)?;
                let values: Row = row
                    .iter()
                    .map(|expr| eval(expr, &ctx, ec))
                    .collect::<Result<_>>()?;
                out.push(values);
            }
        }
        Some(source) => {
            let plan = source.plan.as_ref().ok_or_else(|| SqlError::UnsupportedExecution {
                detail: "an INSERT ... SELECT reached execution without a physical plan (internal planner invariant violation)"
                    .to_string(),
            })?;
            let mut operator = build_operator(plan, &RowContext::new(), ec)?;
            loop {
                ec.check()?;
                check_point(cancellation, at)?;
                let Some(tuple) = operator.next(ec)? else {
                    break;
                };
                if out.len() >= limits.max_dml_target_rows {
                    return Err(SqlError::ResourceLimit {
                        detail: format!(
                            "INSERT ... SELECT produces more than max_dml_target_rows ({}) rows",
                            limits.max_dml_target_rows
                        ),
                    });
                }
                let mut row: Row = vec![None; columns.len()];
                for (target, value) in source.targets.iter().zip(tuple.projected) {
                    row[target.ordinal as usize] = match (value, target.convert_to) {
                        (Some(v), Some(to)) => Some(convert_for_insert(v, to)?),
                        (v, _) => v,
                    };
                }
                for (ordinal, default) in &source.defaults {
                    row[*ordinal as usize] = default.clone();
                }
                out.push(row);
            }
        }
    }
    for row in &out {
        check_not_null(columns, row)?;
    }
    Ok(out)
}

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
) -> Result<InsertOutcome> {
    // `Transaction::put_row` is a generic upsert-at-key primitive shared with
    // `UPDATE`: by itself it has no notion of "this key must not already exist",
    // and its commit-time validation only catches a *concurrently racing* write of
    // the same key, never a later, non-overlapping insert of a key some earlier
    // transaction used. `INSERT` is the one caller that needs "must be a new row",
    // so the existence check belongs here, against the transaction's own view
    // (`Transaction::get_row`: snapshot plus its earlier writes). The commit-time
    // freshness check stays the sole authority for the concurrent case.
    //
    // The statement is decided in two phases. Phase 1 reads: it evaluates the
    // proposed rows, finds each one's conflict (if any) and computes the row to
    // write and the `RETURNING` values -- against the transaction and against what
    // the statement itself has already decided -- and writes **nothing**. Phase 2
    // buffers the decided rows into the transaction's write-set. So a statement that
    // is going to fail on a duplicate key, a NOT NULL violation, a failing
    // `DO UPDATE` expression, an arithmetic overflow or a second hit on one row has
    // changed nothing even inside an explicit transaction; only a failure of the
    // buffering itself (a write-set limit, a row-shape error) can leave earlier rows
    // of the statement buffered there, and an autocommit statement drops its whole
    // transaction in every case.
    let table = catalog.get_table(insert.table_id)?.ok_or_else(|| {
        SqlError::Storage("a target table no longer resolves in the catalog".to_string())
    })?;
    let columns = catalog.get_columns(insert.table_id)?;
    let at = deadline_at(limits);
    let metrics = ExecMetrics::default();
    let arbiters: Vec<BoundArbiter> = match &insert.on_conflict {
        Some(oc) => oc.arbiters.clone(),
        None => vec![BoundArbiter {
            index_id: None,
            ordinals: table.pk_ordinals.clone(),
        }],
    };

    let (writes, outcome) = {
        let ec = ExecCtx::new(
            txn,
            table_store,
            index_builder,
            params,
            limits,
            &metrics,
            cancellation,
        );
        let proposed = proposed_rows(insert, &columns, &ec, limits, cancellation, at)?;
        let lookup = Lookup {
            txn,
            table_store,
            catalog,
            index_builder,
            params,
            limits,
            cancellation,
            table_id: insert.table_id,
            pk_ordinals: &table.pk_ordinals,
            columns: &columns,
        };
        decide_insert(
            insert.on_conflict.as_ref(),
            insert.returning.as_ref(),
            &lookup,
            &ec,
            proposed,
            &arbiters,
            &columns,
            limits,
            at,
        )?
    };

    // CHECK constraints: every row about to be written, before any is.
    check_rows(
        txn,
        table_store,
        catalog,
        index_builder,
        params,
        limits,
        cancellation,
        insert.table_id,
        writes.iter().filter_map(|c| c.new.as_ref()),
    )?;
    for change in &writes {
        check_point(cancellation, at)?;
        txn.put_row(
            insert.table_id,
            change.new.as_ref().expect("an insert writes a row"),
        )?;
    }
    // Foreign keys: every written row's parent must exist, and an upserted row
    // whose referenced columns changed is a change to a parent.
    let cascaded = txn.enforce_foreign_keys(insert.table_id, &writes)?;
    check_cascaded(
        txn,
        table_store,
        catalog,
        index_builder,
        params,
        limits,
        cancellation,
        &cascaded,
    )?;
    Ok(outcome)
}

/// Phase 1 of an `INSERT` (see `execute_insert`).
#[allow(clippy::too_many_arguments)]
fn decide_insert(
    on_conflict: Option<&BoundOnConflict>,
    returning: Option<&BoundReturning>,
    lookup: &Lookup<'_>,
    ec: &ExecCtx<'_>,
    proposed: Vec<Row>,
    arbiters: &[BoundArbiter],
    columns: &[ColumnRow],
    limits: &ExecLimits,
    at: Option<std::time::Instant>,
) -> Result<(Vec<RowChange>, InsertOutcome)> {
    let mut pending = Pending::new(arbiters);
    let mut sink = ReturningSink::new(returning);
    let (mut inserted, mut updated, mut skipped) = (0u64, 0u64, 0u64);

    for proposed_row in proposed {
        check_point(lookup.cancellation, at)?;
        let hit = lookup.find_hit(&pending, arbiters, &proposed_row)?;
        let Some(hit) = hit else {
            // No conflict: a new row.
            let pk = pk_values_of(&proposed_row, lookup.pk_ordinals)?;
            if let Some(sink) = sink.as_mut() {
                sink.push(&proposed_row, ec, limits)?;
            }
            pending.add(pk, proposed_row, None, true, arbiters);
            inserted += 1;
            continue;
        };
        let Some(oc) = on_conflict else {
            return Err(SqlError::Conflict {
                detail: "duplicate key value violates primary key constraint".to_string(),
            });
        };
        match &oc.action {
            BoundConflictAction::DoNothing => skipped += 1,
            BoundConflictAction::DoUpdate {
                assignments,
                selection,
            } => {
                let (pk, existing) = match hit {
                    Hit::Pending => {
                        // PostgreSQL's cardinality violation: the command may not
                        // change one row twice, or the result would depend on the
                        // order the rows were proposed in.
                        return Err(SqlError::ExecutionParameter {
                            detail: "ON CONFLICT DO UPDATE command cannot affect a row a second time: the statement proposes more than one row for the same key"
                                .to_string(),
                        });
                    }
                    Hit::Committed { pk, row } => (pk, row),
                };
                // `existing` is the table row under its own name, `EXCLUDED` the
                // proposed row; every SET expression sees the same pre-update pair.
                let ctx = RowContext::single(oc.existing_ref, existing.clone())
                    .with_row(oc.excluded_ref, proposed_row);
                if let Some(selection) = selection {
                    if !eval_predicate(selection, &ctx, ec)?.is_true() {
                        skipped += 1;
                        continue;
                    }
                }
                let mut new_row = existing.clone();
                for assignment in assignments {
                    new_row[assignment.ordinal as usize] = eval(&assignment.value, &ctx, ec)?;
                }
                check_not_null(columns, &new_row)?;
                if let Some(sink) = sink.as_mut() {
                    sink.push(&new_row, ec, limits)?;
                }
                let changed = new_row != existing;
                pending.add(pk, new_row, Some(existing), changed, arbiters);
                updated += 1;
            }
        }
    }

    let writes = pending
        .rows
        .into_iter()
        .filter(|p| p.write)
        .map(|p| RowChange {
            old: p.old,
            new: Some(p.row),
        })
        .collect();
    Ok((
        writes,
        InsertOutcome {
            inserted,
            updated,
            skipped,
            returning: sink.map(ReturningSink::finish),
        },
    ))
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
    returning: Option<&BoundReturning>,
    txn: &mut Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
) -> Result<(u64, Option<ReturnedRows>)> {
    let table = catalog.get_table(table_id)?.ok_or_else(|| {
        SqlError::Storage("a target table no longer resolves in the catalog".to_string())
    })?;
    let table_ref = access_table_ref(access);
    let mut sink = ReturningSink::new(returning);
    let (target_pks, usage) = find_target_pks(
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

    // SQL-mission-1 Slice B: a `SET` expression containing a subquery reads
    // table state, so it must see the table as it was BEFORE this statement
    // changed any row -- not the half-updated state a row-at-a-time write
    // loop would show it. Such statements evaluate every new row first and
    // write afterwards.
    if assignments
        .iter()
        .any(|a| crate::plan::expr_util::contains_subquery(&a.value))
    {
        return execute_update_with_subqueries(
            table_id,
            table_ref,
            assignments,
            &target_pks,
            usage,
            sink,
            txn,
            table_store,
            catalog,
            index_builder,
            params,
            limits,
            cancellation,
        );
    }

    let mut updated = 0u64;
    let mut changes: Vec<(Row, Row)> = Vec::new();
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
            // RETURNING reports the row as this statement leaves it.
            if let Some(sink) = sink.as_mut() {
                sink.push(&new_row, &ec, limits)?;
            }
        }
        // A `SET` of a primary-key column changes the row's key. The writes are
        // applied after every new row is computed (`Transaction::apply_row_updates`):
        // a key-preserving row is an upsert at its own key (old/new secondary-index
        // entries delete+insert atomically, D11), a key-changing row a delete of the
        // old key and an insert of the new one, with all old keys removed first so a
        // statement may shift keys among its own rows.
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
        changes.push((old_row_for_write(&row_ctx, table_ref), new_row));
        updated += 1;
    }
    check_rows(
        txn,
        table_store,
        catalog,
        index_builder,
        params,
        limits,
        cancellation,
        table_id,
        changes.iter().map(|(_, new)| new),
    )?;
    write_updates(
        txn,
        table_store,
        catalog,
        index_builder,
        params,
        limits,
        cancellation,
        table_id,
        changes,
    )?;
    Ok((updated, sink.map(ReturningSink::finish)))
}

/// The row an `UPDATE` started from, as the row context holds it.
fn old_row_for_write(row_ctx: &RowContext, table_ref: u32) -> Row {
    row_ctx
        .row_for(table_ref)
        .cloned()
        .expect("the statement's own row context holds its target row")
}

/// Buffers an `UPDATE`'s `(old, new)` rows and enforces the foreign keys they touch
/// (a changed referencing column needs its parent; a changed referenced column
/// applies `ON UPDATE` to the rows that reference it).
#[allow(clippy::too_many_arguments)]
fn write_updates(
    txn: &mut Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
    table_id: u32,
    changes: Vec<(Row, Row)>,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    txn.apply_row_updates(table_id, &changes)?;
    let row_changes: Vec<RowChange> = changes
        .into_iter()
        .map(|(old, new)| RowChange {
            old: Some(old),
            new: Some(new),
        })
        .collect();
    let cascaded = txn.enforce_foreign_keys(table_id, &row_changes)?;
    check_cascaded(
        txn,
        table_store,
        catalog,
        index_builder,
        params,
        limits,
        cancellation,
        &cascaded,
    )
}

// =======================================================================
// DELETE (items 14-16, 23)
// =======================================================================

#[allow(clippy::too_many_arguments)]
fn execute_delete(
    table_id: u32,
    access: &PhysicalAccess,
    returning: Option<&BoundReturning>,
    txn: &mut Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
) -> Result<(u64, Option<ReturnedRows>)> {
    let table = catalog.get_table(table_id)?.ok_or_else(|| {
        SqlError::Storage("a target table no longer resolves in the catalog".to_string())
    })?;
    let (target_pks, _usage) = find_target_pks(
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

    // RETURNING reports the rows as they were when this statement deleted them:
    // read (and evaluated) first, in one pass over the transaction's own view,
    // so nothing is written if a RETURNING expression fails.
    let mut sink = ReturningSink::new(returning);
    if let Some(sink) = sink.as_mut() {
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
        for pk in &target_pks {
            check_point(cancellation, at)?;
            let row = txn.get_row(table_id, pk)?.ok_or_else(|| {
                SqlError::Storage(
                    "a target row disappeared between being matched and being deleted".to_string(),
                )
            })?;
            sink.push(&row, &ec, limits)?;
        }
    }

    // A table another table references needs the deleted rows' values to apply
    // `ON DELETE` (read before the deletes, from the transaction's own view).
    let needs_rows = txn.table_is_referenced(table_id)?;
    let mut deleted_rows: Vec<RowChange> = Vec::new();
    let mut deleted = 0u64;
    for pk in target_pks {
        check_point(cancellation, at)?;
        if needs_rows {
            if let Some(row) = txn.get_row(table_id, &pk)? {
                deleted_rows.push(RowChange {
                    old: Some(row),
                    new: None,
                });
            }
        }
        // Item 14: the row's own current existence is authoritative
        // through `delete_row` itself (D10-certified); the already-collected
        // primary key is all a `DELETE` needs.
        txn.delete_row(table_id, &pk)?;
        deleted += 1;
    }
    if needs_rows {
        let cascaded = txn.enforce_foreign_keys(table_id, &deleted_rows)?;
        check_cascaded(
            txn,
            table_store,
            catalog,
            index_builder,
            params,
            limits,
            cancellation,
            &cascaded,
        )?;
    }
    Ok((deleted, sink.map(ReturningSink::finish)))
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
) -> Result<(Vec<Vec<RelationalValue>>, CorrelatedUsage)> {
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
    Ok((out, ec.correlated_usage()))
}

/// `UPDATE` whose `SET` contains a subquery (SQL-mission-1 Slice B).
///
/// Phase 1 evaluates every matched row's new value against the unmodified
/// table (one shared `ExecCtx`, so the subquery limits and the uncorrelated-
/// result cache span the whole statement, and the `WHERE` phase's correlated
/// usage carries over); phase 2 writes. Every `SET` expression -- and any
/// correlated subquery inside it -- sees the matched row's ORIGINAL values,
/// and no row's update can change what another row's subquery reads. The
/// pending rows are bounded by `max_dml_target_rows` (already enforced when
/// the targets were collected) and by `max_materialized_bytes`.
#[allow(clippy::too_many_arguments)]
fn execute_update_with_subqueries(
    table_id: u32,
    table_ref: u32,
    assignments: &[BoundAssignment],
    target_pks: &[Vec<RelationalValue>],
    usage: CorrelatedUsage,
    mut sink: Option<ReturningSink<'_>>,
    txn: &mut Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
) -> Result<(u64, Option<ReturnedRows>)> {
    let at = deadline_at(limits);
    let metrics = ExecMetrics::default();
    let mut pending: Vec<(Row, Row)> = Vec::new();
    let mut pending_bytes = 0usize;
    let mut updated = 0u64;
    {
        let ec = ExecCtx::new(
            txn,
            table_store,
            index_builder,
            params,
            limits,
            &metrics,
            cancellation,
        )
        .with_correlated_usage(usage);
        for pk in target_pks {
            check_point(cancellation, at)?;
            let old_row = txn.get_row(table_id, pk)?.ok_or_else(|| {
                SqlError::Storage(
                    "a target row disappeared between being matched and being updated".to_string(),
                )
            })?;
            let row_ctx = RowContext::single(table_ref, old_row.clone());
            let mut new_row = old_row.clone();
            for assignment in assignments {
                new_row[assignment.ordinal as usize] = eval(&assignment.value, &row_ctx, &ec)?;
            }
            if let Some(sink) = sink.as_mut() {
                sink.push(&new_row, &ec, limits)?;
            }
            updated += 1;
            if row_ctx.row_for(table_ref) == Some(&new_row) {
                continue;
            }
            pending_bytes = pending_bytes.saturating_add(
                crate::aggregate::GroupingKey::from_values(&new_row).estimated_bytes(),
            );
            if pending_bytes > limits.max_materialized_bytes {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "an UPDATE with a subquery in SET buffered more than max_materialized_bytes ({}) of new rows",
                        limits.max_materialized_bytes
                    ),
                });
            }
            pending.push((old_row, new_row));
        }
    }
    check_point(cancellation, at)?;
    check_rows(
        txn,
        table_store,
        catalog,
        index_builder,
        params,
        limits,
        cancellation,
        table_id,
        pending.iter().map(|(_, new)| new),
    )?;
    write_updates(
        txn,
        table_store,
        catalog,
        index_builder,
        params,
        limits,
        cancellation,
        table_id,
        pending,
    )?;
    Ok((updated, sink.map(ReturningSink::finish)))
}

fn access_table_ref(access: &PhysicalAccess) -> u32 {
    match access {
        PhysicalAccess::PkLookup { table_ref, .. }
        | PhysicalAccess::IndexScan { table_ref, .. }
        | PhysicalAccess::SeqScan { table_ref, .. }
        | PhysicalAccess::PkRangeScan { table_ref, .. } => *table_ref,
    }
}

// =======================================================================
// DDL (items 35-45, 96)
// =======================================================================

#[allow(clippy::too_many_arguments)]
fn execute_ddl(
    bound: &BoundStatement,
    txn: &Transaction,
    table_store: &TableStore,
    catalog: &CatalogService,
    index_builder: &IndexBuilder,
    params: &[Option<RelationalValue>],
    limits: &ExecLimits,
    cancellation: &CancellationToken,
) -> Result<WriteResult> {
    let n = match bound {
        BoundStatement::CreateSchema(cs) => match catalog.create_schema(cs.database_id, &cs.name) {
            Ok(_) => 1,
            Err(CatalogError::AlreadyExists { .. }) if cs.if_not_exists => 0,
            Err(e) => return Err(e.into()),
        },
        BoundStatement::CreateTable(ct) => {
            let columns: Vec<ColumnDef> = ct.columns.iter().map(bound_column_to_catalog).collect();
            let foreign_keys: Vec<NewTableForeignKey> = ct
                .foreign_keys
                .iter()
                .map(|fk| NewTableForeignKey {
                    name: fk.name.clone(),
                    child_ordinals: fk.child_ordinals.clone(),
                    parent_table_id: fk.parent_table_id,
                    parent_ordinals: fk.parent_ordinals.clone(),
                    on_delete: fk.on_delete,
                    on_update: fk.on_update,
                })
                .collect();
            let checks: Vec<NewTableCheck> = ct
                .checks
                .iter()
                .map(|c| NewTableCheck {
                    name: c.name.clone(),
                    column_ordinals: c.columns.clone(),
                    expression: c.text.clone(),
                })
                .collect();
            match catalog.create_table_with_foreign_keys(
                ct.schema_id,
                &ct.name,
                &columns,
                &ct.pk_ordinals,
                &foreign_keys,
                &checks,
            ) {
                Ok(_) => 1,
                Err(CatalogError::AlreadyExists { .. }) if ct.if_not_exists => 0,
                Err(e) => return Err(ddl_catalog_error(e)),
            }
        }
        BoundStatement::DropTable(dt) => match dt.table_id {
            // `None` only when `IF EXISTS` was given and the table
            // genuinely does not exist -- already resolved at bind
            // time (`BoundDropTable::table_id`'s own doc comment); a
            // real no-op, not an error.
            None => 0,
            Some(table_id) => {
                catalog.drop_table(table_id).map_err(ddl_catalog_error)?;
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
                index_builder
                    .drop_index_online(index_id)
                    .map_err(ddl_relational_error)?;
                1
            }
        },
        BoundStatement::AlterTable(at) => match (at.table_id, &at.operation) {
            (None, _) | (_, BoundAlterOp::Noop) => 0,
            (Some(table_id), op) => {
                match op {
                    BoundAlterOp::Noop => {}
                    BoundAlterOp::AddColumn(column) => {
                        index_builder
                            .alter_add_column(table_id, &bound_column_to_catalog(column))
                            .map_err(ddl_relational_error)?;
                    }
                    BoundAlterOp::RenameColumn { ordinal, new_name } => index_builder
                        .alter_rename_column(table_id, *ordinal, new_name)
                        .map_err(ddl_relational_error)?,
                    BoundAlterOp::SetDefault {
                        ordinal,
                        default_value,
                    } => index_builder
                        .alter_set_default(table_id, *ordinal, Some(default_value.clone()))
                        .map_err(ddl_relational_error)?,
                    BoundAlterOp::DropDefault { ordinal } => index_builder
                        .alter_set_default(table_id, *ordinal, None)
                        .map_err(ddl_relational_error)?,
                    BoundAlterOp::SetNotNull { ordinal } => index_builder
                        .alter_set_not_null(table_id, *ordinal)
                        .map_err(ddl_relational_error)?,
                    BoundAlterOp::DropNotNull { ordinal } => index_builder
                        .alter_drop_not_null(table_id, *ordinal)
                        .map_err(ddl_relational_error)?,
                    BoundAlterOp::AddForeignKey(fk) => {
                        index_builder
                            .alter_add_foreign_key(fk)
                            .map_err(ddl_relational_error)?;
                    }
                    BoundAlterOp::AddUnique {
                        name,
                        column_ordinals,
                    } => {
                        index_builder
                            .alter_add_unique(table_id, name, column_ordinals)
                            .map_err(ddl_relational_error)?;
                    }
                    BoundAlterOp::AddCheck(def) => {
                        // Bind the stored text against the table as it is now, then
                        // evaluate it for every existing row under the table's lock.
                        let table = catalog.get_table(table_id)?.ok_or_else(|| {
                            SqlError::Storage("a target table no longer resolves in the catalog".to_string())
                        })?;
                        let columns = catalog.get_columns(table_id)?;
                        let sql_limits = SqlLimits::default();
                        let expr = check::parse_check_text(&def.text, &sql_limits)?;
                        let bound = check::bind_check(&table.name, &columns, &expr, &sql_limits)?;
                        let compiled = [CompiledCheck {
                            name: def.name.clone(),
                            expr: bound.expr,
                            table_ref: bound.table_ref,
                        }];
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
                        index_builder
                            .alter_add_check(
                                table_id,
                                &def.name,
                                &def.columns,
                                &def.text,
                                &mut |row| {
                                    check::first_violation(&compiled, row, &ec)
                                        .map(|v| v.is_none())
                                        .map_err(|e| RelationalError::InvalidInput {
                                            detail: format!("the constraint could not be evaluated: {e}"),
                                        })
                                },
                            )
                            .map_err(ddl_relational_error)?;
                    }
                    BoundAlterOp::DropCheck { constraint_id } => index_builder
                        .alter_drop_check(*constraint_id)
                        .map_err(ddl_relational_error)?,
                    BoundAlterOp::DropForeignKey { fk_id } => index_builder
                        .alter_drop_foreign_key(*fk_id)
                        .map_err(ddl_relational_error)?,
                    BoundAlterOp::DropUnique { index_id } => index_builder
                        .drop_index_online(*index_id)
                        .map_err(ddl_relational_error)?,
                }
                1
            }
        },
        // v1 is one database per instance (D1: bootstrap creates "default" and nothing else does), and
        // there is deliberately no `CatalogService::create_database` primitive. `CREATE DATABASE` is
        // therefore refused for every name, with the way forward spelled out, rather than faked.
        BoundStatement::CreateDatabase(_) => {
            return Err(SqlError::UnsupportedExecution {
                detail: "CREATE DATABASE is disabled. rubiXDb v1 runs on a single database ('default'). Use CREATE SCHEMA <schema_name> to organize your tables."
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
        returning: None,
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
        BoundStatement::AlterTable(_) => "ALTER TABLE",
        BoundStatement::Explain(_) => "EXPLAIN",
        BoundStatement::Begin => "BEGIN",
        BoundStatement::Commit => "COMMIT",
        BoundStatement::Rollback => "ROLLBACK",
    }
}

use crate::error::{ddl_catalog_error, ddl_relational_error};

fn bound_column_to_catalog(c: &BoundColumnDef) -> ColumnDef {
    ColumnDef {
        name: c.name.clone(),
        data_type: c.data_type,
        nullable: c.nullable,
        default_value: c.default_value.clone(),
        type_params: c.type_params.clone(),
    }
}
