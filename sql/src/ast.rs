//! RubiXDB's own internal SQL AST — never `sqlparser::ast` types beyond
//! `crate::parse`'s own conversion boundary (item 4 of the governing
//! directive: "do NOT expose sqlparser-rs AST types throughout the
//! database... this prevents the SQL engine architecture from being
//! permanently coupled to one parser library"). Every type here is
//! produced only by `crate::convert::convert_statement`, and is itself
//! still **unbound** — no catalog resolution, no type resolution, no
//! authorization has happened yet (that is `crate::bind`, producing
//! `crate::bound::BoundStatement`).
//!
//! **Execution-status boundary** (item 5): every `Statement` variant
//! below reaches, at most, `BOUND` in this crate (via `crate::bind`) —
//! **none** are executed. A `planner`/`executor` crate does not exist
//! yet. Parsing or binding a statement is never itself a claim that it
//! can run.

/// One already-case-folded identifier (`PHASE_RELATIONAL_SQL_GRAMMAR.md`
/// §"Identifiers"): an unquoted source identifier is folded to lowercase
/// here (once, at conversion time — `crate::convert`), a quoted one
/// (`"MixedCase"`) is kept byte-for-byte as written, matching
/// `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §1's already-committed
/// rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Ident {
    pub value: String,
    pub quoted: bool,
}

impl Ident {
    pub fn new(value: impl Into<String>, quoted: bool) -> Self {
        Ident {
            value: value.into(),
            quoted,
        }
    }
}

/// A possibly-qualified name (`table`, `schema.table`, `db.schema.table`,
/// or the same depth for a column: `column`, `table.column`,
/// `schema.table.column`) — parts in the order written, left to right.
/// Resolved against the catalog only in `crate::bind` (item 16: "Do not
/// maintain another authoritative metadata map").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectName(pub Vec<Ident>);

impl ObjectName {
    pub fn single(ident: Ident) -> Self {
        ObjectName(vec![ident])
    }

    /// The last (most specific) part — e.g. the table name of
    /// `schema.table`, or the column name of `table.column`.
    pub fn last(&self) -> &Ident {
        self.0.last().expect("ObjectName is never empty")
    }
}

/// A raw literal value — deliberately **not** yet a `RelationalValue`
/// (item 6: the mapping happens "eventually," at bind time, once a
/// target type is known; a bare `123` has no fixed type until context —
/// a target column, a comparison partner — narrows it, item 21).
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Boolean(bool),
    /// Raw numeric text exactly as tokenized (sign already folded in for
    /// a simple negated numeric literal — see `crate::convert` — so
    /// `-9223372036854775808` binds correctly to `BIGINT`'s minimum
    /// rather than overflowing a positive-only parse then negating).
    /// `is_integer` is `true` iff the text contains neither `.` nor an
    /// exponent, the syntactic signal `crate::bind` uses to prefer an
    /// integer type when no other context applies (item 21).
    Number { text: String, is_integer: bool },
    Text(String),
    /// `X'...'`/`x'...'` hex string literals only (item 6's `BLOB`).
    Blob(Vec<u8>),
    /// A typed string literal, `TYPE 'text'` (standard ANSI SQL syntax,
    /// e.g. `DATE '2024-01-01'`, `TIME '12:00:00'`, `TIMESTAMP
    /// '2024-01-01 12:00:00'`) — the only way item 6's temporal types
    /// enter an expression as a literal (there is no bare-number-to-
    /// `DATE` coercion, item 21).
    Typed { data_type: SqlDataType, text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

/// An unresolved column reference: 1–3 parts (`column`, `table.column`,
/// `schema.table.column`), qualification resolved only in `crate::bind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    pub parts: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    /// `$n`, 1-based (item 7) — never `?` or any other placeholder form
    /// (rejected at conversion time, `crate::convert`).
    Parameter(u32),
    Column(ColumnRef),
    UnaryOp {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    BinaryOp {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        negated: bool,
        low: Box<Expr>,
        high: Box<Expr>,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
        case_insensitive: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
    },
    /// A call into `crate::functions`' explicit registry — an unknown
    /// name is rejected at *bind* time (item 23: "unknown functions must
    /// fail during binding"), not conversion time, since the registry is
    /// catalog-independent but still logically part of binding.
    Function {
        name: ObjectName,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectItemExpr {
    pub expr: Expr,
    pub alias: Option<Ident>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Item(SelectItemExpr),
    /// `*`
    Wildcard,
    /// `table.*` or `schema.table.*`
    QualifiedWildcard(ObjectName),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub name: ObjectName,
    pub alias: Option<Ident>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    pub on: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FromClause {
    pub first: TableRef,
    pub joins: Vec<Join>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub descending: bool,
    /// `None` = database default (D5: `NULLS LAST` ascending / `NULLS
    /// FIRST` descending), `Some(true)`/`Some(false)` = explicit
    /// `NULLS FIRST`/`NULLS LAST`.
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub selection: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: ObjectName,
    /// `None` = every column, in catalog ordinal order (`INSERT INTO t
    /// VALUES (...)`); `Some` = the explicit `(col, ...)` list.
    pub columns: Option<Vec<Ident>>,
    pub rows: Vec<Vec<Expr>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: Ident,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub table: ObjectName,
    pub assignments: Vec<Assignment>,
    pub selection: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: ObjectName,
    pub selection: Option<Expr>,
}

/// RubiXDB's own closed SQL type-name set — D4's type system, named at
/// the SQL surface (item 6: "use the existing relational type system, do
/// not duplicate datatype definitions" — `crate::bind` maps each variant
/// onto `rubixdb::relational::RelationalType` 1:1, never redefining a
/// type's own semantics here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDataType {
    Boolean,
    Integer,
    Bigint,
    Real,
    Double,
    Decimal { precision: u8, scale: u8 },
    Text,
    Blob,
    Date,
    Time,
    Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: Ident,
    pub data_type: SqlDataType,
    /// `true` unless `NOT NULL` (or an inline `PRIMARY KEY`, which is
    /// always `NOT NULL`, D6) was present.
    pub nullable: bool,
    pub default: Option<Expr>,
    /// Inline `col TYPE PRIMARY KEY` — folded together with any table-
    /// level `PRIMARY KEY(...)` clause at bind time (`crate::bind::ddl`).
    pub primary_key: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub name: ObjectName,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
    /// A table-level `PRIMARY KEY(a, b, ...)` constraint, if present
    /// (composite keys, D6).
    pub table_primary_key: Option<Vec<Ident>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropTable {
    pub name: ObjectName,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateDatabase {
    pub name: Ident,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSchema {
    /// `schema` or `db.schema`.
    pub name: ObjectName,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIndex {
    pub name: Option<Ident>,
    pub table: ObjectName,
    pub columns: Vec<Ident>,
    pub unique: bool,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropIndex {
    /// The catalog scopes an index name's uniqueness per-table (`CA.2`),
    /// so — unlike most objects — `DROP INDEX` needs its target table
    /// named explicitly (`DROP INDEX name ON table`, the same MySQL-
    /// style syntax `sqlparser` already models for exactly this reason).
    pub table: ObjectName,
    pub name: Ident,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Explain {
    pub statement: Box<Statement>,
}

/// A statement's execution-status boundary (item 5) is documented per
/// variant; every one of them, without exception, is `PARSED` and, once
/// `crate::bind` accepts it, `BOUND` — **never** `EXECUTED`. No variant
/// here is a claim that the corresponding SQL feature works end to end.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `SELECT` — PARSED, BOUND. NOT EXECUTABLE YET (no executor).
    Select(Select),
    /// `INSERT` — PARSED, BOUND. NOT EXECUTABLE YET (bound form carries
    /// everything a future executor needs to perform the real
    /// `TableStore`/index write without reparsing, item 26 — but this
    /// crate never performs that write itself).
    Insert(Insert),
    /// `UPDATE` — PARSED, BOUND. NOT EXECUTABLE YET.
    Update(Update),
    /// `DELETE` — PARSED, BOUND. NOT EXECUTABLE YET.
    Delete(Delete),
    /// `CREATE DATABASE` — PARSED, BOUND. NOT EXECUTABLE YET (D1: even
    /// once an executor exists, this deployment's v1 scope rejects it at
    /// execution time — parsing/binding it is forward grammar coverage
    /// only, per D1's own stated precedent).
    CreateDatabase(CreateDatabase),
    /// `CREATE SCHEMA` — PARSED, BOUND. NOT EXECUTABLE YET.
    CreateSchema(CreateSchema),
    /// `CREATE TABLE` — PARSED, BOUND. NOT EXECUTABLE YET (the bound
    /// form is shaped to call `CatalogService::create_table` directly,
    /// item 25 — but this crate never calls it).
    CreateTable(CreateTable),
    /// `DROP TABLE` — PARSED, BOUND. NOT EXECUTABLE YET.
    DropTable(DropTable),
    /// `CREATE INDEX` — PARSED, BOUND. NOT EXECUTABLE YET (the bound
    /// form is shaped to call `IndexBuilder::create_index_online`
    /// directly — but this crate never calls it).
    CreateIndex(CreateIndex),
    /// `DROP INDEX` — PARSED, BOUND. NOT EXECUTABLE YET.
    DropIndex(DropIndex),
    /// `EXPLAIN <stmt>` — PARSED, BOUND (the inner statement is bound
    /// too). NOT EXECUTABLE YET — item 30: "do not generate a fake
    /// plan," and none is generated; a future planner owns that.
    Explain(Explain),
    /// `BEGIN`/`START TRANSACTION` — PARSED, BOUND as a trivial marker
    /// statement. NOT EXECUTABLE YET — item 29: "do not implement
    /// transaction execution here... do not claim transactions are
    /// supported merely because the parser accepts the syntax."
    Begin,
    /// `COMMIT` — PARSED, BOUND. NOT EXECUTABLE YET (see `Begin`).
    Commit,
    /// `ROLLBACK` — PARSED, BOUND. NOT EXECUTABLE YET (see `Begin`).
    Rollback,
}
