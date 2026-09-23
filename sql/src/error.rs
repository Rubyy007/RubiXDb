//! SQL-layer error taxonomy — `PHASE_RELATIONAL_SQL_GRAMMAR.md` §"Safe
//! error model" (D26/item 31). Distinguishes parse/convert/bind failure
//! classes so a future API/CLI layer can map each to a distinct, safe
//! response, mirroring `catalog::error::CatalogError`/`relational::
//! error::RelationalError`'s own established "kept separate, never
//! carries raw internals" discipline one layer up.
//!
//! **Never** carries: filesystem paths, raw I/O errors, physical engine
//! keys, catalog implementation details, credentials, API keys, or the
//! caller's SQL/parameter values (item 31/44). `UnknownObject` is
//! deliberately the **same** variant an authorization denial produces
//! (item 15/32/42) — see `SqlError::UnknownObject`'s own doc comment.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlError {
    /// The SQL text itself could not be tokenized/parsed. Carries the
    /// parser's own message (already text-only, never SQL text is
    /// echoed back beyond what the parser itself reports positionally)
    /// plus nothing else.
    Parse { detail: String },
    /// A limit from `crate::limits::SqlLimits` was exceeded, checked
    /// *before* the corresponding expensive parse/convert/bind step.
    ResourceLimit { detail: String },
    /// The parser accepted grammar this crate's internal AST has no
    /// representation for (a real SQL feature sqlparser-rs supports but
    /// the approved RubiXDB grammar subset does not) — never silently
    /// dropped, always a hard error (item 5's "do not make an
    /// unimplemented statement appear production-supported").
    Unsupported { detail: String },
    /// Bind-time: an identifier's qualification is syntactically
    /// malformed (more parts than `db.schema.table.column` allows, an
    /// empty part, etc.) — distinct from `UnknownObject` (which covers
    /// "well-formed but does not resolve").
    InvalidIdentifier { detail: String },
    /// **Deliberately the single, indistinguishable outcome for both**
    /// "this object does not exist" **and** "this object exists but the
    /// caller is not authorized to see it" (D25/D26, item 15/32: "a
    /// principal without access to an object must not receive an error
    /// that reveals that the object exists"). Every catalog-resolution
    /// path in the binder must route both failure cases through this one
    /// variant with the same message shape — never a separate
    /// `Forbidden`/`NotFound` pair for object *existence* checks
    /// (`AuthorizationDenied` below is for a different case: an
    /// *action* — e.g. `INSERT` — denied on an object the principal is
    /// already allowed to see via some other privilege, where revealing
    /// "you can SELECT but not INSERT" is not an existence leak).
    UnknownObject { kind: &'static str, detail: String },
    /// A column reference matches more than one table in scope (a join)
    /// without qualification.
    AmbiguousColumn { detail: String },
    /// A value's resolved type does not match what the surrounding
    /// expression/assignment/column requires, or an operator's operand
    /// types are incompatible.
    TypeMismatch { detail: String },
    /// A `$n` parameter reference is malformed (non-numeric, zero,
    /// exceeds `SqlLimits::max_parameters`), or a caller-supplied
    /// parameter count/type does not match what the statement declares.
    InvalidParameter { detail: String },
    /// A statement is well-formed and every identifier resolves, but the
    /// specific action is denied because the principal has no privilege
    /// for it on an object it *is* allowed to know exists (e.g. `SELECT`
    /// granted, `INSERT` not) — never used for "does this object exist,"
    /// which is always `UnknownObject` instead.
    AuthorizationDenied { detail: String },
    /// A lower-layer catalog error, propagated with its own already-safe
    /// `Display` text (never enriched with anything from this layer that
    /// could leak more).
    Catalog(String),
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::Parse { detail } => write!(f, "parse error: {detail}"),
            SqlError::ResourceLimit { detail } => write!(f, "resource limit exceeded: {detail}"),
            SqlError::Unsupported { detail } => write!(f, "unsupported: {detail}"),
            SqlError::InvalidIdentifier { detail } => write!(f, "invalid identifier: {detail}"),
            SqlError::UnknownObject { kind, detail } => {
                write!(f, "unknown {kind}: {detail}")
            }
            SqlError::AmbiguousColumn { detail } => write!(f, "ambiguous column: {detail}"),
            SqlError::TypeMismatch { detail } => write!(f, "type mismatch: {detail}"),
            SqlError::InvalidParameter { detail } => write!(f, "invalid parameter: {detail}"),
            SqlError::AuthorizationDenied { detail } => {
                write!(f, "authorization denied: {detail}")
            }
            SqlError::Catalog(detail) => write!(f, "catalog error: {detail}"),
        }
    }
}

impl std::error::Error for SqlError {}

impl From<rubixdb::catalog::CatalogError> for SqlError {
    fn from(e: rubixdb::catalog::CatalogError) -> Self {
        SqlError::Catalog(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SqlError>;
