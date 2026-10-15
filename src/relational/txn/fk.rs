//! Foreign-key enforcement for a transaction -- `docs/adr/ADR-DDL-FK-01.md`.
//!
//! Two enforcement points, one rule set:
//!
//! * **Statement time** -- `Transaction::enforce_foreign_keys`, called by the
//!   write executor right after it buffers a statement's row changes. It reads the
//!   transaction's own view (its snapshot plus its buffered writes), so a
//!   violation is reported by the statement that causes it ("immediate"
//!   enforcement), and it carries out `ON DELETE` / `ON UPDATE` actions by
//!   buffering the cascaded changes into the same transaction.
//! * **Commit time** -- `Transaction::validate_foreign_keys`, called from `commit`
//!   with the affected tables' epoch locks held. It re-checks the *whole*
//!   write-set against the **latest committed state** plus the write-set. This is
//!   what makes enforcement correct under concurrency: the statement-time check
//!   looked at a snapshot, and a transaction that committed since may have
//!   deleted a parent or added a child. Holding the locks, no other commit can
//!   change either table while the check and the `write_batch` run.
//!
//! A row whose referencing columns include a `NULL` references nothing and is
//! never checked (`MATCH SIMPLE`). Error messages name the constraint, tables and
//! columns -- never a row value.

use std::collections::{HashMap, HashSet};

use super::{RowOp, Transaction};
use crate::catalog::schema::{ColumnRow, FkAction, ForeignKeyRow};
use crate::relational::error::{RelationalError, Result};
use crate::relational::key::encode_composite_key;
use crate::relational::lookup::{row_matches, Access};
use crate::relational::table_store::{self, Row};
use crate::relational::value::RelationalValue;

/// Cascades nest (a deleted child can itself be a referenced parent). Beyond this
/// depth the statement fails closed rather than recursing without bound.
pub const MAX_CASCADE_DEPTH: usize = 32;

/// Which committed state a lookup reads (the transaction's own writes are always
/// layered on top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadView {
    /// The state at the transaction's `BEGIN` -- what its statements see.
    Snapshot,
    /// The state committed right now -- what commit-time validation must use.
    Latest,
}

/// A row the enforcement itself wrote (a cascaded `SET NULL`, `SET DEFAULT` or key
/// update of a child row). The caller checks it against constraints that only it can
/// evaluate (`CHECK`).
#[derive(Debug, Clone, PartialEq)]
pub struct CascadedWrite {
    pub table_id: u32,
    pub row: Row,
}

/// One row a statement changed: `old == None` is an insert, `new == None` a delete.
#[derive(Debug, Clone, PartialEq)]
pub struct RowChange {
    pub old: Option<Row>,
    pub new: Option<Row>,
}

type Tuple = Vec<RelationalValue>;

/// What a parent key became: `None` = its row is gone; `Some(values)` = the row's new
/// values for the referenced columns (any of which may be `NULL`).
type NewKey = Option<Vec<Option<RelationalValue>>>;

/// The values of `ordinals` in `row`, or `None` if any is `NULL`.
fn tuple_of(row: &Row, ordinals: &[u16]) -> Option<Tuple> {
    ordinals
        .iter()
        .map(|&o| row.get(o as usize).cloned().flatten())
        .collect()
}

/// A foreign key with the access paths its two lookups use, resolved once.
struct FkPlan<'a> {
    fk: &'a ForeignKeyRow,
    /// Finds the referenced (parent) row for a child's values.
    parent_access: Access,
    /// Finds the referencing (child) rows for a parent's values.
    child_access: Access,
}

impl Transaction {
    fn plan_fk<'a>(&self, fk: &'a ForeignKeyRow) -> Result<FkPlan<'a>> {
        let store = &self.inner.table_store;
        let (parent, _) = store.resolve_table(fk.parent_table_id)?;
        let (child, _) = store.resolve_table(fk.table_id)?;
        Ok(FkPlan {
            fk,
            parent_access: store.plan_access(&parent, &fk.parent_ordinals)?,
            child_access: store.plan_access(&child, &fk.child_ordinals)?,
        })
    }

    fn seq_of(&self, view: ReadView) -> u64 {
        match view {
            ReadView::Snapshot => self.snapshot.seq(),
            ReadView::Latest => u64::MAX,
        }
    }

    /// Up to `limit` rows of `table_id` holding `values` in `ordinals`, as the
    /// transaction sees the table under `view`: the committed rows (at the view's
    /// sequence) except those the transaction has itself rewritten or deleted,
    /// plus the transaction's own written rows that match.
    fn find_rows_in_view(
        &self,
        table_id: u32,
        ordinals: &[u16],
        values: &[RelationalValue],
        access: &Access,
        view: ReadView,
        limit: usize,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        let own = self.writes.get(&table_id).filter(|w| !w.is_empty());
        // With local writes in play, committed rows they hide must not use up the limit:
        // at most `own.len()` base rows can be hidden, so that many extra are read.
        let base_limit = limit.saturating_add(own.map_or(0, |w| w.len()));
        let mut found = self.inner.table_store.find_rows_with_access(
            table_id,
            ordinals,
            values,
            access,
            self.seq_of(view),
            base_limit,
        )?;
        if let Some(own) = own {
            let mut keep = Vec::with_capacity(found.len());
            for (pk, row) in found {
                if !own.contains_key(&encode_composite_key(&pk)?) {
                    keep.push((pk, row));
                }
            }
            found = keep;
            for (pk_values, op) in own.values() {
                if let RowOp::Put(row) = op {
                    if row_matches(row, ordinals, values) {
                        found.push((pk_values.clone(), row.clone()));
                    }
                }
            }
            found.truncate(limit);
        }
        Ok(found)
    }

    /// Does the referenced row for `key` (a child's referencing values) exist?
    fn referenced_exists(
        &self,
        plan: &FkPlan<'_>,
        key: &[RelationalValue],
        view: ReadView,
    ) -> Result<bool> {
        Ok(!self
            .find_rows_in_view(
                plan.fk.parent_table_id,
                &plan.fk.parent_ordinals,
                key,
                &plan.parent_access,
                view,
                1,
            )?
            .is_empty())
    }

    /// Up to `limit` child rows referencing any of `tuples` (parent-order values),
    /// under `view`. With a narrow access path each tuple is looked up; without
    /// one the child table is read once for all of them.
    fn children_of(
        &self,
        plan: &FkPlan<'_>,
        tuples: &[Tuple],
        view: ReadView,
        limit: usize,
    ) -> Result<Vec<(Vec<RelationalValue>, Row)>> {
        let fk = plan.fk;
        let mut out = Vec::new();
        if plan.child_access.is_narrow() || tuples.len() <= 1 {
            for t in tuples {
                let rows = self.find_rows_in_view(
                    fk.table_id,
                    &fk.child_ordinals,
                    t,
                    &plan.child_access,
                    view,
                    limit - out.len(),
                )?;
                out.extend(rows);
                if out.len() >= limit {
                    break;
                }
            }
            return Ok(out);
        }
        let mut wanted: HashSet<Vec<u8>> = HashSet::with_capacity(tuples.len());
        for t in tuples {
            wanted.insert(encode_composite_key(t)?);
        }
        let own = self.writes.get(&fk.table_id).filter(|w| !w.is_empty());
        self.inner
            .table_store
            .for_each_row_as_of(fk.table_id, self.seq_of(view), |pk, row| {
                if own
                    .is_some_and(|w| w.contains_key(&encode_composite_key(pk).unwrap_or_default()))
                {
                    return Ok(true);
                }
                if let Some(t) = tuple_of(row, &fk.child_ordinals) {
                    if wanted.contains(&encode_composite_key(&t)?) {
                        out.push((pk.to_vec(), row.clone()));
                    }
                }
                Ok(out.len() < limit)
            })?;
        if out.len() < limit {
            if let Some(own) = own {
                for (pk_values, op) in own.values() {
                    if let RowOp::Put(row) = op {
                        if let Some(t) = tuple_of(row, &fk.child_ordinals) {
                            if wanted.contains(&encode_composite_key(&t)?) {
                                out.push((pk_values.clone(), row.clone()));
                            }
                        }
                    }
                }
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    fn table_name(&self, table_id: u32) -> String {
        self.inner
            .table_store
            .catalog()
            .get_table(table_id)
            .ok()
            .flatten()
            .map(|t| t.name)
            .unwrap_or_else(|| format!("#{table_id}"))
    }

    fn column_names(&self, table_id: u32, ordinals: &[u16]) -> String {
        let columns = self
            .inner
            .table_store
            .catalog()
            .get_columns(table_id)
            .unwrap_or_default();
        ordinals
            .iter()
            .map(|o| {
                columns
                    .iter()
                    .find(|c| c.ordinal == *o)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| format!("#{o}"))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A row of the child table has no referenced row.
    fn child_violation(&self, fk: &ForeignKeyRow) -> RelationalError {
        RelationalError::ForeignKeyViolation {
            detail: format!(
                "insert or update on table \"{}\" violates foreign key constraint \"{}\": ({}) does not match a row of table \"{}\" ({})",
                self.table_name(fk.table_id),
                fk.name,
                self.column_names(fk.table_id, &fk.child_ordinals),
                self.table_name(fk.parent_table_id),
                self.column_names(fk.parent_table_id, &fk.parent_ordinals),
            ),
        }
    }

    /// A row of the parent table that is still referenced would go away.
    fn parent_violation(&self, fk: &ForeignKeyRow) -> RelationalError {
        RelationalError::ForeignKeyViolation {
            detail: format!(
                "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\": the row is still referenced by ({})",
                self.table_name(fk.parent_table_id),
                fk.name,
                self.table_name(fk.table_id),
                self.column_names(fk.table_id, &fk.child_ordinals),
            ),
        }
    }

    // ---------------------------------------------------------------
    // Commit-time validation
    // ---------------------------------------------------------------

    /// Re-checks every foreign key the write-set touches against the latest
    /// committed state plus the write-set. Called from `commit` with the child and
    /// parent tables locked (see the module documentation).
    pub(super) fn validate_foreign_keys(&self) -> Result<()> {
        let fks = self.inner.table_store.catalog().foreign_keys()?;
        for fk in fks.iter() {
            let child_writes = self.writes.get(&fk.table_id);
            let parent_writes = self.writes.get(&fk.parent_table_id);
            if child_writes.is_none() && parent_writes.is_none() {
                continue;
            }
            let plan = self.plan_fk(fk)?;

            // Referencing side: each written child row must have its parent.
            if let Some(child_writes) = child_writes {
                for (_, op) in child_writes.values() {
                    let RowOp::Put(row) = op else { continue };
                    let Some(key) = tuple_of(row, &fk.child_ordinals) else {
                        continue;
                    };
                    if !self.referenced_exists(&plan, &key, ReadView::Latest)? {
                        return Err(self.child_violation(fk));
                    }
                }
            }

            // Referenced side: a parent key that a written row vacated (changed or
            // deleted) must not be referenced by any child that survives this commit.
            if let Some(parent_writes) = parent_writes {
                let mut vacated: HashMap<Vec<u8>, Tuple> = HashMap::new();
                for (pk_values, op) in parent_writes.values() {
                    // The freshness check passed, so the value at the snapshot is the
                    // value committed now.
                    let Some(old_row) = self.get_committed_row(fk.parent_table_id, pk_values)?
                    else {
                        continue;
                    };
                    let Some(old_key) = tuple_of(&old_row, &fk.parent_ordinals) else {
                        continue;
                    };
                    let new_key = match op {
                        RowOp::Put(row) => tuple_of(row, &fk.parent_ordinals),
                        RowOp::Delete => None,
                    };
                    if new_key.as_ref() != Some(&old_key) {
                        vacated.insert(encode_composite_key(&old_key)?, old_key);
                    }
                }
                let mut gone: Vec<Tuple> = Vec::new();
                for key in vacated.into_values() {
                    // Another row may provide the same key (a delete and re-insert).
                    if !self.referenced_exists(&plan, &key, ReadView::Latest)? {
                        gone.push(key);
                    }
                }
                if !gone.is_empty()
                    && !self
                        .children_of(&plan, &gone, ReadView::Latest, 1)?
                        .is_empty()
                {
                    return Err(self.parent_violation(fk));
                }
            }
        }
        Ok(())
    }

    /// The row at `pk_values` in the engine at this transaction's snapshot, ignoring
    /// the transaction's own writes.
    fn get_committed_row(
        &self,
        table_id: u32,
        pk_values: &[RelationalValue],
    ) -> Result<Option<Row>> {
        let (table, columns) = self.inner.table_store.resolve_table(table_id)?;
        let encoded = encode_composite_key(pk_values)?;
        self.inner.table_store.fetch_row_by_encoded_pk(
            &table,
            &columns,
            pk_values,
            &encoded,
            self.snapshot.seq(),
        )
    }

    // ---------------------------------------------------------------
    // Statement-time enforcement and cascades
    // ---------------------------------------------------------------

    /// Enforces every foreign key that `changes` -- row changes to `table_id`
    /// the caller has **already buffered** -- touch, and carries out the
    /// referential actions: a cascade is buffered into this transaction, a
    /// refusal is an error. On `Err` the transaction may hold part of the
    /// statement's writes; callers wrap a statement in `savepoint` / `rollback_to`.
    ///
    /// Returns the child rows it wrote by cascading an update, for the caller to check
    /// against constraints only it can evaluate.
    pub fn enforce_foreign_keys(
        &mut self,
        table_id: u32,
        changes: &[RowChange],
    ) -> Result<Vec<CascadedWrite>> {
        self.require_active()?;
        let fks = self.inner.table_store.catalog().foreign_keys()?;
        let mut cascaded = Vec::new();
        if fks.is_empty() {
            return Ok(cascaded);
        }
        self.enforce_inner(&fks, table_id, changes, 0, &mut cascaded)?;
        Ok(cascaded)
    }

    fn enforce_inner(
        &mut self,
        fks: &[ForeignKeyRow],
        table_id: u32,
        changes: &[RowChange],
        depth: usize,
        cascaded: &mut Vec<CascadedWrite>,
    ) -> Result<()> {
        if depth > MAX_CASCADE_DEPTH {
            return Err(RelationalError::ResourceLimit {
                detail: format!("foreign-key cascade is nested deeper than {MAX_CASCADE_DEPTH}"),
            });
        }

        // 1. Referencing side: an inserted row, or an updated row whose referencing
        // columns changed, must have its parent.
        for fk in fks.iter().filter(|f| f.table_id == table_id) {
            let plan = self.plan_fk(fk)?;
            for change in changes {
                let Some(new) = &change.new else { continue };
                let Some(key) = tuple_of(new, &fk.child_ordinals) else {
                    continue;
                };
                if let Some(old) = &change.old {
                    if tuple_of(old, &fk.child_ordinals).as_ref() == Some(&key) {
                        continue;
                    }
                }
                // The parent must exist in what the transaction sees. A parent that was
                // committed after this transaction began is not in its snapshot but is
                // there for anyone to reference (as with PostgreSQL's referential checks,
                // which read the latest state), so it is accepted here and left to the
                // commit-time validation, which is the authority.
                if !self.referenced_exists(&plan, &key, ReadView::Snapshot)?
                    && !self.referenced_exists(&plan, &key, ReadView::Latest)?
                {
                    return Err(self.child_violation(fk));
                }
            }
        }

        // 2. Referenced side: a deleted row, or an updated row whose referenced
        // columns changed, vacates its old key.
        for fk in fks.iter().filter(|f| f.parent_table_id == table_id) {
            // old key -> the key it became (None = the row is gone)
            let mut moved: Vec<(Tuple, NewKey)> = Vec::new();
            for change in changes {
                let Some(old) = &change.old else { continue };
                let Some(old_key) = tuple_of(old, &fk.parent_ordinals) else {
                    continue;
                };
                match &change.new {
                    None => moved.push((old_key, None)),
                    Some(new) => {
                        let new_values: Vec<Option<RelationalValue>> = fk
                            .parent_ordinals
                            .iter()
                            .map(|&o| new.get(o as usize).cloned().flatten())
                            .collect();
                        if new_values.iter().all(Option::is_some)
                            && new_values.iter().flatten().cloned().collect::<Vec<_>>() == old_key
                        {
                            continue;
                        }
                        moved.push((old_key, Some(new_values)));
                    }
                }
            }
            if moved.is_empty() {
                continue;
            }
            let plan = self.plan_fk(fk)?;
            let old_keys: Vec<Tuple> = moved.iter().map(|(k, _)| k.clone()).collect();
            let limit = self.inner.limits.max_write_set_ops.saturating_add(1);
            let children = self.children_of(&plan, &old_keys, ReadView::Snapshot, limit)?;
            if children.is_empty() {
                continue;
            }
            // A deleted key that another row now provides keeps its children.
            // (Updated keys are handled per action below.)
            let mut provided: HashSet<Vec<u8>> = HashSet::new();
            for (key, new_values) in &moved {
                if new_values.is_none() && self.referenced_exists(&plan, key, ReadView::Snapshot)? {
                    provided.insert(encode_composite_key(key)?);
                }
            }
            let new_for: HashMap<Vec<u8>, (bool, &NewKey)> = moved
                .iter()
                .map(|(k, n)| Ok((encode_composite_key(k)?, (n.is_none(), n))))
                .collect::<Result<_>>()?;

            let columns = self.inner.table_store.catalog().get_columns(fk.table_id)?;
            let mut deletes: Vec<(Vec<RelationalValue>, Row)> = Vec::new();
            let mut updates: Vec<(Row, Row)> = Vec::new();
            for (child_pk, child_row) in children {
                let Some(child_key) = tuple_of(&child_row, &fk.child_ordinals) else {
                    continue;
                };
                let enc = encode_composite_key(&child_key)?;
                if provided.contains(&enc) {
                    continue;
                }
                let Some(&(is_delete, new_values)) = new_for.get(&enc) else {
                    continue;
                };
                let action = if is_delete {
                    fk.on_delete
                } else {
                    fk.on_update
                };
                match action {
                    FkAction::NoAction | FkAction::Restrict => {
                        // An updated key that another row now provides is fine.
                        if !is_delete
                            && self.referenced_exists(&plan, &child_key, ReadView::Snapshot)?
                        {
                            continue;
                        }
                        return Err(self.parent_violation(fk));
                    }
                    FkAction::Cascade if is_delete => deletes.push((child_pk, child_row)),
                    FkAction::Cascade => {
                        let new_values = new_values.as_ref().expect("an update has new values");
                        updates.push((
                            child_row.clone(),
                            with_columns(&child_row, &fk.child_ordinals, new_values),
                        ));
                    }
                    FkAction::SetNull => {
                        let nulls = vec![None; fk.child_ordinals.len()];
                        updates.push((
                            child_row.clone(),
                            with_columns(&child_row, &fk.child_ordinals, &nulls),
                        ));
                    }
                    FkAction::SetDefault => {
                        let defaults = self.defaults_of(&columns, &fk.child_ordinals)?;
                        updates.push((
                            child_row.clone(),
                            with_columns(&child_row, &fk.child_ordinals, &defaults),
                        ));
                    }
                }
            }

            let mut child_changes: Vec<RowChange> = Vec::new();
            for (pk, row) in deletes {
                self.delete_row(fk.table_id, &pk)?;
                child_changes.push(RowChange {
                    old: Some(row),
                    new: None,
                });
            }
            updates.retain(|(old, new)| old != new);
            if !updates.is_empty() {
                self.apply_row_updates(fk.table_id, &updates)?;
                for (old, new) in updates {
                    cascaded.push(CascadedWrite {
                        table_id: fk.table_id,
                        row: new.clone(),
                    });
                    child_changes.push(RowChange {
                        old: Some(old),
                        new: Some(new),
                    });
                }
            }
            if !child_changes.is_empty() {
                // The cascaded changes may themselves reference or be referenced.
                self.enforce_inner(fks, fk.table_id, &child_changes, depth + 1, cascaded)?;
            }
        }
        Ok(())
    }

    fn defaults_of(
        &self,
        columns: &[ColumnRow],
        ordinals: &[u16],
    ) -> Result<Vec<Option<RelationalValue>>> {
        ordinals
            .iter()
            .map(|&o| {
                let column = columns.iter().find(|c| c.ordinal == o).ok_or_else(|| {
                    RelationalError::InvalidInput {
                        detail: format!("column {o} no longer exists"),
                    }
                })?;
                match &column.default_value {
                    None => Ok(None),
                    Some(bytes) => table_store::decode_literal(column, bytes),
                }
            })
            .collect()
    }

    /// Rewrites rows of `table_id`, `(old, new)` pairs, buffering the result --
    /// including a change of primary key, which is a delete of the old key and an
    /// insert of the new one (refused if the new key is taken). All the
    /// primary-key-changing rows are removed before any is inserted, so a statement
    /// that shifts keys among themselves (`SET id = id + 1`) works.
    pub fn apply_row_updates(&mut self, table_id: u32, updates: &[(Row, Row)]) -> Result<()> {
        self.require_active()?;
        let (table, columns) = self.inner.table_store.resolve_table(table_id)?;
        let mut moved: Vec<(Vec<RelationalValue>, &Row)> = Vec::new();
        for (old, new) in updates {
            table_store::validate_row_shape(&table, &columns, new)?;
            let old_pk = table_store::extract_pk_values(&table, old);
            let new_pk = table_store::extract_pk_values(&table, new);
            if old_pk == new_pk {
                self.put_row(table_id, new)?;
            } else {
                moved.push((old_pk, new));
            }
        }
        for (old_pk, _) in &moved {
            self.delete_row(table_id, old_pk)?;
        }
        for (_, new) in &moved {
            let new_pk = table_store::extract_pk_values(&table, new);
            if self.get_row(table_id, &new_pk)?.is_some() {
                return Err(RelationalError::Conflict {
                    detail: "duplicate key value violates primary key constraint".to_string(),
                });
            }
            self.put_row(table_id, new)?;
        }
        Ok(())
    }

    /// Does another row's foreign key reference `table_id` (is it a parent)? A
    /// `DELETE` needs the deleted rows' values only then.
    pub fn table_is_referenced(&self, table_id: u32) -> Result<bool> {
        Ok(self
            .inner
            .table_store
            .catalog()
            .foreign_keys()?
            .iter()
            .any(|fk| fk.parent_table_id == table_id))
    }

    /// Does `table_id` take part in any foreign key (as child or parent)?
    pub fn table_has_foreign_keys(&self, table_id: u32) -> Result<bool> {
        Ok(self
            .inner
            .table_store
            .catalog()
            .foreign_keys()?
            .iter()
            .any(|fk| fk.table_id == table_id || fk.parent_table_id == table_id))
    }
}

/// `row` with `ordinals` set to `values` pairwise.
fn with_columns(row: &Row, ordinals: &[u16], values: &[Option<RelationalValue>]) -> Row {
    let mut out = row.clone();
    for (&o, v) in ordinals.iter().zip(values) {
        out[o as usize] = v.clone();
    }
    out
}
