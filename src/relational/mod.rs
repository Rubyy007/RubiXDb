//! The relational row-storage foundation — `RELATIONAL ADR AMENDMENT
//! 003`. Connects the completed catalog (`crate::catalog`) to actual
//! user-table row storage: D4's full type system, D3's `RowValue`
//! applied to it, D2's order-preserving key encoding, and `TableStore`'s
//! put/get/delete/scan primitives — every mutation through the certified
//! `LsmEngine::write_batch`, every read an ordinary `get`/`range_scan`.
//!
//! Scope of this increment: row-storage primitives only. No SQL parser/
//! binder/executor, no `CREATE TABLE`/`INSERT`/`UPDATE`/`DELETE`/
//! `SELECT` SQL, no query planning, no joins, no aggregation, no
//! authorization enforcement (deferred to D15's binder, per the existing
//! architecture).

pub mod error;
pub mod key;
pub mod table_store;
pub mod value;

pub use error::{RelationalError, Result};
pub use table_store::{Row, TableStore, MAX_ROW_VALUE_BYTES};
pub use value::{RelationalType, RelationalValue};

#[cfg(test)]
mod tests;
