//! The persistent relational catalog — `PHASE_RELATIONAL_DATABASE_ADR.md`
//! D1, resolved concretely by `RELATIONAL ADR AMENDMENT 002`. The catalog
//! is not a new storage mechanism: every row lives in the certified
//! `LsmEngine`'s own keyspace, under the reserved `0x00` namespace (D2),
//! written exclusively through the certified, atomic `LsmEngine::
//! write_batch` (D9/AMENDMENT 001). No new WAL, no new recovery path, no
//! independent catalog file, no in-memory-only authoritative state.
//!
//! Scope of this increment: catalog storage and CRUD for the seven
//! system tables (D1) only. No SQL parser/binder/executor, no user-table
//! row storage, no authorization *enforcement* (D25's grants are stored
//! but not yet checked — that is D15's binder, a later increment).

pub mod encoding;
pub mod error;
pub mod schema;
pub mod service;

pub use error::{CatalogError, Result};
pub use schema::{
    ColumnRow, ConstraintKind, ConstraintRow, DatabaseRow, GrantRow, IndexKind, IndexRow,
    IndexState, ObjectKind, Privilege, SchemaRow, TableRow, TableState,
};
pub use service::{CatalogService, ColumnDef};

#[cfg(test)]
mod tests;
