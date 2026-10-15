//! `ALTER TABLE` execution -- `docs/adr/ADR-DDL-FK-01.md`.
//!
//! Every operation here follows one protocol:
//!
//! 1. take the epoch **write**-lock of each table involved, in sorted order (the
//!    lock every commit takes, so no transaction commits to those tables while
//!    the operation runs and every commit that ran before it is fully visible);
//! 2. validate -- the definition against the catalog, and, where the change adds a
//!    rule, the **existing rows** against it;
//! 3. apply the change as one catalog `write_batch` (which also bumps the table's
//!    `schema_version`), then release the locks.
//!
//! Step 3 is the only write and it is atomic and durable through the certified
//! `write_batch`, so a failure or a crash anywhere before it leaves the catalog
//! exactly as it was and one after it leaves the change complete: there is no
//! partial `ALTER`. A transaction that buffered rows before the change and commits
//! after it sees the bumped `schema_version` and fails with a `Conflict`
//! (`Transaction::commit`), so it can neither write a row of the old shape nor
//! slip a row past a rule added meanwhile.
//!
//! These live on `IndexBuilder`, the existing home of online DDL that needs the
//! epoch locks, so the SQL executor needs no new handle.

use std::sync::{Arc, RwLock};

use crate::catalog::schema::{IndexKind, IndexState};
use crate::catalog::service::{ColumnDef, NewForeignKey};
use crate::relational::error::{RelationalError, Result};
use crate::relational::index::IndexBuilder;

fn sorted_unique(ids: &[u32]) -> Vec<u32> {
    let mut v = ids.to_vec();
    v.sort_unstable();
    v.dedup();
    v
}

impl IndexBuilder {
    fn epoch_locks(&self, table_ids: &[u32]) -> Vec<Arc<RwLock<()>>> {
        sorted_unique(table_ids)
            .into_iter()
            .map(|id| self.table_store.epoch_lock(id))
            .collect()
    }

    /// `ALTER TABLE ... ADD COLUMN`. The column is appended; rows already stored are
    /// not rewritten -- they read the new column as its `DEFAULT` (or `NULL`). A
    /// `NOT NULL` column needs a `DEFAULT` unless the table is empty.
    pub fn alter_add_column(&self, table_id: u32, column: &ColumnDef) -> Result<u16> {
        let locks = self.epoch_locks(&[table_id]);
        let _guards: Vec<_> = locks
            .iter()
            .map(|l| l.write().unwrap_or_else(|p| p.into_inner()))
            .collect();
        if !column.nullable
            && column.default_value.is_none()
            && self.table_store.table_has_rows(table_id)?
        {
            return Err(RelationalError::InvalidInput {
                detail: format!(
                    "cannot add the NOT NULL column {:?} without a DEFAULT to a table that has rows",
                    column.name
                ),
            });
        }
        Ok(self
            .catalog
            .add_column(table_id, column, column.default_value.clone())?)
    }

    /// `ALTER TABLE ... RENAME COLUMN`.
    pub fn alter_rename_column(&self, table_id: u32, ordinal: u16, new_name: &str) -> Result<()> {
        let locks = self.epoch_locks(&[table_id]);
        let _guards: Vec<_> = locks
            .iter()
            .map(|l| l.write().unwrap_or_else(|p| p.into_inner()))
            .collect();
        Ok(self.catalog.rename_column(table_id, ordinal, new_name)?)
    }

    /// `ALTER COLUMN ... SET DEFAULT <literal>` / `DROP DEFAULT`.
    pub fn alter_set_default(
        &self,
        table_id: u32,
        ordinal: u16,
        default_value: Option<Vec<u8>>,
    ) -> Result<()> {
        let locks = self.epoch_locks(&[table_id]);
        let _guards: Vec<_> = locks
            .iter()
            .map(|l| l.write().unwrap_or_else(|p| p.into_inner()))
            .collect();
        Ok(self
            .catalog
            .set_column_default(table_id, ordinal, default_value)?)
    }

    /// `ALTER COLUMN ... SET NOT NULL`: refused, changing nothing, if any row holds
    /// `NULL` in the column. The scan runs with the table locked against commits,
    /// so no row can acquire a `NULL` between the scan and the change.
    pub fn alter_set_not_null(&self, table_id: u32, ordinal: u16) -> Result<()> {
        let locks = self.epoch_locks(&[table_id]);
        let _guards: Vec<_> = locks
            .iter()
            .map(|l| l.write().unwrap_or_else(|p| p.into_inner()))
            .collect();
        let columns = self.catalog.get_columns(table_id)?;
        let column = columns
            .iter()
            .find(|c| c.ordinal == ordinal)
            .ok_or_else(|| RelationalError::NotFound {
                object: format!("column {ordinal} of table {table_id}"),
            })?;
        if !column.nullable {
            return Ok(());
        }
        let mut has_null = false;
        self.table_store
            .for_each_row_as_of(table_id, u64::MAX, |_, row| {
                if row.get(ordinal as usize).is_none_or(Option::is_none) {
                    has_null = true;
                    return Ok(false);
                }
                Ok(true)
            })?;
        if has_null {
            return Err(RelationalError::InvalidInput {
                detail: format!(
                    "column {:?} contains NULL values; it cannot be made NOT NULL",
                    column.name
                ),
            });
        }
        Ok(self.catalog.set_column_nullable(table_id, ordinal, false)?)
    }

    /// `ALTER COLUMN ... DROP NOT NULL`.
    pub fn alter_drop_not_null(&self, table_id: u32, ordinal: u16) -> Result<()> {
        let locks = self.epoch_locks(&[table_id]);
        let _guards: Vec<_> = locks
            .iter()
            .map(|l| l.write().unwrap_or_else(|p| p.into_inner()))
            .collect();
        Ok(self.catalog.set_column_nullable(table_id, ordinal, true)?)
    }

    /// `ALTER TABLE ... ADD FOREIGN KEY`: the definition is checked, then **every
    /// existing row** of the referencing table is checked against the referenced
    /// table, with both tables locked; the key is recorded only if all pass.
    pub fn alter_add_foreign_key(&self, new: &NewForeignKey) -> Result<u32> {
        let locks = self.epoch_locks(&[new.table_id, new.parent_table_id]);
        let _guards: Vec<_> = locks
            .iter()
            .map(|l| l.write().unwrap_or_else(|p| p.into_inner()))
            .collect();
        self.catalog.check_foreign_key(new)?;

        let (parent, _) = self.table_store.resolve_table(new.parent_table_id)?;
        let access = self.table_store.plan_access(&parent, &new.parent_ordinals)?;
        let mut orphan = false;
        self.table_store
            .for_each_row_as_of(new.table_id, u64::MAX, |_, row| {
                let key: Option<Vec<_>> = new
                    .child_ordinals
                    .iter()
                    .map(|&o| row.get(o as usize).cloned().flatten())
                    .collect();
                // A NULL referencing column references nothing.
                let Some(key) = key else { return Ok(true) };
                let found = self.table_store.find_rows_with_access(
                    new.parent_table_id,
                    &new.parent_ordinals,
                    &key,
                    &access,
                    u64::MAX,
                    1,
                )?;
                if found.is_empty() {
                    orphan = true;
                    return Ok(false);
                }
                Ok(true)
            })?;
        if orphan {
            return Err(RelationalError::ForeignKeyViolation {
                detail: format!(
                    "cannot add foreign key constraint {:?}: existing rows of the referencing table have no matching row in the referenced table",
                    new.name
                ),
            });
        }
        Ok(self.catalog.create_foreign_key(new)?)
    }

    /// `ALTER TABLE ... ADD [CONSTRAINT name] UNIQUE (cols)`: a `UNIQUE` index, built
    /// online. Refused -- and leaving no index row behind -- if the existing rows
    /// already hold a value twice.
    pub fn alter_add_unique(&self, table_id: u32, name: &str, column_ordinals: &[u16]) -> Result<u32> {
        match self.create_index_online(table_id, name, IndexKind::Unique, column_ordinals) {
            Ok(index_id) => Ok(index_id),
            Err(e) => {
                // A failed build leaves a `Failed` row for operator visibility; an
                // `ALTER` that was refused must leave no trace.
                if let Some(failed) = self
                    .catalog
                    .list_indexes(table_id)?
                    .into_iter()
                    .find(|i| i.name == name && i.state == IndexState::Failed)
                {
                    let _ = self.drop_index_online(failed.index_id);
                }
                Err(e)
            }
        }
    }

    /// `ALTER TABLE ... ADD [CONSTRAINT name] CHECK (expression)`. `satisfied` is the
    /// caller's evaluation of the (already bound) expression against one row: `false`
    /// means the row violates it. Every existing row is evaluated with the table
    /// locked against commits; the constraint is recorded only if all pass.
    pub fn alter_add_check(
        &self,
        table_id: u32,
        name: &str,
        column_ordinals: &[u16],
        expression: &str,
        satisfied: &mut dyn FnMut(&crate::relational::Row) -> Result<bool>,
    ) -> Result<u32> {
        let locks = self.epoch_locks(&[table_id]);
        let _guards: Vec<_> = locks
            .iter()
            .map(|l| l.write().unwrap_or_else(|p| p.into_inner()))
            .collect();
        // The name and expression are checked before any row is read.
        if self
            .catalog
            .check_constraints_for(table_id)?
            .iter()
            .any(|c| c.name == name)
        {
            return Err(crate::catalog::CatalogError::AlreadyExists {
                object: format!("constraint {name:?}"),
            }
            .into());
        }
        let mut violated = false;
        self.table_store
            .for_each_row_as_of(table_id, u64::MAX, |_, row| {
                if !satisfied(row)? {
                    violated = true;
                    return Ok(false);
                }
                Ok(true)
            })?;
        if violated {
            return Err(RelationalError::InvalidInput {
                detail: format!(
                    "cannot add CHECK constraint {name:?}: existing rows violate it"
                ),
            });
        }
        Ok(self
            .catalog
            .create_check_constraint(table_id, name, column_ordinals, expression)?)
    }

    /// `ALTER TABLE ... DROP CONSTRAINT <check>`.
    pub fn alter_drop_check(&self, constraint_id: u32) -> Result<()> {
        let check = self
            .catalog
            .check_constraints()?
            .iter()
            .find(|c| c.constraint_id == constraint_id)
            .cloned()
            .ok_or_else(|| RelationalError::NotFound {
                object: format!("constraint {constraint_id}"),
            })?;
        let locks = self.epoch_locks(&[check.table_id]);
        let _guards: Vec<_> = locks
            .iter()
            .map(|l| l.write().unwrap_or_else(|p| p.into_inner()))
            .collect();
        Ok(self.catalog.drop_check_constraint(constraint_id)?)
    }

    /// `ALTER TABLE ... DROP CONSTRAINT <foreign key>`.
    pub fn alter_drop_foreign_key(&self, fk_id: u32) -> Result<()> {
        let fk = self
            .catalog
            .foreign_keys()?
            .iter()
            .find(|f| f.fk_id == fk_id)
            .cloned()
            .ok_or_else(|| RelationalError::NotFound {
                object: format!("foreign key {fk_id}"),
            })?;
        let locks = self.epoch_locks(&[fk.table_id, fk.parent_table_id]);
        let _guards: Vec<_> = locks
            .iter()
            .map(|l| l.write().unwrap_or_else(|p| p.into_inner()))
            .collect();
        Ok(self.catalog.drop_foreign_key(fk_id)?)
    }
}
