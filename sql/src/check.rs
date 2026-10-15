//! `CHECK` constraints -- `docs/adr/ADR-DDL-FK-01.md` (D8).
//!
//! The catalog stores a constraint as its expression's SQL text
//! (`ConstraintRow::check_expression`) and the ordinals of the columns it reads
//! (so a rename of one can be refused). Whenever a statement writes a table's
//! rows, the table's constraints are parsed and bound again against the table's
//! *current* columns -- the same pipeline a `WHERE` clause goes through -- and
//! evaluated per written row with the engine's own evaluator.
//!
//! A row violates a constraint when the expression evaluates to `FALSE`; `TRUE` and
//! `NULL` (unknown) pass, as in the SQL standard. An expression may read the
//! table's own columns and use literals, operators and scalar functions; it may not
//! contain a subquery, an aggregate, a window function or a parameter.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use rubixdb::catalog::schema::{ColumnRow, ConstraintRow};
use rubixdb::catalog::CatalogService;
use rubixdb::relational::{RelationalType, RelationalValue};

use crate::ast;
use crate::bind::expr::ExprBinder;
use crate::bind::scope::{self, Scope};
use crate::bound::{BoundExpr, BoundExprKind};
use crate::error::{Result, SqlError};
use crate::exec::expr_eval::eval;
use crate::exec::{ExecCtx, RowContext};
use crate::limits::SqlLimits;

/// A bound constraint, ready to evaluate against rows of its table.
#[derive(Debug, Clone)]
pub struct CompiledCheck {
    pub name: String,
    pub expr: BoundExpr,
    /// The table-reference id the expression's column references use.
    pub table_ref: u32,
}

/// What binding an expression found out about it.
pub struct BoundCheck {
    pub expr: BoundExpr,
    pub table_ref: u32,
    /// The columns it reads, sorted and distinct.
    pub columns: Vec<u16>,
}

/// Binds `expr` against `columns` (the table's columns, ordinal order).
pub fn bind_check(
    table_name: &str,
    columns: &[ColumnRow],
    expr: &ast::Expr,
    limits: &SqlLimits,
) -> Result<BoundCheck> {
    let mut scope = Scope::with_counter(Rc::new(Cell::new(0)));
    let table_ref = scope.push(
        scope::synthetic_resolved(table_name, columns.to_vec()),
        None,
        false,
    );
    let mut binder = ExprBinder::new(Some(&scope), limits);
    let bound = binder.bind(expr, Some(RelationalType::Boolean))?;
    if binder.max_parameter > 0 {
        return Err(SqlError::Unsupported {
            detail: "a CHECK expression cannot use parameters".to_string(),
        });
    }
    let forbidden = |e: &BoundExpr| {
        matches!(
            e.kind,
            BoundExprKind::Subquery(_)
                | BoundExprKind::Aggregate(_)
                | BoundExprKind::Window(_)
                | BoundExprKind::Parameter { .. }
        )
    };
    if bound.any(&forbidden) {
        return Err(SqlError::Unsupported {
            detail:
                "a CHECK expression cannot contain a subquery, an aggregate or a window function"
                    .to_string(),
        });
    }
    if bound.ty != Some(RelationalType::Boolean) {
        return Err(SqlError::TypeMismatch {
            detail: "a CHECK expression must be a boolean condition".to_string(),
        });
    }
    let read = RefCell::new(Vec::new());
    bound.any(&|e| {
        if let BoundExprKind::Column(c) = &e.kind {
            read.borrow_mut().push(c.ordinal);
        }
        false
    });
    let mut columns = read.into_inner();
    columns.sort_unstable();
    columns.dedup();
    Ok(BoundCheck {
        expr: bound,
        table_ref,
        columns,
    })
}

/// Parses a stored expression text back into an expression.
pub fn parse_check_text(text: &str, limits: &SqlLimits) -> Result<ast::Expr> {
    // The text came from this module's own writer, but it is parsed through the same
    // front door as any other SQL, so every limit applies.
    let stmt = crate::parse::parse_statement(&format!("SELECT 1 FROM c WHERE {text}"), limits)?;
    match stmt {
        ast::Statement::Select(select) => select.selection.ok_or_else(|| {
            SqlError::Catalog("a stored CHECK expression did not parse as a condition".to_string())
        }),
        _ => Err(SqlError::Catalog(
            "a stored CHECK expression did not parse as a condition".to_string(),
        )),
    }
}

/// The constraints a statement writing `table_id` must satisfy, bound against the
/// table's current columns. Empty (and cheap) for a table without any.
pub fn compile_table_checks(
    catalog: &CatalogService,
    table_id: u32,
    limits: &SqlLimits,
) -> Result<Vec<CompiledCheck>> {
    let stored: Vec<ConstraintRow> = catalog.check_constraints_for(table_id)?;
    if stored.is_empty() {
        return Ok(Vec::new());
    }
    let table = catalog.get_table(table_id)?.ok_or_else(|| {
        SqlError::Storage("a target table no longer resolves in the catalog".to_string())
    })?;
    let columns = catalog.get_columns(table_id)?;
    stored
        .iter()
        .map(|c| {
            let text = c.check_expression.as_deref().unwrap_or("");
            let ast = parse_check_text(text, limits)?;
            let bound = bind_check(&table.name, &columns, &ast, limits)?;
            Ok(CompiledCheck {
                name: c.name.clone(),
                expr: bound.expr,
                table_ref: bound.table_ref,
            })
        })
        .collect()
}

/// The first constraint `row` violates, if any.
pub fn first_violation<'a>(
    checks: &'a [CompiledCheck],
    row: &[Option<RelationalValue>],
    ec: &ExecCtx<'_>,
) -> Result<Option<&'a CompiledCheck>> {
    for check in checks {
        let ctx = RowContext::single(check.table_ref, row.to_vec());
        if let Some(RelationalValue::Boolean(false)) = eval(&check.expr, &ctx, ec)? {
            return Ok(Some(check));
        }
    }
    Ok(None)
}

/// The error a violated constraint raises: names the constraint and the table, never
/// a value.
pub fn violation(table: &str, check: &CompiledCheck) -> SqlError {
    SqlError::CheckViolation {
        detail: format!(
            "new row for table \"{table}\" violates check constraint \"{}\"",
            check.name
        ),
    }
}
