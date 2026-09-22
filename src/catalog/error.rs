//! Catalog-layer error taxonomy — `RELATIONAL ADR AMENDMENT 002`. Kept
//! separate from `crate::error::EngineError` rather than adding variants
//! to it: "table already exists," "object not found," and "invalid
//! catalog input" are relational-layer concepts with no meaning at the
//! engine layer, exactly the same reasoning `RELATIONAL ADR AMENDMENT
//! 001` AA.9 already gave for why `write_batch` itself carries no
//! relational-layer authorization concept ("wrong layer, wrong
//! information available"). Never carries key/value bytes, catalog row
//! contents, or filesystem paths — the same discipline every
//! `EngineError` variant already follows.

use crate::error::EngineError;
use std::fmt;

#[derive(Debug)]
pub enum CatalogError {
    /// A lower-layer engine failure, propagated unchanged.
    Engine(EngineError),
    /// A `CREATE`-shaped operation targeted a name already in use within
    /// its namespace (schema-scoped for tables, database-scoped for
    /// schemas, v1's single database for databases).
    AlreadyExists { object: String },
    /// A lookup/`DROP`/reference targeted an object that does not exist.
    NotFound { object: String },
    /// A caller-supplied value violates a catalog-level precondition
    /// (e.g. an empty column list, a `pk_ordinals` entry with no
    /// matching column, an oversized name) — never a byte-corruption
    /// concern (that's `EngineError::Corruption`, propagated via
    /// `Engine` above).
    InvalidInput { detail: String },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::Engine(e) => write!(f, "engine error: {e}"),
            CatalogError::AlreadyExists { object } => write!(f, "already exists: {object}"),
            CatalogError::NotFound { object } => write!(f, "not found: {object}"),
            CatalogError::InvalidInput { detail } => write!(f, "invalid input: {detail}"),
        }
    }
}

impl std::error::Error for CatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CatalogError::Engine(e) => Some(e),
            _ => None,
        }
    }
}

impl From<EngineError> for CatalogError {
    fn from(e: EngineError) -> Self {
        CatalogError::Engine(e)
    }
}

pub type Result<T> = std::result::Result<T, CatalogError>;
