//! Binding of set operations (SQL-mission-1 Slice E): `UNION`, `INTERSECT`,
//! `EXCEPT`, each with or without `ALL`.
//!
//! A set operation is bound as a **shell** `BoundSelect` (`set_op: Some`)
//! whose two operands are ordinary bound queries -- each one is bound by the
//! same `bind_select_in` any query is, against the same enclosing-query chain
//! (so an operand may be correlated, may contain subqueries, derived tables
//! and CTE references, and every table it names is authorised at bind time
//! exactly as it would be on its own).
//!
//! # Rules
//!
//! * **Column count** -- both operands must produce the same number of columns.
//! * **Column names** -- the first operand's output names.
//! * **Column types** -- one type per column, from the widening table in
//!   [`unify_types`]. Numeric types widen to the wider one; every other type
//!   combines with itself only; anything else is a bind error. Nothing is ever
//!   converted to `TEXT`, and no non-numeric conversion exists.
//! * **`ORDER BY` / `LIMIT` / `OFFSET`** written after the last operand apply
//!   to the whole result. `ORDER BY` can name the operation's *output columns*
//!   (the first operand's names) and expressions over them -- not a column of
//!   any table an operand reads.
//! * **`WITH`** written in front of the operation is visible in every operand.

use rubixdb::relational::RelationalType;

use crate::ast;
use crate::bind::env::BindEnv;
use crate::bind::expr::{is_numeric, ExprBinder};
use crate::bind::scope::{self, CteBinding, OuterScopes, ScopeEntry};
use crate::bound::{
    BoundExpr, BoundExprKind, BoundOrderByItem, BoundSelect, BoundSelectItem, BoundSetOp,
    ColumnRef, NullsOrder, TableRefId, SYNTHETIC_TABLE_ID,
};
use crate::error::{Result, SqlError};
use crate::limits::SqlLimits;

/// Name of the one `FROM`-less relation a set operation's `ORDER BY` is bound
/// against. It is not a valid SQL identifier, so no user text can qualify a
/// column with it.
const OUTPUT_RELATION: &str = "?set?";

/// `SQL type name` of a relational type, for error messages.
pub fn type_name(ty: RelationalType) -> String {
    match ty {
        RelationalType::Boolean => "BOOLEAN".to_string(),
        RelationalType::Integer => "INTEGER".to_string(),
        RelationalType::Bigint => "BIGINT".to_string(),
        RelationalType::Real => "REAL".to_string(),
        RelationalType::Double => "DOUBLE".to_string(),
        RelationalType::Decimal { scale } => format!("DECIMAL(scale {scale})"),
        RelationalType::Text => "TEXT".to_string(),
        RelationalType::Blob => "BLOB".to_string(),
        RelationalType::Date => "DATE".to_string(),
        RelationalType::Time => "TIME".to_string(),
        RelationalType::Timestamp => "TIMESTAMP".to_string(),
    }
}

/// The one type a column of a set operation has when its two operands give it
/// types `a` and `b`; `None` when no such type exists.
///
/// | left \ right | INTEGER | BIGINT | REAL | DOUBLE | DECIMAL(s2) | other |
/// |---|---|---|---|---|---|---|
/// | **INTEGER** | INTEGER | BIGINT | DOUBLE | DOUBLE | DECIMAL(s2) | - |
/// | **BIGINT** | BIGINT | BIGINT | DOUBLE | DOUBLE | DECIMAL(s2) | - |
/// | **REAL** | DOUBLE | DOUBLE | REAL | DOUBLE | DOUBLE | - |
/// | **DOUBLE** | DOUBLE | DOUBLE | DOUBLE | DOUBLE | DOUBLE | - |
/// | **DECIMAL(s1)** | DECIMAL(s1) | DECIMAL(s1) | DOUBLE | DOUBLE | DECIMAL(max(s1, s2)) | - |
///
/// Every non-numeric type (BOOLEAN, TEXT, BLOB, DATE, TIME, TIMESTAMP)
/// combines with the same type only. The relation is symmetric and
/// associative, so the result does not depend on the order of the operands.
/// Widening an exact number to DOUBLE rounds a `BIGINT` or `DECIMAL` whose
/// magnitude exceeds 2^53; that is the only information a conversion can lose.
pub fn unify_types(a: RelationalType, b: RelationalType) -> Option<RelationalType> {
    use RelationalType::*;
    if a == b {
        return Some(a);
    }
    match (a, b) {
        (Integer, Bigint) | (Bigint, Integer) => Some(Bigint),
        (Decimal { scale: x }, Decimal { scale: y }) => Some(Decimal { scale: x.max(y) }),
        (Integer | Bigint, Decimal { scale }) | (Decimal { scale }, Integer | Bigint) => {
            Some(Decimal { scale })
        }
        // `a == b` is handled above, so these are the mixed pairs.
        (Double, n) | (n, Double) if is_numeric(n) => Some(Double),
        (Real, n) | (n, Real) if is_numeric(n) => Some(Double),
        _ => None,
    }
}

/// An operand that is itself a set operation without a `WITH`, `ORDER BY` or
/// `LIMIT` of its own: its operator chain can be walked in a loop instead of
/// by recursion.
fn is_bare_set_operation(s: &ast::Select) -> bool {
    s.set_op.is_some()
        && s.ctes.is_empty()
        && s.order_by.is_empty()
        && s.limit.is_none()
        && s.offset.is_none()
}

/// Binds the set operation `stmt` (`stmt.set_op` is `Some`). `inherited` is
/// the CTEs of the enclosing set operations, visible in every operand.
pub(super) fn bind_set_operation(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    stmt: &ast::Select,
    outer: Option<&OuterScopes<'_>>,
    inherited: &[CteBinding],
) -> Result<BoundSelect> {
    let top = stmt
        .set_op
        .as_ref()
        .expect("the caller checked that this query is a set operation");
    let mut max_parameter = 0u32;

    // The operation's own WITH list, visible in every operand.
    let mut scope = env.new_scope();
    scope.ctes = inherited.to_vec();
    let ctes = super::select::bind_with_list(
        env,
        limits,
        &stmt.ctes,
        &mut scope,
        outer,
        &mut max_parameter,
    )?;
    let visible = scope.ctes.clone();

    // `a UNION b UNION c` is `(a UNION b) UNION c`: walk the left spine in a
    // loop so a long chain costs no native stack, then fold the operands in.
    let mut chain = vec![top];
    let mut base: &ast::Select = &top.left;
    while is_bare_set_operation(base) {
        let node = base.set_op.as_ref().expect("checked above");
        chain.push(node);
        base = &node.left;
    }

    let mut acc = super::select::bind_select_in_with(env, limits, base, outer, &visible)?;
    for node in chain.iter().rev() {
        let right = super::select::bind_select_in_with(env, limits, &node.right, outer, &visible)?;
        acc = combine(env, node.op, node.all, acc, right)?;
    }
    max_parameter = max_parameter.max(acc.max_parameter);

    // The clauses written after the last operand belong to the whole result.
    acc.ctes = ctes;
    let (set_ref, output_names) = {
        let set = acc.set_op.as_ref().expect("combine builds a set operation");
        (
            set.set_ref,
            acc.projection
                .iter()
                .map(|i| i.output_name.clone())
                .collect::<Vec<_>>(),
        )
    };

    if !stmt.order_by.is_empty() {
        acc.order_by = bind_order_by(env, limits, stmt, &acc, set_ref, &output_names, &mut max_parameter)?;
    }

    let mut int_binder = ExprBinder::new(None, limits);
    acc.limit = stmt
        .limit
        .as_ref()
        .map(|e| int_binder.bind(e, Some(RelationalType::Bigint)))
        .transpose()?;
    acc.offset = stmt
        .offset
        .as_ref()
        .map(|e| int_binder.bind(e, Some(RelationalType::Bigint)))
        .transpose()?;
    max_parameter = max_parameter.max(int_binder.max_parameter);
    acc.max_parameter = max_parameter;
    Ok(acc)
}

/// `ORDER BY` of a set operation: bound against the operation's output columns
/// and nothing else -- no table of any operand, no enclosing query.
fn bind_order_by(
    env: &BindEnv<'_>,
    limits: &SqlLimits,
    stmt: &ast::Select,
    shell: &BoundSelect,
    set_ref: u32,
    output_names: &[String],
    max_parameter: &mut u32,
) -> Result<Vec<BoundOrderByItem>> {
    let typed: Vec<(String, Option<RelationalType>, bool)> = shell
        .projection
        .iter()
        .zip(output_names)
        .map(|(item, name)| (name.clone(), item.expr.ty, item.expr.nullable))
        .collect();
    let columns = scope::relation_columns("the set operation", &typed)?;
    let mut order_scope = env.new_scope();
    order_scope.entries.push(ScopeEntry {
        table_ref_id: set_ref,
        resolved: scope::synthetic_resolved(OUTPUT_RELATION, columns),
        effective_name: OUTPUT_RELATION.to_string(),
        null_extended: false,
    });

    stmt.order_by
        .iter()
        .map(|item| -> Result<BoundOrderByItem> {
            let mut eb = ExprBinder::new(Some(&order_scope), limits);
            let bound = match eb.bind(&item.expr, None) {
                Err(SqlError::UnknownObject { kind, detail })
                    if kind == "column" || kind == "table reference" =>
                {
                    return Err(SqlError::InvalidIdentifier {
                        detail: format!(
                            "ORDER BY of a set operation can only name the output columns of its first query ({}); '{detail}' is not one",
                            output_names.join(", ")
                        ),
                    });
                }
                other => other?,
            };
            *max_parameter = (*max_parameter).max(eb.max_parameter);
            // D5 default: NULLS LAST ascending / NULLS FIRST descending.
            let nulls = match item.nulls_first {
                Some(true) => NullsOrder::First,
                Some(false) => NullsOrder::Last,
                None if item.descending => NullsOrder::First,
                None => NullsOrder::Last,
            };
            Ok(BoundOrderByItem {
                expr: bound,
                descending: item.descending,
                nulls,
            })
        })
        .collect()
}

/// `left <op> [ALL] right` as a shell query: checks the column counts, unifies
/// the column types and names the output after the left operand.
pub(super) fn combine(
    env: &BindEnv<'_>,
    op: ast::SetOperator,
    all: bool,
    left: BoundSelect,
    right: BoundSelect,
) -> Result<BoundSelect> {
    let name = op.name();
    if left.projection.len() != right.projection.len() {
        return Err(SqlError::TypeMismatch {
            detail: format!(
                "each {name} query must have the same number of columns (the left one has {}, the right one has {})",
                left.projection.len(),
                right.projection.len()
            ),
        });
    }
    let set_ref = env.alloc_table_ref();
    let mut column_types = Vec::with_capacity(left.projection.len());
    let mut projection = Vec::with_capacity(left.projection.len());
    for (i, (l, r)) in left.projection.iter().zip(&right.projection).enumerate() {
        let ty = match (l.expr.ty, r.expr.ty) {
            (Some(a), Some(b)) => Some(unify_types(a, b).ok_or_else(|| SqlError::TypeMismatch {
                detail: format!(
                    "{name} column {} cannot combine {} with {}: only numeric types convert to a wider one, any other type combines with itself only",
                    i + 1,
                    type_name(a),
                    type_name(b)
                ),
            })?),
            (Some(t), None) | (None, Some(t)) => Some(t),
            (None, None) => None,
        };
        column_types.push(ty);
        projection.push(BoundSelectItem {
            expr: BoundExpr {
                kind: BoundExprKind::Column(ColumnRef {
                    table_ref: TableRefId(set_ref),
                    table_id: SYNTHETIC_TABLE_ID,
                    ordinal: i as u16,
                }),
                ty,
                nullable: l.expr.nullable || r.expr.nullable,
            },
            // The first operand names the columns.
            output_name: l.output_name.clone(),
        });
    }
    let max_parameter = left.max_parameter.max(right.max_parameter);
    Ok(BoundSelect {
        ctes: Vec::new(),
        set_op: Some(Box::new(BoundSetOp {
            op,
            all,
            left,
            right,
            set_ref,
            column_types,
            left_plan: None,
            right_plan: None,
        })),
        distinct: false,
        from: Vec::new(),
        projection,
        selection: None,
        group_by: Vec::new(),
        grouping: None,
        aggregates: Vec::new(),
        having: None,
        windows: Vec::new(),
        qualify: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        max_parameter,
    })
}
