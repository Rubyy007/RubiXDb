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
    /// `PHASE_RELATIONAL_QUERY_PLANNER_ARCHITECTURE.md` (item 29): a
    /// structural post-optimization plan-validation check failed (a
    /// referenced table/column/index does not belong to the bound
    /// statement's own scope, or an optimization rule produced an
    /// internally inconsistent plan). This is a defensive, should-never-
    /// fire check on this crate's own output, not a user-facing SQL
    /// error class — reported the same safe way regardless.
    PlanValidation { detail: String },
    /// `PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md` (item 38): a
    /// lower-layer storage/relational error surfaced during execution —
    /// I/O failure, corruption (the certified Read Engine's own fail-
    /// closed contract, item 39, propagated unchanged), or any other
    /// `RelationalError`/`EngineError` this crate does not have a more
    /// specific variant for. Carries only that error's own already-safe
    /// `Display` text (never a filesystem path, physical key, or raw I/O
    /// detail — the lower layers' own established discipline, reused).
    Storage(String),
    /// A physical-plan node this crate's executor has no implementation
    /// for reached execution (item 65: "no plan node may silently fall
    /// through... return `UnsupportedExecution`"). In practice
    /// unreachable for any `Plan::Query` this crate's own planner
    /// produces (every `PhysicalPlan` variant has a real operator), but
    /// covers `Plan` variants this increment does not execute at all
    /// (`Insert`/`Update`/`Delete`/`Ddl`/`Begin`/`Commit`/`Rollback` —
    /// item 5: "All writes are outside this increment").
    UnsupportedExecution { detail: String },
    /// A caller-supplied runtime parameter is missing, `NULL` where the
    /// bound expression's own context requires a value, or otherwise
    /// does not match what `BoundExpr::Parameter`'s own resolved type
    /// requires (item 34). Never carries the parameter's own value.
    ExecutionParameter { detail: String },
    /// Execution was cancelled by its caller before completion (item
    /// 40) — a controlled stop, not a failure of the query itself.
    Cancelled,
    /// The execution deadline (item 41, `ExecLimits::deadline`) elapsed
    /// before the query finished. Checked against `Instant::now()`
    /// (monotonic), never wall-clock time.
    DeadlineExceeded,
    /// A `Transaction` operation this execution depended on reported a
    /// snapshot-isolation conflict (D10) — reserved for when write
    /// execution lands in a future increment; no code path in this
    /// increment's read-only executor can produce it yet (`Transaction::
    /// get_row` never conflict-checks; only `commit()` does, and nothing
    /// here calls it with a non-empty write-set).
    Conflict { detail: String },
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
            SqlError::PlanValidation { detail } => write!(f, "plan validation failed: {detail}"),
            SqlError::Storage(detail) => write!(f, "storage error: {detail}"),
            SqlError::UnsupportedExecution { detail } => {
                write!(f, "unsupported execution: {detail}")
            }
            SqlError::ExecutionParameter { detail } => {
                write!(f, "invalid execution parameter: {detail}")
            }
            SqlError::Cancelled => write!(f, "execution cancelled"),
            SqlError::DeadlineExceeded => write!(f, "execution deadline exceeded"),
            SqlError::Conflict { detail } => write!(f, "transaction conflict: {detail}"),
        }
    }
}

impl std::error::Error for SqlError {}

impl From<rubixdb::catalog::CatalogError> for SqlError {
    fn from(e: rubixdb::catalog::CatalogError) -> Self {
        SqlError::Catalog(e.to_string())
    }
}

/// `PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md` §3: every `Table
/// Store`/`IndexBuilder`/`Transaction` call the executor makes returns
/// `rubixdb::relational::Result` — this is the one conversion point,
/// classifying each `RelationalError` variant into the safe SQL-layer
/// class it belongs to rather than a single catch-all (item 38).
impl From<rubixdb::relational::RelationalError> for SqlError {
    fn from(e: rubixdb::relational::RelationalError) -> Self {
        use rubixdb::relational::RelationalError as RE;
        match e {
            RE::ResourceLimit { detail } => SqlError::ResourceLimit { detail },
            RE::Conflict { detail } => SqlError::Conflict { detail },
            RE::InvalidInput { detail } => SqlError::ExecutionParameter { detail },
            RE::NotFound { .. }
            | RE::Engine(_)
            | RE::Catalog(_)
            | RE::InvalidTransactionState { .. } => SqlError::Storage(e.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, SqlError>;
