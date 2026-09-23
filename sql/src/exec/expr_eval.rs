//! Runtime `BoundExpr` evaluation — item 16/17/18/62: SQL three-valued
//! logic, no reparsing, no new implicit coercion beyond what the binder
//! already decided (D21: an arithmetic/comparison `BoundExpr` reaching
//! this module already has matching operand types on both sides — the
//! binder's own `bind_shared` unification guarantees it, so this module
//! never coerces between types itself).

use rubixdb::relational::RelationalValue;

use crate::ast::{BinaryOp, UnaryOp};
use crate::bound::{BoundExpr, BoundExprKind};
use crate::error::{Result, SqlError};
use crate::exec::{ExecCtx, RowContext};

/// SQL three-valued logic (item 16/62). Never collapsed to a plain
/// `bool` — `Unknown` is `WHERE`'s own "discard the row" outcome,
/// distinct from (but behaviorally identical to, for `WHERE` alone)
/// `False`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    pub fn from_bool(b: bool) -> Tri {
        if b {
            Tri::True
        } else {
            Tri::False
        }
    }

    /// `WHERE`'s own keep/discard rule (item 16): only `True` keeps a
    /// row; `False` and `Unknown` both discard it.
    pub fn is_true(self) -> bool {
        matches!(self, Tri::True)
    }
}

fn tri_and(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::False, _) | (_, Tri::False) => Tri::False,
        (Tri::True, Tri::True) => Tri::True,
        _ => Tri::Unknown,
    }
}

fn tri_or(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::True, _) | (_, Tri::True) => Tri::True,
        (Tri::False, Tri::False) => Tri::False,
        _ => Tri::Unknown,
    }
}

fn tri_not(a: Tri) -> Tri {
    match a {
        Tri::True => Tri::False,
        Tri::False => Tri::True,
        Tri::Unknown => Tri::Unknown,
    }
}

/// Evaluates a boolean-typed `BoundExpr` (`WHERE`/`ON`/`HAVING`-shaped)
/// against three-valued logic. `AND`/`OR`/`NOT` are handled directly
/// here (never routed through `eval`'s own `Option<RelationalValue>`
/// shape) so the standard SQL short-circuit truth tables apply exactly
/// (item 62's own enumerated cases) — every other node falls back to
/// `eval` and interprets its `Boolean`/`NULL` result.
pub fn eval_predicate(expr: &BoundExpr, ctx: &RowContext, ec: &ExecCtx) -> Result<Tri> {
    match &expr.kind {
        BoundExprKind::BinaryOp {
            left,
            op: BinaryOp::And,
            right,
        } => Ok(tri_and(
            eval_predicate(left, ctx, ec)?,
            eval_predicate(right, ctx, ec)?,
        )),
        BoundExprKind::BinaryOp {
            left,
            op: BinaryOp::Or,
            right,
        } => Ok(tri_or(
            eval_predicate(left, ctx, ec)?,
            eval_predicate(right, ctx, ec)?,
        )),
        BoundExprKind::UnaryOp {
            op: UnaryOp::Not,
            expr,
        } => Ok(tri_not(eval_predicate(expr, ctx, ec)?)),
        _ => match eval(expr, ctx, ec)? {
            None => Ok(Tri::Unknown),
            Some(RelationalValue::Boolean(b)) => Ok(Tri::from_bool(b)),
            Some(_) => Err(SqlError::ExecutionParameter {
                detail: "predicate expression did not evaluate to a boolean".to_string(),
            }),
        },
    }
}

/// General value evaluation — item 17: column, literal, parameter,
/// arithmetic, comparison, `AND`/`OR`/`NOT`, `IS [NOT] NULL`, `IN`,
/// `BETWEEN`, `LIKE`, `CASE`, and the four scalar functions `crate::
/// functions::REGISTRY` binds (`length`/`upper`/`lower`/`abs`) — exactly
/// the `BoundExprKind` variants the binder currently produces, no more.
pub fn eval(expr: &BoundExpr, ctx: &RowContext, ec: &ExecCtx) -> Result<Option<RelationalValue>> {
    match &expr.kind {
        BoundExprKind::Literal(v) => Ok(v.clone()),
        BoundExprKind::Parameter { index } => ec.resolve_parameter(*index),
        BoundExprKind::Column(c) => Ok(ctx.get(c.table_ref.0, c.ordinal).cloned()),
        BoundExprKind::UnaryOp { op, expr } => {
            let v = eval(expr, ctx, ec)?;
            match op {
                UnaryOp::Not => Ok(v.map(|v| match v {
                    RelationalValue::Boolean(b) => RelationalValue::Boolean(!b),
                    other => other,
                })),
                UnaryOp::Neg => v.map(negate).transpose(),
            }
        }
        BoundExprKind::BinaryOp { left, op, right } => eval_binary(left, *op, right, ctx, ec),
        BoundExprKind::IsNull { expr, negated } => {
            let v = eval(expr, ctx, ec)?;
            Ok(Some(RelationalValue::Boolean(v.is_none() != *negated)))
        }
        BoundExprKind::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let lower = cmp_tri(expr, low, BinaryOp::GtEq, ctx, ec)?;
            let upper = cmp_tri(expr, high, BinaryOp::LtEq, ctx, ec)?;
            let t = tri_and(lower, upper);
            let t = if *negated { tri_not(t) } else { t };
            Ok(tri_to_bool(t))
        }
        BoundExprKind::InList {
            expr,
            list,
            negated,
        } => {
            let v = eval(expr, ctx, ec)?;
            let mut found_true = false;
            let mut found_unknown = false;
            for item in list {
                let iv = eval(item, ctx, ec)?;
                match values_eq(&v, &iv) {
                    Tri::True => found_true = true,
                    Tri::Unknown => found_unknown = true,
                    Tri::False => {}
                }
            }
            let t = if found_true {
                Tri::True
            } else if found_unknown {
                Tri::Unknown
            } else {
                Tri::False
            };
            let t = if *negated { tri_not(t) } else { t };
            Ok(tri_to_bool(t))
        }
        BoundExprKind::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => {
            let v = eval(expr, ctx, ec)?;
            let p = eval(pattern, ctx, ec)?;
            match (v, p) {
                (Some(RelationalValue::Text(v)), Some(RelationalValue::Text(p))) => {
                    let m = like_match(&v, &p, *case_insensitive);
                    Ok(Some(RelationalValue::Boolean(m != *negated)))
                }
                _ => Ok(None),
            }
        }
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => eval_case(
            operand.as_deref(),
            branches,
            else_result.as_deref(),
            ctx,
            ec,
        ),
        BoundExprKind::Function { name, args } => eval_function(name, args, ctx, ec),
    }
}

fn tri_to_bool(t: Tri) -> Option<RelationalValue> {
    match t {
        Tri::Unknown => None,
        Tri::True => Some(RelationalValue::Boolean(true)),
        Tri::False => Some(RelationalValue::Boolean(false)),
    }
}

fn cmp_tri(
    a: &BoundExpr,
    b: &BoundExpr,
    op: BinaryOp,
    ctx: &RowContext,
    ec: &ExecCtx,
) -> Result<Tri> {
    let av = eval(a, ctx, ec)?;
    let bv = eval(b, ctx, ec)?;
    compare(&av, &bv, op)
}

fn eval_binary(
    left: &BoundExpr,
    op: BinaryOp,
    right: &BoundExpr,
    ctx: &RowContext,
    ec: &ExecCtx,
) -> Result<Option<RelationalValue>> {
    match op {
        BinaryOp::And => Ok(tri_to_bool(tri_and(
            eval_predicate(left, ctx, ec)?,
            eval_predicate(right, ctx, ec)?,
        ))),
        BinaryOp::Or => Ok(tri_to_bool(tri_or(
            eval_predicate(left, ctx, ec)?,
            eval_predicate(right, ctx, ec)?,
        ))),
        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => {
            let av = eval(left, ctx, ec)?;
            let bv = eval(right, ctx, ec)?;
            Ok(tri_to_bool(compare(&av, &bv, op)?))
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            let av = eval(left, ctx, ec)?;
            let bv = eval(right, ctx, ec)?;
            match (av, bv) {
                (Some(a), Some(b)) => Ok(Some(arith(op, a, b)?)),
                _ => Ok(None),
            }
        }
    }
}

/// `a = b` per SQL equality (`NULL = NULL` is `Unknown`, never `True`)
/// — the same rule `CASE`'s operand form, `IN`, and `=` all share.
fn values_eq(a: &Option<RelationalValue>, b: &Option<RelationalValue>) -> Tri {
    compare(a, b, BinaryOp::Eq).unwrap_or(Tri::Unknown)
}

fn compare(a: &Option<RelationalValue>, b: &Option<RelationalValue>, op: BinaryOp) -> Result<Tri> {
    let (a, b) = match (a, b) {
        (Some(a), Some(b)) => (a, b),
        _ => return Ok(Tri::Unknown),
    };
    let ordering = value_cmp(a, b)?;
    let result = match op {
        BinaryOp::Eq => ordering == std::cmp::Ordering::Equal,
        BinaryOp::NotEq => ordering != std::cmp::Ordering::Equal,
        BinaryOp::Lt => ordering == std::cmp::Ordering::Less,
        BinaryOp::LtEq => ordering != std::cmp::Ordering::Greater,
        BinaryOp::Gt => ordering == std::cmp::Ordering::Greater,
        BinaryOp::GtEq => ordering != std::cmp::Ordering::Less,
        BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::Add
        | BinaryOp::Sub
        | BinaryOp::Mul
        | BinaryOp::Div
        | BinaryOp::Mod => {
            return Err(SqlError::ExecutionParameter {
                detail: "non-comparison operator reached value comparison".to_string(),
            })
        }
    };
    Ok(Tri::from_bool(result))
}

/// Same-variant comparison only — D21/item 18's own guarantee (the
/// binder's `bind_shared` unification already forces both operands of
/// any comparison/arithmetic `BoundExpr` to the identical `Relational
/// Type`, `Decimal`'s scale included) makes a mismatched pair here an
/// internal-consistency violation, not a possible user input.
fn value_cmp(a: &RelationalValue, b: &RelationalValue) -> Result<std::cmp::Ordering> {
    use RelationalValue::*;
    match (a, b) {
        (Boolean(x), Boolean(y)) => Ok(x.cmp(y)),
        (Integer(x), Integer(y)) => Ok(x.cmp(y)),
        (Bigint(x), Bigint(y)) => Ok(x.cmp(y)),
        (Real(x), Real(y)) => x.partial_cmp(y).ok_or_else(nan_error),
        (Double(x), Double(y)) => x.partial_cmp(y).ok_or_else(nan_error),
        (Decimal(x, sx), Decimal(y, sy)) if sx == sy => Ok(x.cmp(y)),
        (Text(x), Text(y)) => Ok(x.cmp(y)),
        (Blob(x), Blob(y)) => Ok(x.cmp(y)),
        (Date(x), Date(y)) => Ok(x.cmp(y)),
        (Time(x), Time(y)) => Ok(x.cmp(y)),
        (Timestamp(x), Timestamp(y)) => Ok(x.cmp(y)),
        _ => Err(SqlError::ExecutionParameter {
            detail: "comparison between mismatched value types (the binder already guarantees matching types; this indicates an internal inconsistency)".to_string(),
        }),
    }
}

fn nan_error() -> SqlError {
    SqlError::ExecutionParameter {
        detail: "NaN is not orderable".to_string(),
    }
}

fn negate(v: RelationalValue) -> Result<RelationalValue> {
    use RelationalValue::*;
    match v {
        Integer(x) => x.checked_neg().map(Integer).ok_or_else(overflow_error),
        Bigint(x) => x.checked_neg().map(Bigint).ok_or_else(overflow_error),
        Real(x) => Ok(Real(-x)),
        Double(x) => Ok(Double(-x)),
        Decimal(x, s) => x
            .checked_neg()
            .map(|v| Decimal(v, s))
            .ok_or_else(overflow_error),
        other => Ok(other),
    }
}

fn overflow_error() -> SqlError {
    SqlError::ExecutionParameter {
        detail: "arithmetic overflow".to_string(),
    }
}

fn div_by_zero_error() -> SqlError {
    SqlError::ExecutionParameter {
        detail: "division by zero".to_string(),
    }
}

/// Same-type arithmetic only — see `value_cmp`'s own doc comment for
/// why a mismatched pair here can only be an internal-consistency bug,
/// not user input. `DECIMAL` multiplication/division is deliberately
/// **not implemented** (`SqlError::ExecutionParameter`, a controlled
/// error) rather than silently producing a wrongly-scaled result: true
/// fixed-point decimal multiplication changes scale (the product of two
/// scale-`s` values is scale-`2s`), but `RelationalValue::Decimal`
/// carries no rescale primitive and the binder's own arithmetic-type
/// derivation (`l.ty.or(r.ty)`) does not adjust scale either — computing
/// a same-scale result here would be numerically wrong, not merely
/// unimplemented, so it is refused instead
/// (`PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE.md` §7 states this as
/// an explicit, honest scope boundary).
fn arith(op: BinaryOp, a: RelationalValue, b: RelationalValue) -> Result<RelationalValue> {
    use RelationalValue::*;
    match (a, b) {
        (Integer(x), Integer(y)) => int_arith(op, x, y).map(Integer),
        (Bigint(x), Bigint(y)) => bigint_arith(op, x, y).map(Bigint),
        (Real(x), Real(y)) => float_arith_f32(op, x, y).map(Real),
        (Double(x), Double(y)) => float_arith_f64(op, x, y).map(Double),
        (Decimal(x, sx), Decimal(y, sy)) if sx == sy => match op {
            BinaryOp::Add => x.checked_add(y).map(|v| Decimal(v, sx)).ok_or_else(overflow_error),
            BinaryOp::Sub => x.checked_sub(y).map(|v| Decimal(v, sx)).ok_or_else(overflow_error),
            _ => Err(SqlError::ExecutionParameter {
                detail: "DECIMAL multiplication/division/modulo is not implemented (would require scale rescaling this type does not support)".to_string(),
            }),
        },
        _ => Err(SqlError::ExecutionParameter {
            detail: "arithmetic between mismatched value types (the binder already guarantees matching types; this indicates an internal inconsistency)".to_string(),
        }),
    }
}

fn int_arith(op: BinaryOp, x: i32, y: i32) -> Result<i32> {
    match op {
        BinaryOp::Add => x.checked_add(y).ok_or_else(overflow_error),
        BinaryOp::Sub => x.checked_sub(y).ok_or_else(overflow_error),
        BinaryOp::Mul => x.checked_mul(y).ok_or_else(overflow_error),
        BinaryOp::Div => {
            if y == 0 {
                Err(div_by_zero_error())
            } else {
                x.checked_div(y).ok_or_else(overflow_error)
            }
        }
        BinaryOp::Mod => {
            if y == 0 {
                Err(div_by_zero_error())
            } else {
                x.checked_rem(y).ok_or_else(overflow_error)
            }
        }
        _ => unreachable!("only arithmetic ops reach int_arith"),
    }
}

fn bigint_arith(op: BinaryOp, x: i64, y: i64) -> Result<i64> {
    match op {
        BinaryOp::Add => x.checked_add(y).ok_or_else(overflow_error),
        BinaryOp::Sub => x.checked_sub(y).ok_or_else(overflow_error),
        BinaryOp::Mul => x.checked_mul(y).ok_or_else(overflow_error),
        BinaryOp::Div => {
            if y == 0 {
                Err(div_by_zero_error())
            } else {
                x.checked_div(y).ok_or_else(overflow_error)
            }
        }
        BinaryOp::Mod => {
            if y == 0 {
                Err(div_by_zero_error())
            } else {
                x.checked_rem(y).ok_or_else(overflow_error)
            }
        }
        _ => unreachable!("only arithmetic ops reach bigint_arith"),
    }
}

fn float_arith_f32(op: BinaryOp, x: f32, y: f32) -> Result<f32> {
    if matches!(op, BinaryOp::Div | BinaryOp::Mod) && y == 0.0 {
        return Err(div_by_zero_error());
    }
    Ok(match op {
        BinaryOp::Add => x + y,
        BinaryOp::Sub => x - y,
        BinaryOp::Mul => x * y,
        BinaryOp::Div => x / y,
        BinaryOp::Mod => x % y,
        _ => unreachable!("only arithmetic ops reach float_arith_f32"),
    })
}

fn float_arith_f64(op: BinaryOp, x: f64, y: f64) -> Result<f64> {
    if matches!(op, BinaryOp::Div | BinaryOp::Mod) && y == 0.0 {
        return Err(div_by_zero_error());
    }
    Ok(match op {
        BinaryOp::Add => x + y,
        BinaryOp::Sub => x - y,
        BinaryOp::Mul => x * y,
        BinaryOp::Div => x / y,
        BinaryOp::Mod => x % y,
        _ => unreachable!("only arithmetic ops reach float_arith_f64"),
    })
}

/// `%` = any sequence (including empty), `_` = exactly one character.
/// No `ESCAPE` clause (the bound grammar this increment executes has
/// none to represent — item 17's own "only where actual current binder
/// support exists"). Case-insensitive compares the lowercased form of
/// both strings.
fn like_match(value: &str, pattern: &str, case_insensitive: bool) -> bool {
    let (v, p): (String, String) = if case_insensitive {
        (value.to_lowercase(), pattern.to_lowercase())
    } else {
        (value.to_string(), pattern.to_string())
    };
    like_match_chars(
        &v.chars().collect::<Vec<_>>(),
        &p.chars().collect::<Vec<_>>(),
    )
}

fn like_match_chars(v: &[char], p: &[char]) -> bool {
    match p.first() {
        None => v.is_empty(),
        Some('%') => {
            like_match_chars(v, &p[1..]) || (!v.is_empty() && like_match_chars(&v[1..], p))
        }
        Some('_') => !v.is_empty() && like_match_chars(&v[1..], &p[1..]),
        Some(c) => v.first() == Some(c) && like_match_chars(&v[1..], &p[1..]),
    }
}

fn eval_case(
    operand: Option<&BoundExpr>,
    branches: &[(BoundExpr, BoundExpr)],
    else_result: Option<&BoundExpr>,
    ctx: &RowContext,
    ec: &ExecCtx,
) -> Result<Option<RelationalValue>> {
    match operand {
        Some(operand) => {
            let ov = eval(operand, ctx, ec)?;
            for (when, then) in branches {
                let wv = eval(when, ctx, ec)?;
                if values_eq(&ov, &wv) == Tri::True {
                    return eval(then, ctx, ec);
                }
            }
        }
        None => {
            for (when, then) in branches {
                if eval_predicate(when, ctx, ec)?.is_true() {
                    return eval(then, ctx, ec);
                }
            }
        }
    }
    match else_result {
        Some(e) => eval(e, ctx, ec),
        None => Ok(None),
    }
}

/// Item 17's four scalar functions — `crate::functions::REGISTRY`
/// verbatim, never a fifth invented here (that registry, not this
/// match, is the single source of truth for what function names bind
/// at all; an unbound name never reaches this function).
fn eval_function(
    name: &str,
    args: &[BoundExpr],
    ctx: &RowContext,
    ec: &ExecCtx,
) -> Result<Option<RelationalValue>> {
    let values: Vec<Option<RelationalValue>> = args
        .iter()
        .map(|a| eval(a, ctx, ec))
        .collect::<Result<_>>()?;
    // Every current registry entry is `null_propagates: true`.
    if values.iter().any(Option::is_none) {
        return Ok(None);
    }
    let values: Vec<RelationalValue> = values
        .into_iter()
        .map(|v| v.expect("checked above"))
        .collect();
    match name {
        "length" => match &values[0] {
            RelationalValue::Text(s) => {
                Ok(Some(RelationalValue::Integer(s.chars().count() as i32)))
            }
            _ => Err(function_type_error("length")),
        },
        "upper" => match &values[0] {
            RelationalValue::Text(s) => Ok(Some(RelationalValue::Text(s.to_uppercase()))),
            _ => Err(function_type_error("upper")),
        },
        "lower" => match &values[0] {
            RelationalValue::Text(s) => Ok(Some(RelationalValue::Text(s.to_lowercase()))),
            _ => Err(function_type_error("lower")),
        },
        "abs" => match &values[0] {
            RelationalValue::Integer(x) => x
                .checked_abs()
                .map(RelationalValue::Integer)
                .ok_or_else(overflow_error)
                .map(Some),
            RelationalValue::Bigint(x) => x
                .checked_abs()
                .map(RelationalValue::Bigint)
                .ok_or_else(overflow_error)
                .map(Some),
            RelationalValue::Real(x) => Ok(Some(RelationalValue::Real(x.abs()))),
            RelationalValue::Double(x) => Ok(Some(RelationalValue::Double(x.abs()))),
            RelationalValue::Decimal(x, s) => x
                .checked_abs()
                .map(|v| RelationalValue::Decimal(v, *s))
                .ok_or_else(overflow_error)
                .map(Some),
            _ => Err(function_type_error("abs")),
        },
        other => Err(SqlError::UnsupportedExecution {
            detail: format!("function '{other}' has no runtime implementation"),
        }),
    }
}

fn function_type_error(name: &str) -> SqlError {
    SqlError::ExecutionParameter {
        detail: format!("'{name}' received an argument of the wrong type (the binder already guarantees matching types; this indicates an internal inconsistency)"),
    }
}
