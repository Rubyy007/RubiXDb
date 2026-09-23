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
        sp::Statement::Query(q) => Ok(Statement::Select(convert_query(q, &dg, 0)?)),
        sp::Statement::Insert(insert) => Ok(Statement::Insert(convert_insert(insert, &dg)?)),
        sp::Statement::Update(update) => Ok(Statement::Update(convert_update(update, &dg)?)),
        sp::Statement::Delete(delete) => Ok(Statement::Delete(convert_delete(delete, &dg)?)),
        sp::Statement::CreateTable(ct) => Ok(Statement::CreateTable(convert_create_table(ct, limits)?)),
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
                    let table_name = table
                        .as_ref()
                        .ok_or_else(|| unsupported("DROP INDEX requires 'ON <table>' (index names are table-scoped)"))?;
                    let index_ident_name = convert_object_name(&names[0], limits)?;
                    if index_ident_name.0.len() != 1 {
                        return Err(SqlError::InvalidIdentifier {
                            detail: "index name must not be qualified".to_string(),
                        });
                    }
                    Ok(Statement::DropIndex(DropIndex {
                        table: convert_object_name(table_name, limits)?,
                        name: index_ident_name.0.into_iter().next().expect("checked len 1"),
                        if_exists: *if_exists,
                    }))
                }
                other => Err(unsupported(format!("DROP {other:?}"))),
            }
        }
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
            if *analyze || *verbose || *query_plan || *estimate || format.is_some() || options.is_some() {
                return Err(unsupported("EXPLAIN with modifiers (ANALYZE/VERBOSE/FORMAT/...)"));
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
                return Err(unsupported("BEGIN/START TRANSACTION with modes/modifiers/a statement block"));
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

fn convert_query(q: &sp::Query, dg: &DepthGuard, depth: usize) -> Result<Select> {
    dg.check(depth)?;
    if q.with.is_some() {
        return Err(unsupported("WITH (common table expressions)"));
    }
    if q.fetch.is_some() || !q.locks.is_empty() || q.for_clause.is_some() {
        return Err(unsupported("FETCH/FOR UPDATE/FOR XML|JSON clauses"));
    }
    let sp::SetExpr::Select(select) = q.body.as_ref() else {
        return Err(unsupported(
            "query body must be a plain SELECT (no UNION/EXCEPT/INTERSECT/VALUES/subquery-as-body)",
        ));
    };

    if select.top.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
        || select.exclude.is_some()
    {
        return Err(unsupported(
            "SELECT with TOP/INTO/LATERAL/PREWHERE/CONNECT BY/CLUSTER BY/DISTRIBUTE BY/SORT BY/HAVING/WINDOW/QUALIFY/value-table-mode/EXCLUDE",
        ));
    }
    match &select.group_by {
        sp::GroupByExpr::All(_) => return Err(unsupported("GROUP BY ALL")),
        sp::GroupByExpr::Expressions(exprs, modifiers) => {
            if !exprs.is_empty() || !modifiers.is_empty() {
                return Err(unsupported("GROUP BY (aggregation binding is out of this increment's scope)"));
            }
        }
    }

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
        return Err(unsupported("multiple comma-separated FROM items (implicit CROSS JOIN)"));
    };

    let selection = select
        .selection
        .as_ref()
        .map(|e| convert_expr(e, dg, depth + 1))
        .transpose()?;

    let order_by = match &q.order_by {
        None => Vec::new(),
        Some(sp::OrderBy {
            kind: sp::OrderByKind::Expressions(items),
            interpolate: None,
        }) => items
            .iter()
            .map(|item| convert_order_by_item(item, dg, depth + 1))
            .collect::<Result<Vec<_>>>()?,
        Some(_) => return Err(unsupported("ORDER BY ALL / WITH INTERPOLATE")),
    };

    let (limit, offset) = match &q.limit_clause {
        None => (None, None),
        Some(sp::LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        }) => {
            if !limit_by.is_empty() {
                return Err(unsupported("LIMIT ... BY"));
            }
            let limit = limit.as_ref().map(|e| convert_expr(e, dg, depth + 1)).transpose()?;
            let offset = offset
                .as_ref()
                .map(|o| convert_expr(&o.value, dg, depth + 1))
                .transpose()?;
            (limit, offset)
        }
        Some(sp::LimitClause::OffsetCommaLimit { .. }) => {
            return Err(unsupported("MySQL LIMIT offset, limit syntax"))
        }
    };

    Ok(Select {
        distinct,
        projection,
        from,
        selection,
        order_by,
        limit,
        offset,
    })
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
                return Err(unsupported("qualified * with EXCLUDE/REPLACE/RENAME options"));
            }
            match kind {
                sp::SelectItemQualifiedWildcardKind::ObjectName(name) => {
                    Ok(SelectItem::QualifiedWildcard(convert_object_name(name, dg.limits)?))
                }
                sp::SelectItemQualifiedWildcardKind::Expr(_) => {
                    Err(unsupported("wildcard on an arbitrary expression"))
                }
            }
        }
        sp::SelectItem::ExprWithAliases { .. } => Err(unsupported("expression with multiple aliases")),
    }
}

fn is_default_wildcard_options(opts: &sp::WildcardAdditionalOptions) -> bool {
    opts.opt_exclude.is_none()
        && opts.opt_except.is_none()
        && opts.opt_rename.is_none()
        && opts.opt_replace.is_none()
        && opts.opt_ilike.is_none()
}

fn convert_table_with_joins(t: &sp::TableWithJoins, dg: &DepthGuard, depth: usize) -> Result<FromClause> {
    dg.check(depth)?;
    let first = convert_table_factor(&t.relation, dg.limits)?;
    let joins = t
        .joins
        .iter()
        .map(|j| convert_join(j, dg, depth + 1))
        .collect::<Result<Vec<_>>>()?;
    Ok(FromClause { first, joins })
}

fn convert_table_factor(tf: &sp::TableFactor, limits: &SqlLimits) -> Result<TableRef> {
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
                return Err(unsupported("table reference with function args/hints/version/partitions/ordinality"));
            }
            Ok(TableRef {
                name: convert_object_name(name, limits)?,
                alias: alias
                    .as_ref()
                    .map(|a| convert_table_alias(a, limits))
                    .transpose()?,
            })
        }
        sp::TableFactor::Derived { .. } => Err(unsupported("derived table (subquery in FROM)")),
        other => Err(unsupported(format!("FROM item kind: {other:?}"))
            .into_kind_only()),
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
                detail: detail.split('(').next().unwrap_or(&detail).trim().to_string(),
            },
            other => other,
        }
    }
}

fn convert_table_alias(alias: &sp::TableAlias, limits: &SqlLimits) -> Result<Ident> {
    if !alias.columns.is_empty() {
        return Err(unsupported("table alias with an explicit column list"));
    }
    convert_ident(&alias.name, limits)
}

fn convert_join(j: &sp::Join, dg: &DepthGuard, depth: usize) -> Result<Join> {
    dg.check(depth)?;
    if j.global {
        return Err(unsupported("GLOBAL JOIN"));
    }
    let table = convert_table_factor(&j.relation, dg.limits)?;
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

fn convert_order_by_item(item: &sp::OrderByExpr, dg: &DepthGuard, depth: usize) -> Result<OrderByItem> {
    if item.with_fill.is_some() {
        return Err(unsupported("ORDER BY ... WITH FILL"));
    }
    let descending = match item.options.sort {
        None | Some(sp::OrderBySort::Asc) => false,
        Some(sp::OrderBySort::Desc) => true,
        Some(sp::OrderBySort::Using(_)) => return Err(unsupported("ORDER BY ... USING <operator>")),
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
        || insert.on.is_some()
        || insert.returning.is_some()
        || insert.replace_into
        || insert.priority.is_some()
        || insert.settings.is_some()
        || insert.format_clause.is_some()
    {
        return Err(unsupported("INSERT with dialect-specific modifiers (OR/IGNORE/ON CONFLICT/RETURNING/...)"));
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
                detail: format!("INSERT column list exceeds max_columns ({})", dg.limits.max_columns),
            });
        }
    }

    let source = insert
        .source
        .as_ref()
        .ok_or_else(|| unsupported("INSERT ... DEFAULT VALUES / SET form"))?;
    if source.with.is_some() || source.order_by.is_some() || source.limit_clause.is_some() {
        return Err(unsupported("INSERT ... SELECT with WITH/ORDER BY/LIMIT"));
    }
    let sp::SetExpr::Values(values) = source.body.as_ref() else {
        return Err(unsupported("INSERT source must be a VALUES list (no INSERT ... SELECT)"));
    };
    if values.rows.len() > dg.limits.max_values_rows {
        return Err(SqlError::ResourceLimit {
            detail: format!("VALUES row count exceeds max_values_rows ({})", dg.limits.max_values_rows),
        });
    }
    let rows = values
        .rows
        .iter()
        .map(|row| {
            if row.content.len() > dg.limits.max_columns {
                return Err(SqlError::ResourceLimit {
                    detail: format!("VALUES row width exceeds max_columns ({})", dg.limits.max_columns),
                });
            }
            row.content
                .iter()
                .map(|e| convert_expr(e, dg, 1))
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Insert { table, columns, rows })
}

fn convert_update(update: &sp::Update, dg: &DepthGuard) -> Result<Update> {
    if update.from.is_some() || update.returning.is_some() {
        return Err(unsupported("UPDATE ... FROM / RETURNING"));
    }
    let table = convert_table_factor(&update.table.relation, dg.limits)?;
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
    Ok(Update {
        table: table.name,
        assignments,
        selection,
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
        || delete.returning.is_some()
        || !delete.order_by.is_empty()
        || delete.limit.is_some()
    {
        return Err(unsupported("DELETE with USING/RETURNING/ORDER BY/LIMIT/multi-table form"));
    }
    let sp::FromTable::WithFromKeyword(from) = &delete.from else {
        return Err(unsupported("DELETE without the FROM keyword"));
    };
    if from.len() != 1 || !from[0].joins.is_empty() {
        return Err(unsupported("DELETE with more than one target or a joined target"));
    }
    let table = convert_table_factor(&from[0].relation, dg.limits)?;
    let selection = delete
        .selection
        .as_ref()
        .map(|e| convert_expr(e, dg, 1))
        .transpose()?;
    Ok(Delete {
        table: table.name,
        selection,
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
        sp::Expr::InList { expr, list, negated } => {
            if list.len() > dg.limits.max_list_elements {
                return Err(SqlError::ResourceLimit {
                    detail: format!("IN list exceeds max_list_elements ({})", dg.limits.max_list_elements),
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
        other => Err(unsupported(format!("expression kind: {}", expr_kind_name(other)))),
    }
}

fn expr_kind_name(e: &sp::Expr) -> &'static str {
    match e {
        sp::Expr::Subquery(_) => "subquery expression",
        sp::Expr::Exists { .. } => "EXISTS",
        sp::Expr::InSubquery { .. } => "IN (subquery)",
        sp::Expr::Cast { .. } => "CAST",
        sp::Expr::Interval(_) => "INTERVAL",
        _ => "not in the approved grammar subset",
    }
}

fn convert_unary_op(op: &sp::UnaryOperator, inner: &sp::Expr, dg: &DepthGuard, depth: usize) -> Result<Expr> {
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
            let idx_text = text.strip_prefix('$').ok_or_else(|| SqlError::InvalidParameter {
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
    if !matches!(data_type, SqlDataType::Date | SqlDataType::Time | SqlDataType::Timestamp) {
        return Err(unsupported("typed string literal for a non-temporal type"));
    }
    let text = ts.value.clone().into_string().ok_or_else(|| SqlError::Parse {
        detail: "typed string literal value must be a plain string".to_string(),
    })?;
    Ok(Expr::Literal(Literal::Typed { data_type, text }))
}

fn convert_function(f: &sp::Function, dg: &DepthGuard, depth: usize) -> Result<Expr> {
    if f.uses_odbc_syntax || !f.within_group.is_empty() || f.filter.is_some() || f.null_treatment.is_some() {
        return Err(unsupported("function call with ODBC/WITHIN GROUP/FILTER/null-treatment modifiers"));
    }
    if !matches!(f.parameters, sp::FunctionArguments::None) {
        return Err(unsupported("function call with a parametric argument list"));
    }
    let args = match &f.args {
        sp::FunctionArguments::None => Vec::new(),
        sp::FunctionArguments::List(list) => {
            if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
                return Err(unsupported("function call with DISTINCT/ORDER BY inside the argument list"));
            }
            if list.args.len() > dg.limits.max_columns {
                return Err(SqlError::ResourceLimit {
                    detail: "function call has too many arguments".to_string(),
                });
            }
            list.args
                .iter()
                .map(|a| match a {
                    sp::FunctionArg::Unnamed(sp::FunctionArgExpr::Expr(e)) => convert_expr(e, dg, depth + 1),
                    _ => Err(unsupported("named/wildcard function arguments")),
                })
                .collect::<Result<Vec<_>>>()?
        }
        sp::FunctionArguments::Subquery(_) => return Err(unsupported("function call with a bare subquery argument")),
    };
    Ok(Expr::Function {
        name: convert_object_name(&f.name, dg.limits)?,
        args,
    })
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
        return Err(unsupported("CREATE TABLE with OR REPLACE/TEMPORARY/EXTERNAL/AS SELECT/LIKE/CLONE"));
    }
    if ct.columns.len() > limits.max_columns {
        return Err(SqlError::ResourceLimit {
            detail: format!("CREATE TABLE column count exceeds max_columns ({})", limits.max_columns),
        });
    }
    let columns = ct
        .columns
        .iter()
        .map(|c| convert_column_def(c, limits))
        .collect::<Result<Vec<_>>>()?;

    let mut table_primary_key: Option<Vec<Ident>> = None;
    for constraint in &ct.constraints {
        match constraint {
            sp::TableConstraint::PrimaryKey(pk) => {
                if table_primary_key.is_some() {
                    return Err(unsupported("more than one table-level PRIMARY KEY"));
                }
                table_primary_key = Some(index_columns_to_idents(&pk.columns, limits)?);
            }
            sp::TableConstraint::Unique(_) => return Err(unsupported("table-level UNIQUE constraint (create the table, then CREATE UNIQUE INDEX)")),
            sp::TableConstraint::ForeignKey(_) => return Err(unsupported("FOREIGN KEY constraints")),
            sp::TableConstraint::Check(_) => return Err(unsupported("table-level CHECK constraints")),
            other => return Err(unsupported(format!("table constraint kind: {other:?}"))),
        }
    }

    Ok(CreateTable {
        name: convert_object_name(&ct.name, limits)?,
        if_not_exists: ct.if_not_exists,
        columns,
        table_primary_key,
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
                _ => Err(unsupported("index/constraint column must be a plain column name")),
            }
        })
        .collect()
}

fn convert_column_def(c: &sp::ColumnDef, limits: &SqlLimits) -> Result<ColumnDef> {
    let data_type = convert_data_type(&c.data_type)?;
    let mut nullable = true;
    let mut default = None;
    let mut primary_key = false;
    for opt in &c.options {
        match &opt.option {
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
                return Err(unsupported("inline UNIQUE column option (create the table, then CREATE UNIQUE INDEX)"))
            }
            other => return Err(unsupported(format!("column option kind: {other:?}"))),
        }
    }
    Ok(ColumnDef {
        name: convert_ident(&c.name, limits)?,
        data_type,
        nullable,
        default,
        primary_key,
    })
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
        return Err(unsupported("CREATE INDEX with USING/CONCURRENTLY/INCLUDE/WITH/WHERE/options"));
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
