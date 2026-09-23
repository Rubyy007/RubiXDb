//! The bound/resolved internal representation — `crate::bind`'s output.
//! Every identifier here is an authoritative catalog reference (item 14:
//! "the future planner must NOT need to repeat name resolution") and
//! every object reference has already passed authorization (item 15).
//! No variant here is executed by this crate.

use rubixdb::catalog::schema::IndexKind;
use rubixdb::relational::{RelationalType, RelationalValue};

use crate::ast::{BinaryOp, JoinKind, UnaryOp};

/// A stable index into a statement's own FROM-scope (`BoundSelect::
/// from`/`BoundUpdate::table`/`BoundDelete::table`'s position) — never a
/// name a later pass would need to re-resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableRefId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundTableRef {
    pub id: TableRefId,
    pub database_id: u32,
    pub schema_id: u32,
    pub table_id: u32,
    /// The name a column reference must qualify against within this
    /// statement (the alias if one was given, else the table's own
    /// catalog name — already case-folded, `crate::convert`).
    pub effective_name: String,
    /// `true` for the introduced (right-hand) side of a `LEFT JOIN` —
    /// every column from this table binds as `nullable: true` regardless
    /// of its own declared `NOT NULL` (D18: unmatched left rows produce
    /// `NULL` on this side).
    pub null_extended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnRef {
    pub table_ref: TableRefId,
    pub table_id: u32,
    pub ordinal: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundExpr {
    pub kind: BoundExprKind,
    /// `None` only for an untyped `NULL` literal with no narrowing
    /// context (item 21) — every other expression has a concrete,
    /// resolved `RelationalType`.
    pub ty: Option<RelationalType>,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundExprKind {
    /// `None` = `NULL` (of `BoundExpr::ty`'s type, if narrowed; fully
    /// untyped otherwise).
    Literal(Option<RelationalValue>),
    Parameter {
        index: u32,
    },
    Column(ColumnRef),
    UnaryOp {
        op: UnaryOp,
        expr: Box<BoundExpr>,
    },
    BinaryOp {
        left: Box<BoundExpr>,
        op: BinaryOp,
        right: Box<BoundExpr>,
    },
    IsNull {
        expr: Box<BoundExpr>,
        negated: bool,
    },
    Between {
        expr: Box<BoundExpr>,
        negated: bool,
        low: Box<BoundExpr>,
        high: Box<BoundExpr>,
    },
    InList {
        expr: Box<BoundExpr>,
        list: Vec<BoundExpr>,
        negated: bool,
    },
    Like {
        expr: Box<BoundExpr>,
        pattern: Box<BoundExpr>,
        negated: bool,
        case_insensitive: bool,
    },
    Case {
        operand: Option<Box<BoundExpr>>,
        branches: Vec<(BoundExpr, BoundExpr)>,
        else_result: Option<Box<BoundExpr>>,
    },
    Function {
        name: &'static str,
        args: Vec<BoundExpr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelectItem {
    pub expr: BoundExpr,
    /// The alias if one was given, else a derived display name (a plain
    /// column reference's own name; any other expression's name is
    /// implementation-defined — a future planner is free to synthesize
    /// one, since no client-visible contract depends on it yet).
    pub output_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundFromItem {
    pub table: BoundTableRef,
    /// `None` for the first (base) item; `Some((kind, on_condition))`
    /// for every joined item.
    pub join: Option<(JoinKind, BoundExpr)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsOrder {
    First,
    Last,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundOrderByItem {
    pub expr: BoundExpr,
    pub descending: bool,
    /// Always resolved to a concrete order here (D5's default applied
    /// when the statement didn't specify one) — never left ambiguous for
    /// a later pass to have to re-derive.
    pub nulls: NullsOrder,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelect {
    pub distinct: bool,
    pub from: Vec<BoundFromItem>,
    pub projection: Vec<BoundSelectItem>,
    pub selection: Option<BoundExpr>,
    pub order_by: Vec<BoundOrderByItem>,
    pub limit: Option<BoundExpr>,
    pub offset: Option<BoundExpr>,
    /// The highest `$n` parameter index referenced anywhere in this
    /// statement (0 if none) — a caller can validate a supplied
    /// parameter list's length against this without re-walking the tree.
    pub max_parameter: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundInsert {
    pub database_id: u32,
    pub schema_id: u32,
    pub table_id: u32,
    /// One row per `VALUES (...)` tuple; each row has exactly one entry
    /// per catalog column, in ordinal order (a column omitted from an
    /// explicit column list binds as an untyped `NULL` `BoundExpr` here
    /// — `crate::bind` has already checked that is legal, i.e. the
    /// column is nullable or has a default; execution-time default-value
    /// substitution, if any, is a future executor's own concern).
    pub rows: Vec<Vec<BoundExpr>>,
    pub max_parameter: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundAssignment {
    pub ordinal: u16,
    pub value: BoundExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundUpdate {
    pub table: BoundTableRef,
    pub assignments: Vec<BoundAssignment>,
    pub selection: Option<BoundExpr>,
    pub max_parameter: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundDelete {
    pub table: BoundTableRef,
    pub selection: Option<BoundExpr>,
    pub max_parameter: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundColumnDef {
    pub name: String,
    /// `rubixdb::relational::value::TYPE_TAG_*` — the exact tag
    /// `CatalogService::create_table`'s `ColumnDef::data_type` expects
    /// (item 25: "do not bypass `CatalogService`").
    pub data_type: u8,
    pub type_params: Option<Vec<u8>>,
    pub nullable: bool,
    /// Already-encoded `RelationalValue` bytes for a literal `DEFAULT`
    /// expression (`crate::bind::ddl` requires `DEFAULT` to bind to a
    /// literal — no function calls or column references, matching
    /// `sqlparser`'s own "restricted-expr" framing for this clause).
    pub default_value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundCreateTable {
    pub database_id: u32,
    pub schema_id: u32,
    pub name: String,
    pub columns: Vec<BoundColumnDef>,
    pub pk_ordinals: Vec<u16>,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundDropTable {
    /// `None` only when `IF EXISTS` was given and the table genuinely
    /// does not exist (a real no-op, not an error) — `if_exists` alone
    /// does not imply this; it is `Some` whenever the table *was*
    /// found, `IF EXISTS` or not.
    pub table_id: Option<u32>,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundCreateIndex {
    pub table_id: u32,
    pub name: String,
    pub kind: IndexKind,
    pub column_ordinals: Vec<u16>,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundDropIndex {
    /// See `BoundDropTable::table_id`'s doc comment — the same `IF
    /// EXISTS`-no-op shape.
    pub index_id: Option<u32>,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundCreateSchema {
    pub database_id: u32,
    pub name: String,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundCreateDatabase {
    pub name: String,
    pub if_not_exists: bool,
}

/// The fully bound, authorized, typed representation of one statement.
/// **Execution-status boundary** (item 5): every variant is `BOUND` and
/// stops there — nothing in this crate executes any of them.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundStatement {
    Select(BoundSelect),
    Insert(BoundInsert),
    Update(BoundUpdate),
    Delete(BoundDelete),
    CreateDatabase(BoundCreateDatabase),
    CreateSchema(BoundCreateSchema),
    CreateTable(BoundCreateTable),
    DropTable(BoundDropTable),
    CreateIndex(BoundCreateIndex),
    DropIndex(BoundDropIndex),
    Explain(Box<BoundStatement>),
    Begin,
    Commit,
    Rollback,
}
