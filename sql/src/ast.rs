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
    Number {
        text: String,
        is_integer: bool,
    },
    Text(String),
    /// `X'...'`/`x'...'` hex string literals only (item 6's `BLOB`).
    Blob(Vec<u8>),
    /// A typed string literal, `TYPE 'text'` (standard ANSI SQL syntax,
    /// e.g. `DATE '2024-01-01'`, `TIME '12:00:00'`, `TIMESTAMP
    /// '2024-01-01 12:00:00'`) — the only way item 6's temporal types
    /// enter an expression as a literal (there is no bare-number-to-
    /// `DATE` coercion, item 21).
    Typed {
        data_type: SqlDataType,
        text: String,
    },
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
    /// An aggregate function call (Increment 11) — COUNT, SUM, AVG, MIN, MAX.
    /// SQL-mission-2: `distinct` is `AGG(DISTINCT x)`, `filter` is
    /// `AGG(x) FILTER (WHERE cond)`.
    Aggregate {
        func: crate::aggregate::AggregateFunc,
        arg: crate::aggregate::AggregateArg<Box<Expr>>,
        distinct: bool,
        filter: Option<Box<Expr>>,
    },
    /// SQL-mission-2: a window function call, `f(args) OVER (...)`.
    Window(Box<WindowCall>),
    /// SQL-mission-2: `GROUPING(e1, ...)`.
    Grouping(Vec<Expr>),
    /// SQL-mission-1: a parenthesized scalar subquery, `(SELECT ...)`.
    ScalarSubquery(Box<QueryExpr>),
    /// SQL-mission-1: `[NOT] EXISTS (SELECT ...)`.
    Exists {
        subquery: Box<QueryExpr>,
        negated: bool,
    },
    /// SQL-mission-1: `expr [NOT] IN (SELECT ...)`.
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<QueryExpr>,
        negated: bool,
    },
    /// `expr <op> ANY|SOME|ALL (SELECT ...)`: `op` is one of the six
    /// comparisons; `all` distinguishes `ALL` from `ANY` / `SOME`.
    Quantified {
        expr: Box<Expr>,
        op: BinaryOp,
        all: bool,
        subquery: Box<QueryExpr>,
    },
}

/// SQL-mission-1: a query used as a value or a relation (a subquery, a CTE
/// body, a derived table, a set-operation branch) — the one nested-query
/// shape every one of those forms shares. Extended by later slices of the
/// mission with set operations; a top-level statement keeps using
/// `Statement::Select` for the plain case.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryExpr {
    Select(Box<Select>),
}

/// What a window function call computes (SQL-mission-2).
#[derive(Debug, Clone, PartialEq)]
pub enum WindowFunction {
    /// `SUM(x) OVER (...)` and the other aggregates used as window functions.
    Aggregate {
        func: crate::aggregate::AggregateFunc,
        arg: crate::aggregate::AggregateArg<Box<Expr>>,
        distinct: bool,
        filter: Option<Box<Expr>>,
    },
    /// `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `NTILE`, `PERCENT_RANK`,
    /// `CUME_DIST`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE`
    /// (`name` is lower-case; the binder checks the arity and the types).
    Named { name: String, args: Vec<Expr> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowCall {
    pub function: WindowFunction,
    pub over: WindowOver,
    /// `IGNORE NULLS` written after the call (`LAG(x) IGNORE NULLS OVER (...)`).
    /// `RESPECT NULLS`, the default, is `false`.
    pub ignore_nulls: bool,
}

/// The `OVER` clause: a name defined by `WINDOW`, or an inline specification.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowOver {
    Named(Ident),
    Spec(WindowSpec),
}

/// `([base] [PARTITION BY ...] [ORDER BY ...] [frame])`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowSpec {
    /// An existing window this one extends (`OVER (w ORDER BY x)`).
    pub base: Option<Ident>,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub frame: Option<FrameSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameUnits {
    Rows,
    Range,
    /// `GROUPS`: offsets count peer groups (rows equal under the window's
    /// `ORDER BY`), PostgreSQL 11+.
    Groups,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(Box<Expr>),
    CurrentRow,
    Following(Box<Expr>),
    UnboundedFollowing,
}

/// `ROWS|RANGE BETWEEN start AND end`; the shorthand `ROWS start` is
/// `BETWEEN start AND CURRENT ROW`.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameSpec {
    pub units: FrameUnits,
    pub start: FrameBound,
    pub end: FrameBound,
}

/// One element of a `GROUP BY` that uses `ROLLUP`, `CUBE` or `GROUPING SETS`
/// (SQL-mission-2). Each inner `Vec<Expr>` is one (possibly composite) member.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupElement {
    Expr(Expr),
    Rollup(Vec<Vec<Expr>>),
    Cube(Vec<Vec<Expr>>),
    Sets(Vec<Vec<Expr>>),
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

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    /// A base table's (or CTE's) name. For a derived table this is the
    /// alias (or empty when none was written) -- callers must check
    /// `derived` first.
    pub name: ObjectName,
    pub alias: Option<Ident>,
    /// SQL-mission-1 Slice C: `(SELECT ...) AS alias`. `Some` makes this a
    /// derived table whose body is this query.
    pub derived: Option<Box<Select>>,
    /// `LATERAL (SELECT ...)`: the body may read the FROM items before it.
    pub lateral: bool,
    /// `AS alias(a, b, ...)`: the relation's column names, replacing the
    /// body's output names. Empty when none were written.
    pub column_aliases: Vec<Ident>,
}

/// One `WITH name [(columns)] AS (SELECT ...)` definition.
#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: Ident,
    pub columns: Vec<Ident>,
    pub query: Box<Select>,
    /// SQL-mission-1 Slice D: the `WITH` list was written `WITH RECURSIVE`
    /// (the keyword belongs to the whole list; it is set on every definition
    /// in it). Whether a definition actually recurses is decided by the
    /// binder: only a body of the form `seed UNION [ALL] recursive-term` that
    /// names itself does.
    pub recursive: bool,
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

/// The set operators (SQL-mission-1 Slice E). `INTERSECT` binds tighter than
/// `UNION` and `EXCEPT`, which are left-associative at one level -- the parser
/// (`sqlparser`) builds the tree that way and `crate::convert` keeps its shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOperator {
    Union,
    Intersect,
    Except,
}

impl SetOperator {
    pub fn name(self) -> &'static str {
        match self {
            SetOperator::Union => "UNION",
            SetOperator::Intersect => "INTERSECT",
            SetOperator::Except => "EXCEPT",
        }
    }
}

/// `left <op> [ALL] right` (SQL-mission-1 Slice E). Both operands are whole
/// queries: a plain `SELECT`, a parenthesised query with its own `ORDER BY` /
/// `LIMIT` / `WITH`, or another set operation.
#[derive(Debug, Clone, PartialEq)]
pub struct SetOperation {
    pub op: SetOperator,
    /// `ALL`: multiset semantics, no duplicate removal. `false` is the
    /// default (and `DISTINCT`) quantifier.
    pub all: bool,
    /// `UNION [ALL] BY NAME`: the operands' columns are matched by **name**, not
    /// by position (a column one operand lacks is `NULL` there). Snowflake and
    /// DuckDB spelling; only `UNION` has it.
    pub by_name: bool,
    pub left: Box<Select>,
    pub right: Box<Select>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    /// SQL-mission-1 Slice C: the `WITH` list of this query (empty if none).
    pub ctes: Vec<Cte>,
    /// SQL-mission-1 Slice E: `Some` makes this query a **set operation**. It
    /// then has no select list, `FROM`, `WHERE`, `GROUP BY` or `HAVING` of its
    /// own (`distinct` is `false`, `projection` and the other clauses are
    /// empty) and its `ctes`, `order_by`, `limit` and `offset` apply to the
    /// whole result of the set operation. Every consumer of a `Select` that
    /// can see a set operation (the binder) checks this first.
    pub set_op: Option<SetOperation>,
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    /// `GROUP BY ALL` (Snowflake): group by every select-list item that is not an
    /// aggregate or window function call. `group_by` and `grouping` are then empty.
    pub group_by_all: bool,
    /// SQL-mission-2: `Some` when the `GROUP BY` uses `ROLLUP`, `CUBE` or
    /// `GROUPING SETS`; it then holds *every* element of the clause in written
    /// order (plain expressions included) and `group_by` is empty.
    pub grouping: Option<Vec<GroupElement>>,
    pub having: Option<Expr>,
    /// SQL-mission-2: `QUALIFY` -- a filter evaluated after window functions.
    pub qualify: Option<Expr>,
    /// SQL-mission-2: `WINDOW name AS (...)` definitions.
    pub named_windows: Vec<(Ident, WindowSpec)>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

impl Select {
    /// `left <op> [ALL] right` as a query of its own: no clauses besides the
    /// operation (the caller attaches `WITH` / `ORDER BY` / `LIMIT`).
    pub fn set_operation(op: SetOperator, all: bool, left: Select, right: Select) -> Select {
        Self::set_operation_with(op, all, false, left, right)
    }

    /// `set_operation` with the `BY NAME` flag.
    pub fn set_operation_with(
        op: SetOperator,
        all: bool,
        by_name: bool,
        left: Select,
        right: Select,
    ) -> Select {
        Select {
            ctes: Vec::new(),
            set_op: Some(SetOperation {
                op,
                all,
                by_name,
                left: Box::new(left),
                right: Box::new(right),
            }),
            distinct: false,
            projection: Vec::new(),
            from: None,
            selection: None,
            group_by: Vec::new(),
            group_by_all: false,
            grouping: None,
            having: None,
            qualify: None,
            named_windows: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    /// A `WITH` list written before the statement (SQL-mission-1 Slice C).
    pub ctes: Vec<Cte>,
    pub table: ObjectName,
    /// `INSERT INTO t AS alias`: the name the existing row goes by in
    /// `ON CONFLICT DO UPDATE` and `RETURNING`.
    pub table_alias: Option<Ident>,
    /// `None` = every column, in catalog ordinal order (`INSERT INTO t
    /// VALUES (...)`); `Some` = the explicit `(col, ...)` list.
    pub columns: Option<Vec<Ident>>,
    /// The `VALUES` tuples (empty when the rows come from `query`).
    pub rows: Vec<Vec<Expr>>,
    /// SQL Phase 2a: `INSERT ... SELECT` -- the query whose rows are inserted.
    pub query: Option<Box<Select>>,
    /// SQL Phase 2a: `ON CONFLICT [target] DO NOTHING | DO UPDATE ...`.
    pub on_conflict: Option<OnConflict>,
    /// SQL Phase 2a: `RETURNING <select list>`.
    pub returning: Option<Vec<SelectItem>>,
}

/// What an `ON CONFLICT` clause names as the unique constraint it arbitrates on.
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictTarget {
    /// `(col, ...)`: the unique constraint or unique index over exactly these columns.
    Columns(Vec<Ident>),
    /// `ON CONSTRAINT name`: a primary key (`<table>_pkey`) or unique index by name.
    Constraint(Ident),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConflictAction {
    DoNothing,
    /// `DO UPDATE SET ... [WHERE ...]`: the existing row is `table` (or its alias),
    /// the proposed row is `EXCLUDED`.
    DoUpdate {
        assignments: Vec<Assignment>,
        selection: Option<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OnConflict {
    pub target: Option<ConflictTarget>,
    pub action: ConflictAction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: Ident,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// A `WITH` list written before the statement (SQL-mission-1 Slice C).
    pub ctes: Vec<Cte>,
    pub table: ObjectName,
    pub assignments: Vec<Assignment>,
    pub selection: Option<Expr>,
    /// SQL Phase 2a: `RETURNING <select list>` (the updated rows' new values).
    pub returning: Option<Vec<SelectItem>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    /// A `WITH` list written before the statement (SQL-mission-1 Slice C).
    pub ctes: Vec<Cte>,
    pub table: ObjectName,
    pub selection: Option<Expr>,
    /// SQL Phase 2a: `RETURNING <select list>` (the deleted rows' values).
    pub returning: Option<Vec<SelectItem>>,
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
    /// Foreign keys: table-level `FOREIGN KEY (...) REFERENCES ...` and inline
    /// `col TYPE REFERENCES ...` (folded to a one-column key), in source order.
    pub foreign_keys: Vec<ForeignKeyDef>,
    /// `CHECK (...)`: table-level and column-level, in source order.
    pub checks: Vec<CheckDef>,
}

/// `[CONSTRAINT name] CHECK (expression)`. `text` is the expression as SQL text, the
/// form the catalog stores; it is parsed and bound again whenever a row is written.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckDef {
    pub name: Option<Ident>,
    pub expr: Expr,
    pub text: String,
}

/// `ON DELETE` / `ON UPDATE` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferentialAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

/// A foreign key as written: `[CONSTRAINT name] FOREIGN KEY (cols) REFERENCES
/// table [(cols)] [ON DELETE action] [ON UPDATE action]`.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKeyDef {
    pub name: Option<Ident>,
    pub columns: Vec<Ident>,
    pub foreign_table: ObjectName,
    /// Empty = the referenced table's primary key.
    pub referred_columns: Vec<Ident>,
    pub on_delete: Option<ReferentialAction>,
    pub on_update: Option<ReferentialAction>,
}

/// The one change an `ALTER TABLE` statement makes. A statement carries exactly
/// one: the catalog applies a change atomically, and several in a row would not be
/// (`ADR-DDL-FK-01`).
#[derive(Debug, Clone, PartialEq)]
pub enum AlterOperation {
    AddColumn {
        column: ColumnDef,
        if_not_exists: bool,
    },
    RenameColumn {
        from: Ident,
        to: Ident,
    },
    SetDefault {
        column: Ident,
        value: Expr,
    },
    DropDefault {
        column: Ident,
    },
    SetNotNull {
        column: Ident,
    },
    DropNotNull {
        column: Ident,
    },
    AddForeignKey(ForeignKeyDef),
    AddCheck(CheckDef),
    /// `ADD [CONSTRAINT name] UNIQUE (cols)` -- a unique index.
    AddUnique {
        name: Option<Ident>,
        columns: Vec<Ident>,
    },
    /// `DROP CONSTRAINT name` -- a foreign key, a `CHECK`, or a unique constraint (index).
    DropConstraint {
        name: Ident,
        if_exists: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterTable {
    pub name: ObjectName,
    pub if_exists: bool,
    pub operation: AlterOperation,
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

/// `USE SCHEMA [db.]schema` -- a session command: it changes which schema unqualified names resolve
/// against, nothing else. The API/CLI/IDE carry the chosen schema from request to request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseSchema {
    /// `schema` or `db.schema`.
    pub name: ObjectName,
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
    Select(Box<Select>),
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
    /// `USE SCHEMA` -- PARSED. Resolved by the caller before binding (`bind::scope::resolve_use_schema`);
    /// binding it is a controlled refusal, never a statement the planner or executor sees.
    UseSchema(UseSchema),
    /// `CREATE TABLE` — PARSED, BOUND. NOT EXECUTABLE YET (the bound
    /// form is shaped to call `CatalogService::create_table` directly,
    /// item 25 — but this crate never calls it).
    CreateTable(CreateTable),
    /// `DROP TABLE` — PARSED, BOUND. NOT EXECUTABLE YET.
    DropTable(DropTable),
    /// `ALTER TABLE` -- one operation per statement (`AlterOperation`).
    AlterTable(AlterTable),
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
