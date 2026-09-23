//! Relational row-storage error taxonomy — `RELATIONAL ADR AMENDMENT
//! 003`. Mirrors `catalog::error::CatalogError`'s own reasoning exactly:
//! kept separate from both `EngineError` and `CatalogError` because "row
//! too large," "wrong number of primary-key values," and "NaN in a key
//! column" are relational-storage concepts with no meaning at either
//! lower layer. Never carries key/value bytes or row contents.

use crate::catalog::error::CatalogError;
use crate::error::EngineError;
use std::fmt;

#[derive(Debug)]
pub enum RelationalError {
    Engine(EngineError),
    Catalog(CatalogError),
    /// A lookup targeted a table/row that does not exist.
    NotFound {
        object: String,
    },
    /// A caller-supplied value violates a row-storage-level precondition
    /// (wrong column count, oversized row, `NaN`/negative-`TIME` in a
    /// key-bearing column, precision overflow, etc.) — never a byte-
    /// corruption concern (that is `Corruption`/`InvalidInput` from a
    /// lower layer, propagated via `Engine`/`Catalog` above).
    InvalidInput {
        detail: String,
    },
    /// `PHASE_RELATIONAL_TRANSACTION_ARCHITECTURE.md` (D10, item 11/13/
    /// 14): commit-time validation found that a physical key the
    /// transaction's write-set touches was modified (by an already-
    /// committed transaction, or by a `PRIMARY KEY`/`UNIQUE` value
    /// another concurrent commit — or this same transaction's own
    /// write-set — already claims) after the transaction's snapshot.
    /// The transaction is aborted; nothing in its write-set is applied.
    Conflict {
        detail: String,
    },
    /// A transaction resource limit (`TxnLimits`) was exceeded, checked
    /// before the corresponding allocation.
    ResourceLimit {
        detail: String,
    },
    /// A transaction method was called in a state that does not permit
    /// it. In practice unreachable from outside this crate for the
    /// "already committed/rolled back" cases — `Transaction::commit`/
    /// `rollback` consume `self` by value, so the Rust type system
    /// itself, not a runtime check, prevents calling anything on an
    /// already-finished transaction (item 4/30's "do not panic on an
    /// illegal transition" is satisfied at compile time where possible,
    /// this variant covers what remains, defensively).
    InvalidTransactionState {
        detail: String,
    },
}

impl fmt::Display for RelationalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelationalError::Engine(e) => write!(f, "engine error: {e}"),
            RelationalError::Catalog(e) => write!(f, "catalog error: {e}"),
            RelationalError::NotFound { object } => write!(f, "not found: {object}"),
            RelationalError::InvalidInput { detail } => write!(f, "invalid input: {detail}"),
            RelationalError::Conflict { detail } => write!(f, "transaction conflict: {detail}"),
            RelationalError::ResourceLimit { detail } => {
                write!(f, "resource limit exceeded: {detail}")
            }
            RelationalError::InvalidTransactionState { detail } => {
                write!(f, "invalid transaction state: {detail}")
            }
        }
    }
}

impl std::error::Error for RelationalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RelationalError::Engine(e) => Some(e),
            RelationalError::Catalog(e) => Some(e),
            _ => None,
        }
    }
}

impl From<EngineError> for RelationalError {
    fn from(e: EngineError) -> Self {
        RelationalError::Engine(e)
    }
}

impl From<CatalogError> for RelationalError {
    fn from(e: CatalogError) -> Self {
        RelationalError::Catalog(e)
    }
}

pub type Result<T> = std::result::Result<T, RelationalError>;
