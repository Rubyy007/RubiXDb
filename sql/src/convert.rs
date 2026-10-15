//! Converts `sqlparser::ast` (the third-party parse tree) into RubiXDB's
//! own internal `crate::ast` (item 4/12). This is the **only** module
//! that ever names a `sqlparser::ast` type — everything downstream
//! (`crate::bind`, and any future planner/executor) depends solely on
//! `crate::ast`/`crate::bound`. Every sqlparser construct outside the
//! approved grammar subset (`PHASE_RELATIONAL_SQL_GRAMMAR.md`) is
//! rejected here with `SqlError::Unsupported`, never silently dropped
//! (item 12: "do not discard... unless the current architecture
//! explicitly says they are unsupported").

use sqlparser::ast as sp;

use crate::ast::*;
use crate::error::{Result, SqlError};
use crate::limits::SqlLimits;

fn unsupported(detail: impl Into<String>) -> SqlError {
    SqlError::Unsupported {
        detail: detail.into(),
    }
}

/// Depth-limits AST conversion independently of `sqlparser`'s own
/// `RecursionLimitExceeded` guard (`crate::limits::SqlLimits::max_
/// expression_depth`'s own doc comment explains why both exist).
struct DepthGuard<'a> {
    limits: &'a SqlLimits,
}

impl<'a> DepthGuard<'a> {
    fn check(&self, depth: usize) -> Result<()> {
        if depth > self.limits.max_expression_depth {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "expression nesting exceeds max_expression_depth ({})",
                    self.limits.max_expression_depth
                ),
            });
        }
        Ok(())
    }
}

pub fn convert_statement(stmt: &sp::Statement, limits: &SqlLimits) -> Result<Statement> {
    let dg = DepthGuard { limits };
    match stmt {
        sp::Statement::Query(q) => convert_query_statement(q, &dg),
        sp::Statement::Insert(insert) => Ok(Statement::Insert(convert_insert(insert, &dg)?)),
        sp::Statement::Update(update) => Ok(Statement::Update(convert_update(update, &dg)?)),
        sp::Statement::Delete(delete) => Ok(Statement::Delete(convert_delete(delete, &dg)?)),
        sp::Statement::CreateTable(ct) => {
            Ok(Statement::CreateTable(convert_create_table(ct, limits)?))
        }
        sp::Statement::CreateIndex(ci) => Ok(Statement::CreateIndex(convert_create_index(ci)?)),
        sp::Statement::CreateSchema {
            schema_name,
            or_replace,
            if_not_exists,
            ..
        } => {
            if *or_replace {
                return Err(unsupported("CREATE OR REPLACE SCHEMA"));
            }
            let name = match schema_name {
                sp::SchemaName::Simple(name) => convert_object_name(name, limits)?,
                _ => return Err(unsupported("CREATE SCHEMA ... AUTHORIZATION")),
            };
            Ok(Statement::CreateSchema(CreateSchema {
                name,
                if_not_exists: *if_not_exists,
            }))
        }
        sp::Statement::Use(u) => match u {
            sp::Use::Schema(name) => {
                let name = convert_object_name(name, limits)?;
                if name.0.len() > 2 {
                    return Err(SqlError::InvalidIdentifier {
                        detail: format!(
                            "USE SCHEMA name has {} parts; use schema or default.schema",
                            name.0.len()
                        ),
                    });
                }
                Ok(Statement::UseSchema(UseSchema { name }))
            }
            sp::Use::Database(_) | sp::Use::Catalog(_) => Err(SqlError::UnsupportedExecution {
                detail: "USE DATABASE is disabled. Active database is locked to 'default'. Use USE SCHEMA <schema_name> to switch working namespaces."
                    .to_string(),
            }),
            sp::Use::Object(_) | sp::Use::Default => Err(SqlError::UnsupportedExecution {
                detail: "USE needs a target. Use USE SCHEMA <schema_name> to switch working namespaces."
                    .to_string(),
            }),
            _ => Err(unsupported("this USE form")),
        },
        sp::Statement::CreateDatabase {
            db_name,
            if_not_exists,
            or_replace,
            ..
        } => {
            if *or_replace {
                return Err(unsupported("CREATE OR REPLACE DATABASE"));
            }
            let name = convert_object_name(db_name, limits)?;
            if name.0.len() != 1 {
                return Err(SqlError::InvalidIdentifier {
                    detail: "CREATE DATABASE name must not be qualified".to_string(),
                });
            }
            Ok(Statement::CreateDatabase(CreateDatabase {
                name: name.0.into_iter().next().expect("checked len 1"),
                if_not_exists: *if_not_exists,
            }))
        }
        sp::Statement::Drop {
            object_type,
            if_exists,
            names,
            cascade,
            restrict: _,
            purge,
            temporary,
            table,
        } => {
            if *cascade {
                return Err(unsupported("DROP ... CASCADE"));
            }
            if *purge || *temporary {
                return Err(unsupported("DROP with dialect-specific modifiers"));
            }
            if names.len() != 1 {
                return Err(unsupported("DROP with more than one object name"));
            }
            match object_type {
                sp::ObjectType::Table => {
                    if table.is_some() {
                        return Err(unsupported("DROP TABLE ... ON ..."));
                    }
                    let name = convert_object_name(&names[0], limits)?;
                    Ok(Statement::DropTable(DropTable {
                        name,
                        if_exists: *if_exists,
                    }))
                }
                sp::ObjectType::Index => {
                    let table_name = table.as_ref().ok_or_else(|| {
                        unsupported(
                            "DROP INDEX requires 'ON <table>' (index names are table-scoped)",
                        )
                    })?;
                    let index_ident_name = convert_object_name(&names[0], limits)?;
                    if index_ident_name.0.len() != 1 {
                        return Err(SqlError::InvalidIdentifier {
                            detail: "index name must not be qualified".to_string(),
                        });
                    }
                    Ok(Statement::DropIndex(DropIndex {
                        table: convert_object_name(table_name, limits)?,
                        name: index_ident_name
                            .0
                            .into_iter()
                            .next()
                            .expect("checked len 1"),
                        if_exists: *if_exists,
                    }))
                }
                other => Err(unsupported(format!("DROP {other:?}"))),
            }
        }
        sp::Statement::AlterTable(at) => Ok(Statement::AlterTable(convert_alter_table(at, limits)?)),
        sp::Statement::Explain {
            statement,
            analyze,
            verbose,
            query_plan,
            estimate,
            format,
            options,
            describe_alias: _,
        } => {
            if *analyze
                || *verbose
                || *query_plan
                || *estimate
                || format.is_some()
                || options.is_some()
            {
                return Err(unsupported(
                    "EXPLAIN with modifiers (ANALYZE/VERBOSE/FORMAT/...)",
                ));
            }
            let inner = convert_statement(statement, limits)?;
            Ok(Statement::Explain(Explain {
                statement: Box::new(inner),
            }))
        }
        sp::Statement::StartTransaction {
            modes,
            transaction: _,
            modifier,
            statements,
            ..
        } => {
            if !modes.is_empty() || modifier.is_some() || !statements.is_empty() {
                return Err(unsupported(
                    "BEGIN/START TRANSACTION with modes/modifiers/a statement block",
                ));
            }
            Ok(Statement::Begin)
        }
        sp::Statement::Commit {
            chain,
            end: _,
            modifier,
        } => {
            if *chain || modifier.is_some() {
                return Err(unsupported("COMMIT AND CHAIN / modifiers"));
            }
            Ok(Statement::Commit)
        }
        sp::Statement::Rollback { chain, savepoint } => {
            if *chain || savepoint.is_some() {
                return Err(unsupported("ROLLBACK AND CHAIN / TO SAVEPOINT"));
            }
            Ok(Statement::Rollback)
        }
        other => Err(unsupported(format!(
            "statement kind not in the approved grammar subset: {}",
            statement_kind_name(other)
        ))),
    }
}

/// A short, safe (never includes the full statement text/values) label
/// for an unsupported statement — used only in the `Unsupported` error's
/// own detail text.
fn statement_kind_name(stmt: &sp::Statement) -> &'static str {
    match stmt {
        sp::Statement::Merge { .. } => "MERGE",
        sp::Statement::CreateView { .. } => "CREATE VIEW",
        sp::Statement::AlterTable { .. } => "ALTER TABLE",
        sp::Statement::Grant { .. } => "GRANT",
        sp::Statement::Revoke { .. } => "REVOKE",
        sp::Statement::Savepoint { .. } => "SAVEPOINT",
        _ => "unrecognized/dialect-specific statement",
    }
}

// ---------------------------------------------------------------------
// SELECT
// ---------------------------------------------------------------------

/// `sp::Statement::Query`: a plain `SELECT` (possibly with a `WITH`), or --
/// when sqlparser reads `WITH ... DELETE|UPDATE|INSERT` as a query whose body
/// is the DML statement -- that statement with its `WITH` list attached.
fn convert_query_statement(q: &sp::Query, dg: &DepthGuard) -> Result<Statement> {
    let dml = match q.body.as_ref() {
        sp::SetExpr::Delete(s) | sp::SetExpr::Update(s) | sp::SetExpr::Insert(s) => Some(s),
        _ => None,
    };
    let Some(inner) = dml else {
        return Ok(Statement::Select(Box::new(convert_query(q, dg, 0)?)));
    };
    if q.order_by.is_some()
        || q.limit_clause.is_some()
        || q.fetch.is_some()
        || !q.locks.is_empty()
        || q.for_clause.is_some()
    {
        return Err(unsupported(
            "ORDER BY/LIMIT/FETCH/FOR on a data-modification statement",
        ));
    }
    let ctes = convert_with(q.with.as_ref(), dg, 0)?;
    match inner {
        sp::Statement::Delete(d) => {
            let mut d = convert_delete(d, dg)?;
            d.ctes = ctes;
            Ok(Statement::Delete(d))
        }
        sp::Statement::Update(u) => {
            let mut u = convert_update(u, dg)?;
            u.ctes = ctes;
            Ok(Statement::Update(u))
        }
        sp::Statement::Insert(i) => {
            let mut i = convert_insert(i, dg)?;
            i.ctes = ctes;
            Ok(Statement::Insert(i))
        }
        _ => Err(unsupported(
            "statement kind not in the approved grammar subset",
        )),
    }
}

/// A `WITH [RECURSIVE]` list (SQL-mission-1 Slices C and D). Only definitions
/// with a plain name, an optional plain column-name list and a query body are
/// accepted; everything else is refused, never dropped.
fn convert_with(with: Option<&sp::With>, dg: &DepthGuard, depth: usize) -> Result<Vec<Cte>> {
    let Some(with) = with else {
        return Ok(Vec::new());
    };
    with.cte_tables
        .iter()
        .map(|cte| {
            if cte.materialized.is_some() {
                return Err(unsupported("WITH ... AS [NOT] MATERIALIZED"));
            }
            if cte.from.is_some() {
                return Err(unsupported("WITH ... FROM"));
            }
            let columns = cte
                .alias
                .columns
                .iter()
                .map(|c| {
                    if c.data_type.is_some() {
                        return Err(unsupported("typed column in a CTE column list"));
                    }
                    convert_ident(&c.name, dg.limits)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Cte {
                name: convert_ident(&cte.alias.name, dg.limits)?,
                columns,
                query: Box::new(convert_query(&cte.query, dg, depth + 1)?),
                recursive: with.recursive,
            })
        })
        .collect()
}

fn convert_query(q: &sp::Query, dg: &DepthGuard, depth: usize) -> Result<Select> {
    dg.check(depth)?;
    let ctes = convert_with(q.with.as_ref(), dg, depth)?;
    if q.fetch.is_some() || !q.locks.is_empty() || q.for_clause.is_some() {
        return Err(unsupported("FETCH/FOR UPDATE/FOR XML|JSON clauses"));
    }
    match q.body.as_ref() {
        sp::SetExpr::Select(select) => convert_select_core(
            select,
            q.order_by.as_ref(),
            q.limit_clause.as_ref(),
            ctes,
            dg,
            depth,
        ),
        // SQL-mission-1 Slice E: `a UNION b [ORDER BY ...] [LIMIT ...]`. The
        // trailing `ORDER BY` / `LIMIT` belong to the whole set operation.
        body @ sp::SetExpr::SetOperation { .. } => {
            let mut shell = convert_set_operation(body, dg, depth)?;
            shell.ctes = ctes;
            shell.order_by = convert_order_by(q.order_by.as_ref(), dg, depth)?;
            let (limit, offset) = convert_limit_clause(q.limit_clause.as_ref(), dg, depth)?;
            shell.limit = limit;
            shell.offset = offset;
            Ok(shell)
        }
        _ => Err(unsupported(
            "query body must be a SELECT or a UNION/INTERSECT/EXCEPT of queries (no VALUES/TABLE/subquery-as-body)",
        )),
    }
}

/// One operand of a set operation: a plain `SELECT` (it cannot carry its own
/// `ORDER BY` / `LIMIT` -- those would belong to the whole operation), a
/// parenthesised query (which can), or a nested set operation.
fn convert_set_operand(expr: &sp::SetExpr, dg: &DepthGuard, depth: usize) -> Result<Select> {
    dg.check(depth)?;
    match expr {
        sp::SetExpr::Select(select) => {
            convert_select_core(select, None, None, Vec::new(), dg, depth + 1)
        }
        sp::SetExpr::Query(q) => convert_query(q, dg, depth + 1),
        body @ sp::SetExpr::SetOperation { .. } => convert_set_operation(body, dg, depth + 1),
        _ => Err(unsupported(
            "a set-operation operand must be a SELECT or a parenthesised query (no VALUES/TABLE)",
        )),
    }
}

/// A `SetExpr::SetOperation` tree as an `ast::Select` shell. The **left
/// spine** (`a UNION b UNION c` is `(a UNION b) UNION c`) is walked with a
/// loop, not recursion, so a long chain costs no native stack here; the
/// chain's length is bounded by `max_expression_depth` instead.
fn convert_set_operation(body: &sp::SetExpr, dg: &DepthGuard, depth: usize) -> Result<Select> {
    let mut spine: Vec<(&sp::SetOperator, &sp::SetQuantifier, &sp::SetExpr)> = Vec::new();
    let mut cursor = body;
    while let sp::SetExpr::SetOperation {
        left,
        op,
        set_quantifier,
        right,
    } = cursor
    {
        dg.check(depth + spine.len())?;
        spine.push((op, set_quantifier, right.as_ref()));
        cursor = left.as_ref();
    }
    let mut acc = convert_set_operand(cursor, dg, depth + spine.len())?;
    for (op, quantifier, right) in spine.into_iter().rev() {
        let op = match op {
            sp::SetOperator::Union => SetOperator::Union,
            sp::SetOperator::Intersect => SetOperator::Intersect,
            sp::SetOperator::Except => SetOperator::Except,
            // Snowflake's name for EXCEPT; the two are interchangeable.
            sp::SetOperator::Minus => SetOperator::Except,
        };
        let (all, by_name) = match quantifier {
            sp::SetQuantifier::None | sp::SetQuantifier::Distinct => (false, false),
            sp::SetQuantifier::All => (true, false),
            sp::SetQuantifier::ByName | sp::SetQuantifier::DistinctByName => (false, true),
            sp::SetQuantifier::AllByName => (true, true),
        };
        if by_name && op != SetOperator::Union {
            return Err(unsupported(
                "BY NAME with INTERSECT / EXCEPT / MINUS (only UNION has it)",
            ));
        }
        let right = convert_set_operand(right, dg, depth + 1)?;
        acc = Select::set_operation_with(op, all, by_name, acc, right);
    }
    Ok(acc)
}

fn convert_order_by(
    order_by: Option<&sp::OrderBy>,
    dg: &DepthGuard,
    depth: usize,
) -> Result<Vec<OrderByItem>> {
    match order_by {
        None => Ok(Vec::new()),
        Some(sp::OrderBy {
            kind: sp::OrderByKind::Expressions(items),
            interpolate: None,
        }) => items
            .iter()
            .map(|item| convert_order_by_item(item, dg, depth + 1))
            .collect(),
        Some(_) => Err(unsupported("ORDER BY ALL / WITH INTERPOLATE")),
    }
}

fn convert_limit_clause(
    limit_clause: Option<&sp::LimitClause>,
    dg: &DepthGuard,
    depth: usize,
) -> Result<(Option<Expr>, Option<Expr>)> {
    match limit_clause {
        None => Ok((None, None)),
        Some(sp::LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        }) => {
            if !limit_by.is_empty() {
                return Err(unsupported("LIMIT ... BY"));
            }
            let limit = limit
                .as_ref()
                .map(|e| convert_expr(e, dg, depth + 1))
                .transpose()?;
            let offset = offset
                .as_ref()
                .map(|o| convert_expr(&o.value, dg, depth + 1))
                .transpose()?;
            Ok((limit, offset))
        }
        Some(sp::LimitClause::OffsetCommaLimit { .. }) => {
            Err(unsupported("MySQL LIMIT offset, limit syntax"))
        }
    }
}

/// A plain `SELECT ... [ORDER BY ...] [LIMIT ...]` (the query-level clauses
/// are passed in: `sqlparser` keeps them on the enclosing `Query`, not on the
/// `Select`).
fn convert_select_core(
    select: &sp::Select,
    order_by_clause: Option<&sp::OrderBy>,
    limit_clause: Option<&sp::LimitClause>,
    ctes: Vec<Cte>,
    dg: &DepthGuard,
    depth: usize,
) -> Result<Select> {
    if select.top.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.value_table_mode.is_some()
        || select.exclude.is_some()
    {
        return Err(unsupported(
            "SELECT with TOP/INTO/LATERAL/PREWHERE/CONNECT BY/CLUSTER BY/DISTRIBUTE BY/SORT BY/value-table-mode/EXCLUDE",
        ));
    }
    let mut group_by_all = false;
    let (group_by, grouping) = match &select.group_by {
        sp::GroupByExpr::All(modifiers) => {
            if !modifiers.is_empty() {
                return Err(unsupported(
                    "GROUP BY ALL with WITH ROLLUP / WITH CUBE / WITH TOTALS modifiers",
                ));
            }
            group_by_all = true;
            (Vec::new(), None)
        }
        sp::GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(unsupported(
                    "GROUP BY with WITH ROLLUP / WITH CUBE / WITH TOTALS modifiers (use ROLLUP(...), CUBE(...) or GROUPING SETS(...))",
                ));
            }
            if exprs.len() > dg.limits.max_columns {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "GROUP BY clause exceeds max_columns ({})",
                        dg.limits.max_columns
                    ),
                });
            }
            let extended = exprs.iter().any(|e| {
                matches!(
                    e,
                    sp::Expr::Rollup(_)
                        | sp::Expr::Cube(_)
                        | sp::Expr::GroupingSets(_)
                        | sp::Expr::Tuple(_)
                )
            });
            if extended {
                let elements = exprs
                    .iter()
                    .map(|e| convert_group_element(e, dg, depth + 1))
                    .collect::<Result<Vec<_>>>()?;
                (Vec::new(), Some(elements))
            } else {
                let plain = exprs
                    .iter()
                    .map(|e| convert_expr(e, dg, depth + 1))
                    .collect::<Result<Vec<_>>>()?;
                (plain, None)
            }
        }
    };

    let named_windows = select
        .named_window
        .iter()
        .map(|def| convert_named_window(def, dg, depth + 1))
        .collect::<Result<Vec<_>>>()?;
    let qualify = select
        .qualify
        .as_ref()
        .map(|e| convert_expr(e, dg, depth + 1))
        .transpose()?;

    let distinct = match &select.distinct {
        None | Some(sp::Distinct::All) => false,
        Some(sp::Distinct::Distinct) => true,
        Some(sp::Distinct::On(_)) => return Err(unsupported("DISTINCT ON (...)")),
    };

    let projection = select
        .projection
        .iter()
        .map(|item| convert_select_item(item, dg, depth + 1))
        .collect::<Result<Vec<_>>>()?;

    let from = if select.from.is_empty() {
        None
    } else if select.from.len() == 1 {
        Some(convert_table_with_joins(&select.from[0], dg, depth + 1)?)
    } else {
        return Err(unsupported(
            "multiple comma-separated FROM items (implicit CROSS JOIN)",
        ));
    };

    let selection = select
        .selection
        .as_ref()
        .map(|e| convert_expr(e, dg, depth + 1))
        .transpose()?;

    let having = select
        .having
        .as_ref()
        .map(|e| convert_expr(e, dg, depth + 1))
        .transpose()?;

    let order_by = convert_order_by(order_by_clause, dg, depth)?;
    let (limit, offset) = convert_limit_clause(limit_clause, dg, depth)?;

    Ok(Select {
        ctes,
        set_op: None,
        distinct,
        projection,
        from,
        selection,
        group_by,
        group_by_all,
        grouping,
        having,
        qualify,
        named_windows,
        order_by,
        limit,
        offset,
    })
}

/// One element of a `GROUP BY` that uses `ROLLUP` / `CUBE` / `GROUPING SETS`.
fn convert_group_element(e: &sp::Expr, dg: &DepthGuard, depth: usize) -> Result<GroupElement> {
    dg.check(depth)?;
    let members = |sets: &[Vec<sp::Expr>]| -> Result<Vec<Vec<Expr>>> {
        sets.iter()
            .map(|set| {
                set.iter()
                    .map(|e| convert_expr(e, dg, depth + 1))
                    .collect::<Result<Vec<_>>>()
            })
            .collect()
    };
    Ok(match e {
        sp::Expr::Rollup(sets) => GroupElement::Rollup(members(sets)?),
        sp::Expr::Cube(sets) => GroupElement::Cube(members(sets)?),
        sp::Expr::GroupingSets(sets) => GroupElement::Sets(members(sets)?),
        // `GROUP BY ()` is the empty grouping set; `GROUP BY (a, b)` is `a, b`.
        sp::Expr::Tuple(es) => GroupElement::Sets(members(std::slice::from_ref(es))?),
        other => GroupElement::Expr(convert_expr(other, dg, depth + 1)?),
    })
}

fn convert_named_window(
    def: &sp::NamedWindowDefinition,
    dg: &DepthGuard,
    depth: usize,
) -> Result<(Ident, WindowSpec)> {
    let name = convert_ident(&def.0, dg.limits)?;
    let spec = match &def.1 {
        sp::NamedWindowExpr::WindowSpec(spec) => convert_window_spec(spec, dg, depth + 1)?,
        sp::NamedWindowExpr::NamedWindow(base) => WindowSpec {
            base: Some(convert_ident(base, dg.limits)?),
            ..WindowSpec::default()
        },
    };
    Ok((name, spec))
}

fn convert_window_spec(spec: &sp::WindowSpec, dg: &DepthGuard, depth: usize) -> Result<WindowSpec> {
    dg.check(depth)?;
    let max = dg.limits.max_columns;
    if spec.partition_by.len() > max || spec.order_by.len() > max {
        return Err(SqlError::ResourceLimit {
            detail: format!("a window's PARTITION BY / ORDER BY exceeds max_columns ({max})"),
        });
    }
    let base = spec
        .window_name
        .as_ref()
        .map(|n| convert_ident(n, dg.limits))
        .transpose()?;
    let partition_by = spec
        .partition_by
        .iter()
        .map(|e| convert_expr(e, dg, depth + 1))
        .collect::<Result<Vec<_>>>()?;
    let order_by = spec
        .order_by
        .iter()
        .map(|item| convert_order_by_item(item, dg, depth + 1))
        .collect::<Result<Vec<_>>>()?;
    let frame = spec
        .window_frame
        .as_ref()
        .map(|f| convert_frame(f, dg, depth + 1))
        .transpose()?;
    Ok(WindowSpec {
        base,
        partition_by,
        order_by,
        frame,
    })
}

fn convert_frame(f: &sp::WindowFrame, dg: &DepthGuard, depth: usize) -> Result<FrameSpec> {
    let units = match f.units {
        sp::WindowFrameUnits::Rows => FrameUnits::Rows,
        sp::WindowFrameUnits::Range => FrameUnits::Range,
        sp::WindowFrameUnits::Groups => FrameUnits::Groups,
    };
    let bound = |b: &sp::WindowFrameBound| -> Result<FrameBound> {
        Ok(match b {
            sp::WindowFrameBound::CurrentRow => FrameBound::CurrentRow,
            sp::WindowFrameBound::Preceding(None) => FrameBound::UnboundedPreceding,
            sp::WindowFrameBound::Following(None) => FrameBound::UnboundedFollowing,
            sp::WindowFrameBound::Preceding(Some(e)) => {
                FrameBound::Preceding(Box::new(convert_expr(e, dg, depth + 1)?))
            }
            sp::WindowFrameBound::Following(Some(e)) => {
                FrameBound::Following(Box::new(convert_expr(e, dg, depth + 1)?))
            }
        })
    };
    let start = bound(&f.start_bound)?;
    // `ROWS <start>` is the shorthand for `BETWEEN <start> AND CURRENT ROW`.
    let end = match &f.end_bound {
        Some(b) => bound(b)?,
        None => FrameBound::CurrentRow,
    };
    Ok(FrameSpec { units, start, end })
}

/// The functions that may be called with an `OVER` clause besides the
/// aggregates (SQL-mission-2).
const WINDOW_ONLY_FUNCTIONS: &[&str] = &[
    "row_number",
    "rank",
    "dense_rank",
    "ntile",
    "percent_rank",
    "cume_dist",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
];

/// `f(args) [FILTER (WHERE ...)] OVER (...)`.
fn convert_window_function(
    f: &sp::Function,
    name: &ObjectName,
    over: &sp::WindowType,
    dg: &DepthGuard,
    depth: usize,
) -> Result<Expr> {
    let ignore_nulls = matches!(f.null_treatment, Some(sp::NullTreatment::IgnoreNulls));
    let over = match over {
        sp::WindowType::NamedWindow(n) => WindowOver::Named(convert_ident(n, dg.limits)?),
        sp::WindowType::WindowSpec(spec) => {
            WindowOver::Spec(convert_window_spec(spec, dg, depth + 1)?)
        }
    };
    let not_a_window_function = || SqlError::TypeMismatch {
        detail: format!("{} is not a window function", name.last().value),
    };
    let function = if name.0.len() == 1 {
        let lower = name.0[0].value.to_ascii_lowercase();
        if let Some(func) = crate::aggregate::AggregateFunc::lookup(&lower) {
            let (arg, distinct, filter) = convert_aggregate_call(func, f, dg, depth)?;
            WindowFunction::Aggregate {
                func,
                arg,
                distinct,
                filter,
            }
        } else if WINDOW_ONLY_FUNCTIONS.contains(&lower.as_str()) {
            if f.filter.is_some() {
                return Err(unsupported(
                    "FILTER on a window function that is not an aggregate",
                ));
            }
            let args = match &f.args {
                sp::FunctionArguments::None => Vec::new(),
                sp::FunctionArguments::List(list) => {
                    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
                        return Err(unsupported(
                            "DISTINCT / ORDER BY / IGNORE NULLS inside a window function's argument list",
                        ));
                    }
                    if list.args.len() > dg.limits.max_columns {
                        return Err(SqlError::ResourceLimit {
                            detail: "window function call has too many arguments".to_string(),
                        });
                    }
                    list.args
                        .iter()
                        .map(|a| match a {
                            sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Expr(e)) => {
                                convert_expr(e, dg, depth + 1)
                            }
                            _ => Err(unsupported("named/wildcard window function arguments")),
                        })
                        .collect::<Result<Vec<_>>>()?
                }
                sp::FunctionArguments::Subquery(_) => {
                    return Err(unsupported("function call with a bare subquery argument"))
                }
            };
            WindowFunction::Named { name: lower, args }
        } else {
            return Err(not_a_window_function());
        }
    } else {
        return Err(not_a_window_function());
    };
    Ok(Expr::Window(Box::new(WindowCall {
        function,
        over,
        ignore_nulls,
    })))
}

/// The argument, `DISTINCT` flag and `FILTER` clause of an aggregate call
/// (shared by plain aggregates and aggregates used as window functions).
#[allow(clippy::type_complexity)]
fn convert_aggregate_call(
    agg_func: crate::aggregate::AggregateFunc,
    f: &sp::Function,
    dg: &DepthGuard,
    depth: usize,
) -> Result<(
    crate::aggregate::AggregateArg<Box<Expr>>,
    bool,
    Option<Box<Expr>>,
)> {
    let filter = f
        .filter
        .as_ref()
        .map(|e| convert_expr(e, dg, depth + 1).map(Box::new))
        .transpose()?;
    match &f.args {
        sp::FunctionArguments::None => Err(SqlError::TypeMismatch {
            detail: format!(
                "aggregate function {} requires an argument",
                agg_func.name()
            ),
        }),
        sp::FunctionArguments::Subquery(_) => {
            Err(unsupported("function call with a bare subquery argument"))
        }
        sp::FunctionArguments::List(list) => {
            if !list.clauses.is_empty() {
                return Err(unsupported(
                    "aggregate function with ORDER BY / LIMIT / IGNORE NULLS clauses inside the argument list",
                ));
            }
            let distinct = matches!(
                list.duplicate_treatment,
                Some(sp::DuplicateTreatment::Distinct)
            );
            if list.args.is_empty() {
                return Err(SqlError::TypeMismatch {
                    detail: format!(
                        "aggregate function {} requires an argument",
                        agg_func.name()
                    ),
                });
            }
            if list.args.len() > 1 {
                // `COUNT(a, b, ...)` / `COUNT(DISTINCT a, b, ...)`: the only
                // aggregate that takes several expressions (Snowflake).
                if agg_func != crate::aggregate::AggregateFunc::Count {
                    return Err(SqlError::TypeMismatch {
                        detail: format!(
                            "aggregate function {} requires exactly 1 argument, found {}",
                            agg_func.name(),
                            list.args.len()
                        ),
                    });
                }
                if list.args.len() > dg.limits.max_columns {
                    return Err(SqlError::ResourceLimit {
                        detail: "COUNT has too many arguments".to_string(),
                    });
                }
                let exprs = list
                    .args
                    .iter()
                    .map(|a| match a {
                        sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Expr(e)) => {
                            convert_expr(e, dg, depth + 1).map(Box::new)
                        }
                        sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Wildcard) => {
                            Err(SqlError::TypeMismatch {
                                detail:
                                    "COUNT(*) takes no other argument; name the columns to count"
                                        .to_string(),
                            })
                        }
                        _ => Err(unsupported(
                            "named or qualified wildcard function arguments",
                        )),
                    })
                    .collect::<Result<Vec<_>>>()?;
                return Ok((
                    crate::aggregate::AggregateArg::Multi(exprs),
                    distinct,
                    filter,
                ));
            }
            match &list.args[0] {
                sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Wildcard) => {
                    if agg_func != crate::aggregate::AggregateFunc::Count {
                        Err(SqlError::TypeMismatch {
                            detail: format!(
                                "aggregate function {} does not support wildcard argument (*)",
                                agg_func.name()
                            ),
                        })
                    } else if distinct {
                        Err(SqlError::TypeMismatch {
                            detail: "COUNT(DISTINCT *) is not valid; name the column to count"
                                .to_string(),
                        })
                    } else {
                        Ok((crate::aggregate::AggregateArg::Wildcard, false, filter))
                    }
                }
                sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Expr(e)) => {
                    let arg_expr = convert_expr(e, dg, depth + 1)?;
                    Ok((
                        crate::aggregate::AggregateArg::Expr(Box::new(arg_expr)),
                        distinct,
                        filter,
                    ))
                }
                _ => Err(unsupported(
                    "named or qualified wildcard function arguments",
                )),
            }
        }
    }
}

/// SQL-mission-1: a nested query (scalar / `EXISTS` / `IN` subquery) — the
/// same `convert_query` a top-level statement uses, so a subquery accepts
/// exactly the grammar a plain `SELECT` does and rejects exactly the same
/// constructs.
fn convert_subquery(q: &sp::Query, dg: &DepthGuard, depth: usize) -> Result<QueryExpr> {
    Ok(QueryExpr::Select(Box::new(convert_query(q, dg, depth)?)))
}

fn convert_select_item(item: &sp::SelectItem, dg: &DepthGuard, depth: usize) -> Result<SelectItem> {
    dg.check(depth)?;
    match item {
        sp::SelectItem::UnnamedExpr(e) => Ok(SelectItem::Item(SelectItemExpr {
            expr: convert_expr(e, dg, depth + 1)?,
            alias: None,
        })),
        sp::SelectItem::ExprWithAlias { expr, alias } => Ok(SelectItem::Item(SelectItemExpr {
            expr: convert_expr(expr, dg, depth + 1)?,
            alias: Some(convert_ident(alias, dg.limits)?),
        })),
        sp::SelectItem::Wildcard(opts) => {
            if !is_default_wildcard_options(opts) {
                return Err(unsupported("* with EXCLUDE/REPLACE/RENAME options"));
            }
            Ok(SelectItem::Wildcard)
        }
        sp::SelectItem::QualifiedWildcard(kind, opts) => {
            if !is_default_wildcard_options(opts) {
                return Err(unsupported(
                    "qualified * with EXCLUDE/REPLACE/RENAME options",
                ));
            }
            match kind {
                sp::SelectItemQualifiedWildcardKind::ObjectName(name) => Ok(
                    SelectItem::QualifiedWildcard(convert_object_name(name, dg.limits)?),
                ),
                sp::SelectItemQualifiedWildcardKind::Expr(_) => {
                    Err(unsupported("wildcard on an arbitrary expression"))
                }
            }
        }
        sp::SelectItem::ExprWithAliases { .. } => {
            Err(unsupported("expression with multiple aliases"))
        }
    }
}

fn is_default_wildcard_options(opts: &sp::WildcardAdditionalOptions) -> bool {
    opts.opt_exclude.is_none()
        && opts.opt_except.is_none()
        && opts.opt_rename.is_none()
        && opts.opt_replace.is_none()
        && opts.opt_ilike.is_none()
}

fn convert_table_with_joins(
    t: &sp::TableWithJoins,
    dg: &DepthGuard,
    depth: usize,
) -> Result<FromClause> {
    dg.check(depth)?;
    let first = convert_table_factor(&t.relation, dg, depth + 1)?;
    let joins = t
        .joins
        .iter()
        .map(|j| convert_join(j, dg, depth + 1))
        .collect::<Result<Vec<_>>>()?;
    Ok(FromClause { first, joins })
}

fn convert_table_factor(tf: &sp::TableFactor, dg: &DepthGuard, depth: usize) -> Result<TableRef> {
    dg.check(depth)?;
    let limits = dg.limits;
    match tf {
        sp::TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            partitions,
            json_path,
            sample: _,
            with_ordinality,
            ..
        } => {
            if args.is_some()
                || !with_hints.is_empty()
                || version.is_some()
                || !partitions.is_empty()
                || json_path.is_some()
                || *with_ordinality
            {
                return Err(unsupported(
                    "table reference with function args/hints/version/partitions/ordinality",
                ));
            }
            let (alias, column_aliases) = match alias {
                Some(a) => {
                    let (name, columns) = convert_table_alias(a, limits)?;
                    (Some(name), columns)
                }
                None => (None, Vec::new()),
            };
            Ok(TableRef {
                name: convert_object_name(name, limits)?,
                alias,
                derived: None,
                lateral: false,
                column_aliases,
            })
        }
        sp::TableFactor::Derived {
            lateral,
            subquery,
            alias,
            sample,
        } => {
            if sample.is_some() {
                return Err(unsupported("TABLESAMPLE on a derived table"));
            }
            let body = convert_query(subquery, dg, depth + 1)?;
            let (alias, column_aliases) = match alias {
                Some(a) => {
                    let (name, columns) = convert_table_alias(a, limits)?;
                    (Some(name), columns)
                }
                None => (None, Vec::new()),
            };
            Ok(TableRef {
                name: ObjectName(alias.iter().cloned().collect()),
                alias,
                derived: Some(Box::new(body)),
                lateral: *lateral,
                column_aliases,
            })
        }
        other => Err(unsupported(format!("FROM item kind: {other:?}")).into_kind_only()),
    }
}

/// Trims a debug-formatted `Unsupported` detail down before it can ever
/// carry table/row *contents* — `TableFactor`'s `Debug` output only ever
/// contains grammar shape (variant names), never literal SQL values, but
/// this makes that invariant explicit and future-proof rather than
/// implicit.
trait IntoKindOnly {
    fn into_kind_only(self) -> SqlError;
}
impl IntoKindOnly for SqlError {
    fn into_kind_only(self) -> SqlError {
        match self {
            SqlError::Unsupported { detail } => SqlError::Unsupported {
                detail: detail
                    .split('(')
                    .next()
                    .unwrap_or(&detail)
                    .trim()
                    .to_string(),
            },
            other => other,
        }
    }
}

fn convert_table_alias(alias: &sp::TableAlias, limits: &SqlLimits) -> Result<(Ident, Vec<Ident>)> {
    let columns = alias
        .columns
        .iter()
        .map(|c| {
            if c.data_type.is_some() {
                return Err(unsupported("typed column in a table alias column list"));
            }
            convert_ident(&c.name, limits)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((convert_ident(&alias.name, limits)?, columns))
}

fn convert_join(j: &sp::Join, dg: &DepthGuard, depth: usize) -> Result<Join> {
    dg.check(depth)?;
    if j.global {
        return Err(unsupported("GLOBAL JOIN"));
    }
    let table = convert_table_factor(&j.relation, dg, depth + 1)?;
    let (kind, constraint) = match &j.join_operator {
        sp::JoinOperator::Join(c) | sp::JoinOperator::Inner(c) => (JoinKind::Inner, c),
        sp::JoinOperator::Left(c) | sp::JoinOperator::LeftOuter(c) => (JoinKind::Left, c),
        other => {
            return Err(unsupported(format!(
                "join kind not in the approved grammar subset (only INNER/LEFT): {other:?}"
            ))
            .into_kind_only())
        }
    };
    let on = match constraint {
        sp::JoinConstraint::On(e) => convert_expr(e, dg, depth + 1)?,
        sp::JoinConstraint::Natural => return Err(unsupported("NATURAL JOIN")),
        sp::JoinConstraint::Using(_) => return Err(unsupported("JOIN ... USING (...)")),
        sp::JoinConstraint::None => return Err(unsupported("JOIN without an ON condition")),
    };
    Ok(Join { kind, table, on })
}

fn convert_order_by_item(
    item: &sp::OrderByExpr,
    dg: &DepthGuard,
    depth: usize,
) -> Result<OrderByItem> {
    if item.with_fill.is_some() {
        return Err(unsupported("ORDER BY ... WITH FILL"));
    }
    let descending = match item.options.sort {
        None | Some(sp::OrderBySort::Asc) => false,
        Some(sp::OrderBySort::Desc) => true,
        Some(sp::OrderBySort::Using(_)) => {
            return Err(unsupported("ORDER BY ... USING <operator>"))
        }
    };
    Ok(OrderByItem {
        expr: convert_expr(&item.expr, dg, depth + 1)?,
        descending,
        nulls_first: item.options.nulls_first,
    })
}

// ---------------------------------------------------------------------
// INSERT / UPDATE / DELETE
// ---------------------------------------------------------------------

fn convert_insert(insert: &sp::Insert, dg: &DepthGuard) -> Result<Insert> {
    if insert.or.is_some()
        || insert.ignore
        || insert.overwrite
        || !insert.assignments.is_empty()
        || insert.partitioned.is_some()
        || !insert.after_columns.is_empty()
        || insert.replace_into
        || insert.priority.is_some()
        || insert.settings.is_some()
        || insert.format_clause.is_some()
    {
        return Err(unsupported(
            "INSERT with dialect-specific modifiers (OR/IGNORE/REPLACE/SET/PARTITION/...)",
        ));
    }
    if insert.output.is_some() {
        return Err(unsupported("INSERT ... OUTPUT"));
    }
    let sp::TableObject::TableName(table_name) = &insert.table else {
        return Err(unsupported("INSERT target must be a plain table name"));
    };
    let table = convert_object_name(table_name, dg.limits)?;

    let columns = if insert.columns.is_empty() {
        None
    } else {
        Some(
            insert
                .columns
                .iter()
                .map(|name| {
                    let converted = convert_object_name(name, dg.limits)?;
                    if converted.0.len() != 1 {
                        return Err(SqlError::InvalidIdentifier {
                            detail: "INSERT column list entries must not be qualified".to_string(),
                        });
                    }
                    Ok(converted.0.into_iter().next().expect("checked len 1"))
                })
                .collect::<Result<Vec<_>>>()?,
        )
    };
    if let Some(cols) = &columns {
        if cols.len() > dg.limits.max_columns {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "INSERT column list exceeds max_columns ({})",
                    dg.limits.max_columns
                ),
            });
        }
    }

    let source = insert
        .source
        .as_ref()
        .ok_or_else(|| unsupported("INSERT ... DEFAULT VALUES / SET form"))?;
    let table_alias = insert
        .table_alias
        .as_ref()
        .map(|a| convert_ident(&a.alias, dg.limits))
        .transpose()?;
    let on_conflict = insert
        .on
        .as_ref()
        .map(|on| convert_on_insert(on, dg))
        .transpose()?;
    let returning = insert
        .returning
        .as_ref()
        .map(|items| convert_returning(items, dg))
        .transpose()?;
    // `INSERT ... SELECT`: any query body but a bare `VALUES` list.
    if !matches!(source.body.as_ref(), sp::SetExpr::Values(_)) {
        let query = convert_query(source, dg, 1)?;
        return Ok(Insert {
            ctes: Vec::new(),
            table,
            table_alias,
            columns,
            rows: Vec::new(),
            query: Some(Box::new(query)),
            on_conflict,
            returning,
        });
    }
    if source.with.is_some() || source.order_by.is_some() || source.limit_clause.is_some() {
        return Err(unsupported("INSERT ... VALUES with WITH/ORDER BY/LIMIT"));
    }
    let sp::SetExpr::Values(values) = source.body.as_ref() else {
        unreachable!("checked above: the body is a VALUES list");
    };
    if values.rows.len() > dg.limits.max_values_rows {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "VALUES row count exceeds max_values_rows ({})",
                dg.limits.max_values_rows
            ),
        });
    }
    let rows = values
        .rows
        .iter()
        .map(|row| {
            if row.content.len() > dg.limits.max_columns {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "VALUES row width exceeds max_columns ({})",
                        dg.limits.max_columns
                    ),
                });
            }
            row.content
                .iter()
                .map(|e| convert_expr(e, dg, 1))
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Insert {
        ctes: Vec::new(),
        table,
        table_alias,
        columns,
        rows,
        query: None,
        on_conflict,
        returning,
    })
}

/// `RETURNING <select list>` of an `INSERT` / `UPDATE` / `DELETE`.
fn convert_returning(
    items: &[sp::SelectItem],
    dg: &DepthGuard,
) -> Result<Vec<SelectItem>> {
    if items.len() > dg.limits.max_columns {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "RETURNING list exceeds max_columns ({})",
                dg.limits.max_columns
            ),
        });
    }
    items
        .iter()
        .map(|item| convert_select_item(item, dg, 1))
        .collect()
}

/// `ON CONFLICT [target] DO NOTHING | DO UPDATE SET ... [WHERE ...]` (PostgreSQL 18).
/// MySQL's `ON DUPLICATE KEY UPDATE` is a different statement and stays refused.
fn convert_on_insert(on: &sp::OnInsert, dg: &DepthGuard) -> Result<OnConflict> {
    let sp::OnInsert::OnConflict(oc) = on else {
        return Err(unsupported(
            "ON DUPLICATE KEY UPDATE (use ON CONFLICT ... DO UPDATE)",
        ));
    };
    let target = match &oc.conflict_target {
        None => None,
        Some(sp::ConflictTarget::Columns(cols)) => {
            if cols.is_empty() || cols.len() > dg.limits.max_columns {
                return Err(SqlError::InvalidIdentifier {
                    detail: "an ON CONFLICT target names between 1 and max_columns columns"
                        .to_string(),
                });
            }
            Some(ConflictTarget::Columns(
                cols.iter()
                    .map(|c| convert_ident(c, dg.limits))
                    .collect::<Result<Vec<_>>>()?,
            ))
        }
        Some(sp::ConflictTarget::OnConstraint(name)) => {
            let converted = convert_object_name(name, dg.limits)?;
            if converted.0.len() != 1 {
                return Err(SqlError::InvalidIdentifier {
                    detail: "an ON CONFLICT ON CONSTRAINT name must not be qualified".to_string(),
                });
            }
            Some(ConflictTarget::Constraint(
                converted.0.into_iter().next().expect("checked len 1"),
            ))
        }
    };
    let action = match &oc.action {
        sp::OnConflictAction::DoNothing => ConflictAction::DoNothing,
        sp::OnConflictAction::DoUpdate(du) => ConflictAction::DoUpdate {
            assignments: du
                .assignments
                .iter()
                .map(|a| convert_assignment(a, dg))
                .collect::<Result<Vec<_>>>()?,
            selection: du
                .selection
                .as_ref()
                .map(|e| convert_expr(e, dg, 1))
                .transpose()?,
        },
    };
    Ok(OnConflict { target, action })
}

fn convert_update(update: &sp::Update, dg: &DepthGuard) -> Result<Update> {
    if update.from.is_some() {
        return Err(unsupported("UPDATE ... FROM"));
    }
    let table = convert_table_factor(&update.table.relation, dg, 1)?;
    if table.derived.is_some() {
        return Err(unsupported("UPDATE of a derived table"));
    }
    if !update.table.joins.is_empty() {
        return Err(unsupported("UPDATE with a joined target"));
    }
    let assignments = update
        .assignments
        .iter()
        .map(|a| convert_assignment(a, dg))
        .collect::<Result<Vec<_>>>()?;
    let selection = update
        .selection
        .as_ref()
        .map(|e| convert_expr(e, dg, 1))
        .transpose()?;
    let returning = update
        .returning
        .as_ref()
        .map(|items| convert_returning(items, dg))
        .transpose()?;
    Ok(Update {
        ctes: Vec::new(),
        table: table.name,
        assignments,
        selection,
        returning,
    })
}

fn convert_assignment(a: &sp::Assignment, dg: &DepthGuard) -> Result<Assignment> {
    let sp::AssignmentTarget::ColumnName(name) = &a.target else {
        return Err(unsupported("UPDATE SET (tuple) = (...)"));
    };
    let converted = convert_object_name(name, dg.limits)?;
    if converted.0.len() != 1 {
        return Err(SqlError::InvalidIdentifier {
            detail: "UPDATE SET target column must not be qualified".to_string(),
        });
    }
    Ok(Assignment {
        column: converted.0.into_iter().next().expect("checked len 1"),
        value: convert_expr(&a.value, dg, 1)?,
    })
}

fn convert_delete(delete: &sp::Delete, dg: &DepthGuard) -> Result<Delete> {
    if !delete.tables.is_empty()
        || delete.using.is_some()
        || !delete.order_by.is_empty()
        || delete.limit.is_some()
    {
        return Err(unsupported(
            "DELETE with USING/ORDER BY/LIMIT/multi-table form",
        ));
    }
    let sp::FromTable::WithFromKeyword(from) = &delete.from else {
        return Err(unsupported("DELETE without the FROM keyword"));
    };
    if from.len() != 1 || !from[0].joins.is_empty() {
        return Err(unsupported(
            "DELETE with more than one target or a joined target",
        ));
    }
    let table = convert_table_factor(&from[0].relation, dg, 1)?;
    if table.derived.is_some() {
        return Err(unsupported("DELETE from a derived table"));
    }
    let selection = delete
        .selection
        .as_ref()
        .map(|e| convert_expr(e, dg, 1))
        .transpose()?;
    let returning = delete
        .returning
        .as_ref()
        .map(|items| convert_returning(items, dg))
        .transpose()?;
    Ok(Delete {
        ctes: Vec::new(),
        table: table.name,
        selection,
        returning,
    })
}

// ---------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------

fn convert_expr(e: &sp::Expr, dg: &DepthGuard, depth: usize) -> Result<Expr> {
    dg.check(depth)?;
    match e {
        sp::Expr::Identifier(id) => Ok(Expr::Column(ColumnRef {
            parts: vec![convert_ident(id, dg.limits)?],
        })),
        sp::Expr::CompoundIdentifier(parts) => {
            if parts.len() > 3 {
                return Err(SqlError::InvalidIdentifier {
                    detail: "column reference has more than 3 qualification parts".to_string(),
                });
            }
            Ok(Expr::Column(ColumnRef {
                parts: parts
                    .iter()
                    .map(|p| convert_ident(p, dg.limits))
                    .collect::<Result<Vec<_>>>()?,
            }))
        }
        sp::Expr::Value(vws) => convert_value_expr(&vws.value),
        sp::Expr::Nested(inner) => convert_expr(inner, dg, depth + 1),
        sp::Expr::UnaryOp { op, expr } => convert_unary_op(op, expr, dg, depth),
        sp::Expr::BinaryOp { left, op, right } => {
            let op = convert_binary_op(op)?;
            Ok(Expr::BinaryOp {
                left: Box::new(convert_expr(left, dg, depth + 1)?),
                op,
                right: Box::new(convert_expr(right, dg, depth + 1)?),
            })
        }
        sp::Expr::IsNull(inner) => Ok(Expr::IsNull {
            expr: Box::new(convert_expr(inner, dg, depth + 1)?),
            negated: false,
        }),
        sp::Expr::IsNotNull(inner) => Ok(Expr::IsNull {
            expr: Box::new(convert_expr(inner, dg, depth + 1)?),
            negated: true,
        }),
        sp::Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Ok(Expr::Between {
            expr: Box::new(convert_expr(expr, dg, depth + 1)?),
            negated: *negated,
            low: Box::new(convert_expr(low, dg, depth + 1)?),
            high: Box::new(convert_expr(high, dg, depth + 1)?),
        }),
        sp::Expr::InList {
            expr,
            list,
            negated,
        } => {
            if list.len() > dg.limits.max_list_elements {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "IN list exceeds max_list_elements ({})",
                        dg.limits.max_list_elements
                    ),
                });
            }
            Ok(Expr::InList {
                expr: Box::new(convert_expr(expr, dg, depth + 1)?),
                list: list
                    .iter()
                    .map(|e| convert_expr(e, dg, depth + 1))
                    .collect::<Result<Vec<_>>>()?,
                negated: *negated,
            })
        }
        sp::Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => {
            if *any || escape_char.is_some() {
                return Err(unsupported("LIKE ANY / ESCAPE"));
            }
            Ok(Expr::Like {
                expr: Box::new(convert_expr(expr, dg, depth + 1)?),
                pattern: Box::new(convert_expr(pattern, dg, depth + 1)?),
                negated: *negated,
                case_insensitive: false,
            })
        }
        sp::Expr::ILike {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => {
            if *any || escape_char.is_some() {
                return Err(unsupported("ILIKE ANY / ESCAPE"));
            }
            Ok(Expr::Like {
                expr: Box::new(convert_expr(expr, dg, depth + 1)?),
                pattern: Box::new(convert_expr(pattern, dg, depth + 1)?),
                negated: *negated,
                case_insensitive: true,
            })
        }
        sp::Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if conditions.len() > dg.limits.max_list_elements {
                return Err(SqlError::ResourceLimit {
                    detail: "CASE has too many WHEN branches".to_string(),
                });
            }
            Ok(Expr::Case {
                operand: operand
                    .as_ref()
                    .map(|e| convert_expr(e, dg, depth + 1))
                    .transpose()?
                    .map(Box::new),
                branches: conditions
                    .iter()
                    .map(|c| {
                        Ok((
                            convert_expr(&c.condition, dg, depth + 1)?,
                            convert_expr(&c.result, dg, depth + 1)?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
                else_result: else_result
                    .as_ref()
                    .map(|e| convert_expr(e, dg, depth + 1))
                    .transpose()?
                    .map(Box::new),
            })
        }
        sp::Expr::TypedString(ts) => convert_typed_string(ts),
        sp::Expr::Function(f) => convert_function(f, dg, depth),
        sp::Expr::Subquery(q) => Ok(Expr::ScalarSubquery(Box::new(convert_subquery(
            q,
            dg,
            depth + 1,
        )?))),
        sp::Expr::Exists { subquery, negated } => Ok(Expr::Exists {
            subquery: Box::new(convert_subquery(subquery, dg, depth + 1)?),
            negated: *negated,
        }),
        sp::Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } => convert_quantified(left, compare_op, right, false, dg, depth),
        sp::Expr::AllOp {
            left,
            compare_op,
            right,
        } => convert_quantified(left, compare_op, right, true, dg, depth),
        sp::Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => Ok(Expr::InSubquery {
            expr: Box::new(convert_expr(expr, dg, depth + 1)?),
            subquery: Box::new(convert_subquery(subquery, dg, depth + 1)?),
            negated: *negated,
        }),
        other => Err(unsupported(format!(
            "expression kind: {}",
            expr_kind_name(other)
        ))),
    }
}

fn expr_kind_name(e: &sp::Expr) -> &'static str {
    match e {
        sp::Expr::Cast { .. } => "CAST",
        sp::Expr::Interval(_) => "INTERVAL",
        _ => "not in the approved grammar subset",
    }
}

fn convert_unary_op(
    op: &sp::UnaryOperator,
    inner: &sp::Expr,
    dg: &DepthGuard,
    depth: usize,
) -> Result<Expr> {
    // Fold a simple `-<number literal>` into one literal (not
    // `UnaryOp(Neg, Literal)`) so boundary values (e.g. BIGINT::MIN)
    // round-trip exactly through `crate::bind`'s numeric parsing —
    // `ast::Literal::Number`'s own doc comment explains why.
    if matches!(op, sp::UnaryOperator::Minus) {
        if let sp::Expr::Value(vws) = inner {
            if let sp::Value::Number(text, _) = &vws.value {
                return Ok(Expr::Literal(Literal::Number {
                    text: format!("-{text}"),
                    is_integer: is_integer_text(text),
                }));
            }
        }
    }
    match op {
        sp::UnaryOperator::Minus => Ok(Expr::UnaryOp {
            op: UnaryOp::Neg,
            expr: Box::new(convert_expr(inner, dg, depth + 1)?),
        }),
        sp::UnaryOperator::Not => Ok(Expr::UnaryOp {
            op: UnaryOp::Not,
            expr: Box::new(convert_expr(inner, dg, depth + 1)?),
        }),
        sp::UnaryOperator::Plus => convert_expr(inner, dg, depth + 1),
        other => Err(unsupported(format!("unary operator: {other:?}"))),
    }
}

/// `left <op> ANY|SOME|ALL (subquery)`. Only the six comparison operators and
/// only a subquery on the right (not an array) are accepted.
fn convert_quantified(
    left: &sp::Expr,
    op: &sp::BinaryOperator,
    right: &sp::Expr,
    all: bool,
    dg: &DepthGuard,
    depth: usize,
) -> Result<Expr> {
    let op = match convert_binary_op(op)? {
        o @ (BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq) => o,
        _ => return Err(unsupported("ANY / ALL with a non-comparison operator")),
    };
    let sp::Expr::Subquery(q) = right else {
        return Err(unsupported(
            "ANY / ALL over something other than a subquery (arrays)",
        ));
    };
    Ok(Expr::Quantified {
        expr: Box::new(convert_expr(left, dg, depth + 1)?),
        op,
        all,
        subquery: Box::new(convert_subquery(q, dg, depth + 1)?),
    })
}

fn convert_binary_op(op: &sp::BinaryOperator) -> Result<BinaryOp> {
    use sp::BinaryOperator as B;
    Ok(match op {
        B::Plus => BinaryOp::Add,
        B::Minus => BinaryOp::Sub,
        B::Multiply => BinaryOp::Mul,
        B::Divide => BinaryOp::Div,
        B::Modulo => BinaryOp::Mod,
        B::Gt => BinaryOp::Gt,
        B::Lt => BinaryOp::Lt,
        B::GtEq => BinaryOp::GtEq,
        B::LtEq => BinaryOp::LtEq,
        B::Eq => BinaryOp::Eq,
        B::NotEq => BinaryOp::NotEq,
        B::And => BinaryOp::And,
        B::Or => BinaryOp::Or,
        other => return Err(unsupported(format!("binary operator: {other:?}"))),
    })
}

fn is_integer_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    !lower.contains('.') && !lower.contains('e')
}

fn convert_value_expr(v: &sp::Value) -> Result<Expr> {
    match v {
        sp::Value::Placeholder(text) => {
            let idx_text = text
                .strip_prefix('$')
                .ok_or_else(|| SqlError::InvalidParameter {
                    detail: "only $n-style parameters are supported (no bare '?')".to_string(),
                })?;
            let idx: u32 = idx_text.parse().map_err(|_| SqlError::InvalidParameter {
                detail: "parameter index must be a positive integer ($1, $2, ...)".to_string(),
            })?;
            if idx == 0 {
                return Err(SqlError::InvalidParameter {
                    detail: "parameter index is 1-based; $0 is invalid".to_string(),
                });
            }
            Ok(Expr::Parameter(idx))
        }
        sp::Value::Null => Ok(Expr::Literal(Literal::Null)),
        sp::Value::Boolean(b) => Ok(Expr::Literal(Literal::Boolean(*b))),
        sp::Value::Number(text, _) => Ok(Expr::Literal(Literal::Number {
            text: text.clone(),
            is_integer: is_integer_text(text),
        })),
        sp::Value::SingleQuotedString(s) | sp::Value::DoubleQuotedString(s) => {
            Ok(Expr::Literal(Literal::Text(s.clone())))
        }
        sp::Value::HexStringLiteral(hex) => {
            let bytes = decode_hex(hex).ok_or_else(|| SqlError::Parse {
                detail: "malformed hex string literal".to_string(),
            })?;
            Ok(Expr::Literal(Literal::Blob(bytes)))
        }
        other => Err(unsupported(format!("literal kind: {other:?}"))),
    }
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Some(out)
}

fn convert_typed_string(ts: &sp::TypedString) -> Result<Expr> {
    if ts.uses_odbc_syntax {
        return Err(unsupported("ODBC-syntax typed string literal"));
    }
    let data_type = convert_data_type(&ts.data_type)?;
    if !matches!(
        data_type,
        SqlDataType::Date | SqlDataType::Time | SqlDataType::Timestamp
    ) {
        return Err(unsupported("typed string literal for a non-temporal type"));
    }
    let text = ts
        .value
        .clone()
        .into_string()
        .ok_or_else(|| SqlError::Parse {
            detail: "typed string literal value must be a plain string".to_string(),
        })?;
    Ok(Expr::Literal(Literal::Typed { data_type, text }))
}

fn convert_function(f: &sp::Function, dg: &DepthGuard, depth: usize) -> Result<Expr> {
    // `IGNORE NULLS` / `RESPECT NULLS` belong to window functions only
    // (`LAG(x) IGNORE NULLS OVER (...)`); on any other call they are refused.
    if f.uses_odbc_syntax
        || !f.within_group.is_empty()
        || (f.null_treatment.is_some() && f.over.is_none())
    {
        return Err(unsupported(
            "function call with ODBC/WITHIN GROUP/null-treatment modifiers",
        ));
    }
    if !matches!(f.parameters, sp::FunctionArguments::None) {
        return Err(unsupported("function call with a parametric argument list"));
    }

    let name = convert_object_name(&f.name, dg.limits)?;
    // SQL-mission-2: an `OVER` clause makes the call a window function. (Before
    // SQL-mission-1 Slice D the clause was silently dropped, turning
    // `SUM(x) OVER ()` into the plain aggregate `SUM(x)`; Slice D refused it;
    // SQL-mission-2 implements it. It is never ignored.)
    if let Some(over) = &f.over {
        return convert_window_function(f, &name, over, dg, depth);
    }
    if name.0.len() == 1 {
        if let Some(agg_func) = crate::aggregate::AggregateFunc::lookup(&name.0[0].value) {
            let (arg, distinct, filter) = convert_aggregate_call(agg_func, f, dg, depth)?;
            return Ok(Expr::Aggregate {
                func: agg_func,
                arg,
                distinct,
                filter,
            });
        }
        if name.0[0].value.eq_ignore_ascii_case("grouping") {
            if f.filter.is_some() {
                return Err(unsupported("FILTER on GROUPING"));
            }
            let sp::FunctionArguments::List(list) = &f.args else {
                return Err(SqlError::TypeMismatch {
                    detail: "GROUPING requires at least one argument".to_string(),
                });
            };
            if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
                return Err(unsupported("DISTINCT / clauses inside GROUPING(...)"));
            }
            if list.args.is_empty() {
                return Err(SqlError::TypeMismatch {
                    detail: "GROUPING requires at least one argument".to_string(),
                });
            }
            if list.args.len() > dg.limits.max_columns {
                return Err(SqlError::ResourceLimit {
                    detail: "GROUPING call has too many arguments".to_string(),
                });
            }
            let args = list
                .args
                .iter()
                .map(|a| match a {
                    sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Expr(e)) => {
                        convert_expr(e, dg, depth + 1)
                    }
                    _ => Err(unsupported("named/wildcard GROUPING arguments")),
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(Expr::Grouping(args));
        }
    }
    if f.filter.is_some() {
        return Err(unsupported("FILTER on a function that is not an aggregate"));
    }

    let args = match &f.args {
        sp::FunctionArguments::None => Vec::new(),
        sp::FunctionArguments::List(list) => {
            if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
                return Err(unsupported(
                    "function call with DISTINCT/ORDER BY inside the argument list",
                ));
            }
            if list.args.len() > dg.limits.max_columns {
                return Err(SqlError::ResourceLimit {
                    detail: "function call has too many arguments".to_string(),
                });
            }
            list.args
                .iter()
                .map(|a| match a {
                    sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Expr(e)) => {
                        convert_expr(e, dg, depth + 1)
                    }
                    _ => Err(unsupported("named/wildcard function arguments")),
                })
                .collect::<Result<Vec<_>>>()?
        }
        sp::FunctionArguments::Subquery(_) => {
            return Err(unsupported("function call with a bare subquery argument"))
        }
    };
    Ok(Expr::Function { name, args })
}

// ---------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------

fn convert_create_table(ct: &sp::CreateTable, limits: &SqlLimits) -> Result<CreateTable> {
    if ct.or_replace
        || ct.temporary
        || ct.external
        || ct.query.is_some()
        || ct.like.is_some()
        || ct.clone.is_some()
    {
        return Err(unsupported(
            "CREATE TABLE with OR REPLACE/TEMPORARY/EXTERNAL/AS SELECT/LIKE/CLONE",
        ));
    }
    if ct.columns.len() > limits.max_columns {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "CREATE TABLE column count exceeds max_columns ({})",
                limits.max_columns
            ),
        });
    }
    let mut columns = Vec::with_capacity(ct.columns.len());
    let mut foreign_keys: Vec<ForeignKeyDef> = Vec::new();
    let mut checks: Vec<CheckDef> = Vec::new();
    for c in &ct.columns {
        let (column, extras) = convert_column_def_with_fk(c, limits)?;
        // An inline `REFERENCES` is a one-column foreign key on that column.
        if let Some(mut fk) = extras.foreign_key {
            fk.columns = vec![column.name.clone()];
            foreign_keys.push(fk);
        }
        checks.extend(extras.checks);
        columns.push(column);
    }

    let mut table_primary_key: Option<Vec<Ident>> = None;
    for constraint in &ct.constraints {
        match constraint {
            sp::TableConstraint::PrimaryKey(pk) => {
                if table_primary_key.is_some() {
                    return Err(unsupported("more than one table-level PRIMARY KEY"));
                }
                table_primary_key = Some(index_columns_to_idents(&pk.columns, limits)?);
            }
            sp::TableConstraint::Unique(_) => {
                return Err(unsupported(
                    "table-level UNIQUE constraint (create the table, then CREATE UNIQUE INDEX)",
                ))
            }
            sp::TableConstraint::ForeignKey(fk) => {
                foreign_keys.push(convert_foreign_key(fk, limits)?);
            }
            sp::TableConstraint::Check(check) => {
                checks.push(convert_check(check, limits)?);
            }
            other => return Err(unsupported(format!("table constraint kind: {other:?}"))),
        }
    }

    Ok(CreateTable {
        name: convert_object_name(&ct.name, limits)?,
        if_not_exists: ct.if_not_exists,
        columns,
        table_primary_key,
        foreign_keys,
        checks,
    })
}

/// `CHECK (expr)`. `NOT ENFORCED` is refused: every constraint here is enforced.
fn convert_check(check: &sp::CheckConstraint, limits: &SqlLimits) -> Result<CheckDef> {
    if check.enforced == Some(false) {
        return Err(unsupported("CHECK ... NOT ENFORCED (every constraint is enforced)"));
    }
    if check.no_inherit {
        return Err(unsupported("CHECK ... NO INHERIT"));
    }
    let dg = DepthGuard { limits };
    Ok(CheckDef {
        name: check.name.as_ref().map(|n| convert_ident(n, limits)).transpose()?,
        expr: convert_expr(&check.expr, &dg, 1)?,
        text: check.expr.to_string(),
    })
}

/// What a column definition declares beyond the column itself.
#[derive(Default)]
struct ColumnExtras {
    foreign_key: Option<ForeignKeyDef>,
    checks: Vec<CheckDef>,
}

fn convert_referential_action(a: &sp::ReferentialAction) -> ReferentialAction {
    match a {
        sp::ReferentialAction::Restrict => ReferentialAction::Restrict,
        sp::ReferentialAction::Cascade => ReferentialAction::Cascade,
        sp::ReferentialAction::SetNull => ReferentialAction::SetNull,
        sp::ReferentialAction::NoAction => ReferentialAction::NoAction,
        sp::ReferentialAction::SetDefault => ReferentialAction::SetDefault,
    }
}

/// `FOREIGN KEY (...) REFERENCES t [(...)] [ON DELETE a] [ON UPDATE a]` (and the
/// inline `REFERENCES` form, whose `columns` the caller fills in). `MATCH FULL` /
/// `MATCH PARTIAL` and `DEFERRABLE` are refused: rubiXDb implements `MATCH SIMPLE`
/// with immediate enforcement.
fn convert_foreign_key(fk: &sp::ForeignKeyConstraint, limits: &SqlLimits) -> Result<ForeignKeyDef> {
    if let Some(kind) = &fk.match_kind {
        if !matches!(kind, sp::ConstraintReferenceMatchKind::Simple) {
            return Err(unsupported(
                "foreign key MATCH FULL / MATCH PARTIAL (only the default MATCH SIMPLE exists)",
            ));
        }
    }
    if fk.characteristics.is_some() {
        return Err(unsupported(
            "DEFERRABLE / INITIALLY DEFERRED foreign keys (enforcement is always immediate)",
        ));
    }
    let name = fk.name.as_ref().map(|n| convert_ident(n, limits)).transpose()?;
    Ok(ForeignKeyDef {
        name,
        columns: fk
            .columns
            .iter()
            .map(|c| convert_ident(c, limits))
            .collect::<Result<_>>()?,
        foreign_table: convert_object_name(&fk.foreign_table, limits)?,
        referred_columns: fk
            .referred_columns
            .iter()
            .map(|c| convert_ident(c, limits))
            .collect::<Result<_>>()?,
        on_delete: fk.on_delete.as_ref().map(convert_referential_action),
        on_update: fk.on_update.as_ref().map(convert_referential_action),
    })
}

// ---------------------------------------------------------------------
// ALTER TABLE
// ---------------------------------------------------------------------

/// One-operation `ALTER TABLE`. The forms rubiXDb cannot do safely are refused here
/// with the reason, so the statement never reaches the binder:
///
/// * `DROP COLUMN` -- BLOCKED: a column's identity is its position, which the
///   primary key, every index, every foreign key and every stored row use
///   (`docs/adr/ADR-DDL-FK-01.md`);
/// * `ALTER COLUMN ... TYPE` -- NOT IMPLEMENTED (needs a bounded rewrite);
/// * `ADD/DROP PRIMARY KEY`, `ADD CHECK`, `RENAME TO`, `NOT VALID` -- NOT IMPLEMENTED.
fn convert_alter_table(at: &sp::AlterTable, limits: &SqlLimits) -> Result<AlterTable> {
    if at.only || at.location.is_some() || at.on_cluster.is_some() || at.table_type.is_some() {
        return Err(unsupported(
            "ALTER TABLE with ONLY / LOCATION / ON CLUSTER / a table type",
        ));
    }
    let [op] = at.operations.as_slice() else {
        return Err(unsupported(
            "ALTER TABLE with more than one operation (each change is applied atomically on its own; send one statement per change)",
        ));
    };
    let operation = match op {
        sp::AlterTableOperation::AddColumn {
            column_def,
            if_not_exists,
            column_position,
            ..
        } => {
            if column_position.is_some() {
                return Err(unsupported("ADD COLUMN ... FIRST / AFTER (columns are always appended)"));
            }
            let (column, inline_fk) = convert_column_def_with_fk(column_def, limits)?;
            if column.primary_key {
                return Err(unsupported(
                    "ADD COLUMN ... PRIMARY KEY (a table has one primary key, fixed at CREATE TABLE)",
                ));
            }
            if inline_fk.foreign_key.is_some() {
                return Err(unsupported(
                    "ADD COLUMN ... REFERENCES (add the column, then ALTER TABLE ... ADD FOREIGN KEY)",
                ));
            }
            if !inline_fk.checks.is_empty() {
                return Err(unsupported(
                    "ADD COLUMN ... CHECK (add the column, then ALTER TABLE ... ADD CONSTRAINT CHECK)",
                ));
            }
            AlterOperation::AddColumn {
                column,
                if_not_exists: *if_not_exists,
            }
        }
        sp::AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => AlterOperation::RenameColumn {
            from: convert_ident(old_column_name, limits)?,
            to: convert_ident(new_column_name, limits)?,
        },
        sp::AlterTableOperation::AlterColumn { column_name, op } => {
            let column = convert_ident(column_name, limits)?;
            match op {
                sp::AlterColumnOperation::SetNotNull => AlterOperation::SetNotNull { column },
                sp::AlterColumnOperation::DropNotNull => AlterOperation::DropNotNull { column },
                sp::AlterColumnOperation::SetDefault { value } => {
                    let dg = DepthGuard { limits };
                    AlterOperation::SetDefault {
                        column,
                        value: convert_expr(value, &dg, 1)?,
                    }
                }
                sp::AlterColumnOperation::DropDefault => AlterOperation::DropDefault { column },
                sp::AlterColumnOperation::SetDataType { .. } => {
                    return Err(unsupported(
                        "ALTER COLUMN ... TYPE (NOT IMPLEMENTED: a type change needs a bounded rewrite of every stored row)",
                    ))
                }
                _ => return Err(unsupported("this ALTER COLUMN operation")),
            }
        }
        sp::AlterTableOperation::AddConstraint {
            constraint,
            not_valid,
        } => {
            if *not_valid {
                return Err(unsupported(
                    "ADD CONSTRAINT ... NOT VALID (a constraint is always validated against the existing rows)",
                ));
            }
            match constraint {
                sp::TableConstraint::ForeignKey(fk) => {
                    AlterOperation::AddForeignKey(convert_foreign_key(fk, limits)?)
                }
                sp::TableConstraint::Unique(u) => AlterOperation::AddUnique {
                    name: u.name.as_ref().map(|n| convert_ident(n, limits)).transpose()?,
                    columns: index_columns_to_idents(&u.columns, limits)?,
                },
                sp::TableConstraint::PrimaryKey(_) => {
                    return Err(unsupported(
                        "ADD PRIMARY KEY (a table's primary key is fixed at CREATE TABLE)",
                    ))
                }
                sp::TableConstraint::Check(check) => {
                    AlterOperation::AddCheck(convert_check(check, limits)?)
                }
                _ => return Err(unsupported("this table constraint kind")),
            }
        }
        sp::AlterTableOperation::DropConstraint {
            if_exists,
            name,
            drop_behavior,
        } => {
            if matches!(drop_behavior, Some(sp::DropBehavior::Cascade)) {
                return Err(unsupported("DROP CONSTRAINT ... CASCADE"));
            }
            AlterOperation::DropConstraint {
                name: convert_ident(name, limits)?,
                if_exists: *if_exists,
            }
        }
        sp::AlterTableOperation::DropColumn { .. } => {
            return Err(unsupported(
                "DROP COLUMN (BLOCKED: a column is identified by its position, which the primary key, indexes, foreign keys and stored rows all use; see docs/adr/ADR-DDL-FK-01.md)",
            ))
        }
        sp::AlterTableOperation::RenameTable { .. } => {
            return Err(unsupported("ALTER TABLE ... RENAME TO (NOT IMPLEMENTED)"))
        }
        _ => return Err(unsupported("this ALTER TABLE operation")),
    };
    Ok(AlterTable {
        name: convert_object_name(&at.name, limits)?,
        if_exists: at.if_exists,
        operation,
    })
}

fn index_columns_to_idents(cols: &[sp::IndexColumn], limits: &SqlLimits) -> Result<Vec<Ident>> {
    cols.iter()
        .map(|c| {
            if c.operator_class.is_some() {
                return Err(unsupported("index column with an operator class"));
            }
            match &c.column.expr {
                sp::Expr::Identifier(id) => convert_ident(id, limits),
                _ => Err(unsupported(
                    "index/constraint column must be a plain column name",
                )),
            }
        })
        .collect()
}

/// A column definition, plus -- when it carries an inline `REFERENCES` -- the
/// foreign key it declares (its `columns` left empty for the caller to fill).
fn convert_column_def_with_fk(
    c: &sp::ColumnDef,
    limits: &SqlLimits,
) -> Result<(ColumnDef, ColumnExtras)> {
    let data_type = convert_data_type(&c.data_type)?;
    let mut nullable = true;
    let mut default = None;
    let mut primary_key = false;
    let mut extras = ColumnExtras::default();
    for opt in &c.options {
        match &opt.option {
            sp::ColumnOption::ForeignKey(fk) => {
                let mut def = convert_foreign_key(fk, limits)?;
                if def.name.is_none() {
                    def.name = opt.name.as_ref().map(|n| convert_ident(n, limits)).transpose()?;
                }
                extras.foreign_key = Some(def);
            }
            sp::ColumnOption::Check(check) => {
                let mut def = convert_check(check, limits)?;
                if def.name.is_none() {
                    def.name = opt.name.as_ref().map(|n| convert_ident(n, limits)).transpose()?;
                }
                extras.checks.push(def);
            }
            sp::ColumnOption::Null => {}
            sp::ColumnOption::NotNull => nullable = false,
            sp::ColumnOption::Default(e) => {
                let dg = DepthGuard { limits };
                default = Some(convert_expr(e, &dg, 1)?);
            }
            sp::ColumnOption::PrimaryKey(_) => {
                primary_key = true;
                nullable = false;
            }
            sp::ColumnOption::Unique(_) => {
                return Err(unsupported(
                    "inline UNIQUE column option (create the table, then CREATE UNIQUE INDEX)",
                ))
            }
            other => return Err(unsupported(format!("column option kind: {other:?}"))),
        }
    }
    Ok((
        ColumnDef {
            name: convert_ident(&c.name, limits)?,
            data_type,
            nullable,
            default,
            primary_key,
        },
        extras,
    ))
}

fn convert_data_type(dt: &sp::DataType) -> Result<SqlDataType> {
    use sp::DataType as D;
    Ok(match dt {
        D::Bool | D::Boolean => SqlDataType::Boolean,
        D::Int(_) | D::Integer(_) => SqlDataType::Integer,
        D::BigInt(_) => SqlDataType::Bigint,
        D::Real => SqlDataType::Real,
        D::Double(_) | D::DoublePrecision => SqlDataType::Double,
        D::Decimal(info) | D::Numeric(info) => convert_exact_number_info(info)?,
        D::Text | D::Varchar(_) | D::CharacterVarying(_) | D::Char(_) => SqlDataType::Text,
        D::Blob(_) | D::Bytea | D::Binary(_) | D::Varbinary(_) => SqlDataType::Blob,
        D::Date => SqlDataType::Date,
        D::Time(None, _) => SqlDataType::Time,
        D::Timestamp(None, _) => SqlDataType::Timestamp,
        other => return Err(unsupported(format!("data type: {other:?}"))),
    })
}

fn convert_exact_number_info(info: &sp::ExactNumberInfo) -> Result<SqlDataType> {
    let (precision, scale) = match info {
        sp::ExactNumberInfo::None => (38u64, 0i64),
        sp::ExactNumberInfo::Precision(p) => (*p, 0),
        sp::ExactNumberInfo::PrecisionAndScale(p, s) => (*p, *s),
    };
    if !(1..=38).contains(&precision) {
        return Err(SqlError::TypeMismatch {
            detail: format!("DECIMAL precision must be in 1..=38, got {precision}"),
        });
    }
    if scale < 0 || scale as u64 > precision {
        return Err(SqlError::TypeMismatch {
            detail: "DECIMAL scale must be non-negative and not exceed precision".to_string(),
        });
    }
    Ok(SqlDataType::Decimal {
        precision: precision as u8,
        scale: scale as u8,
    })
}

fn convert_create_index(ci: &sp::CreateIndex) -> Result<CreateIndex> {
    if ci.using.is_some()
        || ci.concurrently
        || ci.r#async
        || !ci.include.is_empty()
        || ci.nulls_distinct.is_some()
        || !ci.with.is_empty()
        || ci.predicate.is_some()
        || !ci.index_options.is_empty()
        || !ci.alter_options.is_empty()
    {
        return Err(unsupported(
            "CREATE INDEX with USING/CONCURRENTLY/INCLUDE/WITH/WHERE/options",
        ));
    }
    let dummy_limits = SqlLimits::default();
    Ok(CreateIndex {
        name: ci
            .name
            .as_ref()
            .map(|n| {
                let converted = convert_object_name(n, &dummy_limits)?;
                if converted.0.len() != 1 {
                    return Err(SqlError::InvalidIdentifier {
                        detail: "index name must not be qualified".to_string(),
                    });
                }
                Ok(converted.0.into_iter().next().expect("checked len 1"))
            })
            .transpose()?,
        table: convert_object_name(&ci.table_name, &dummy_limits)?,
        columns: index_columns_to_idents(&ci.columns, &dummy_limits)?,
        unique: ci.unique,
        if_not_exists: ci.if_not_exists,
    })
}

// ---------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------

fn convert_ident(id: &sp::Ident, limits: &SqlLimits) -> Result<Ident> {
    if id.value.len() > limits.max_identifier_len {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "identifier exceeds max_identifier_len ({})",
                limits.max_identifier_len
            ),
        });
    }
    let quoted = id.quote_style.is_some();
    let value = if quoted {
        id.value.clone()
    } else {
        id.value.to_lowercase()
    };
    Ok(Ident::new(value, quoted))
}

fn convert_object_name(name: &sp::ObjectName, limits: &SqlLimits) -> Result<ObjectName> {
    if name.0.len() > 3 {
        return Err(SqlError::InvalidIdentifier {
            detail: "object name has more than 3 qualification parts".to_string(),
        });
    }
    let parts = name
        .0
        .iter()
        .map(|part| match part {
            sp::ObjectNamePart::Identifier(id) => convert_ident(id, limits),
            sp::ObjectNamePart::Function(_) => Err(unsupported("function-valued object name part")),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ObjectName(parts))
}
