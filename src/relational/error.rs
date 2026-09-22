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
}

impl fmt::Display for RelationalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelationalError::Engine(e) => write!(f, "engine error: {e}"),
            RelationalError::Catalog(e) => write!(f, "catalog error: {e}"),
            RelationalError::NotFound { object } => write!(f, "not found: {object}"),
            RelationalError::InvalidInput { detail } => write!(f, "invalid input: {detail}"),
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
