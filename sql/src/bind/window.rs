//! Window function binding (SQL-mission-2): `f(args) OVER (...)`,
//! `WINDOW name AS (...)`, frames, and `GROUPING(...)`.
//!
//! Window functions are legal in the SELECT list, `QUALIFY` and `ORDER BY`
//! (`ExprBinder::allow_window`); everywhere else the binder refuses them with a
//! `TypeMismatch`, exactly as it refuses an aggregate in `WHERE`.
//!
//! The expressions inside a window call (arguments, `PARTITION BY`, `ORDER BY`,
//! frame offsets) are bound by a *sub-binder* that shares this binder's scope
//! and outer-query chain but has no `env`, so they may read the query's columns
//! (and record outer-column captures for correlation) yet may not contain a
//! subquery or a nested window function.

use std::collections::HashMap;

use rubixdb::relational::{RelationalType, RelationalValue};

use crate::aggregate::{AggregateArg, AggregateContract};
use crate::ast::{self, FrameUnits};
use crate::bind::expr::{is_numeric, ExprBinder};
use crate::bound::{
    BoundExpr, BoundExprKind, BoundFrame, BoundFrameBound, BoundOrderByItem, BoundWindowCall,
    BoundWindowFunc, BoundWindowSpec, NullsOrder,
};
use crate::error::{Result, SqlError};

/// `WINDOW name AS (...)` definitions, keyed by name.
pub type NamedWindows = HashMap<String, ast::WindowSpec>;

fn err(detail: impl Into<String>) -> SqlError {
    SqlError::TypeMismatch {
        detail: detail.into(),
    }
}

/// A window specification with every named reference followed.
struct Resolved<'c> {
    partition_by: Vec<&'c ast::Expr>,
    order_by: Vec<&'c ast::OrderByItem>,
    frame: Option<&'c ast::FrameSpec>,
}

/// Names may chain (`w2 AS (w1 ORDER BY x)`); a definition that cycles or nests
/// deeper than this is refused.
const MAX_WINDOW_NAME_DEPTH: usize = 16;

fn lookup_named<'c>(
    named: Option<&'c NamedWindows>,
    name: &str,
    depth: usize,
) -> Result<Resolved<'c>> {
    let spec = named
        .and_then(|n| n.get(name))
        .ok_or_else(|| SqlError::InvalidIdentifier {
            detail: format!(
                "window '{name}' is not defined (define it with WINDOW {name} AS (...))"
            ),
        })?;
    resolve_spec(named, spec, depth + 1)
}

fn resolve_spec<'c>(
    named: Option<&'c NamedWindows>,
    spec: &'c ast::WindowSpec,
    depth: usize,
) -> Result<Resolved<'c>> {
    if depth > MAX_WINDOW_NAME_DEPTH {
        return Err(SqlError::InvalidIdentifier {
            detail: "window definitions refer to each other too deeply, or in a cycle".to_string(),
        });
    }
    let mut resolved = match &spec.base {
        Some(base) => {
            let b = lookup_named(named, &base.value, depth)?;
            // SQL standard (ISO/IEC 9075-2 7.15): a window that extends another
            // may not repeat its PARTITION BY, may add an ORDER BY only if the
            // base has none, and may add a frame only if the base has none.
            if !spec.partition_by.is_empty() {
                return Err(err(format!(
                    "a window that refers to window '{}' cannot have its own PARTITION BY",
                    base.value
                )));
            }
            if !spec.order_by.is_empty() && !b.order_by.is_empty() {
                return Err(err(format!(
                    "a window that refers to window '{}' cannot override its ORDER BY",
                    base.value
                )));
            }
            if spec.frame.is_some() && b.frame.is_some() {
                return Err(err(format!(
                    "a window that refers to window '{}' cannot override its frame",
                    base.value
                )));
            }
            b
        }
        None => Resolved {
            partition_by: spec.partition_by.iter().collect(),
            order_by: Vec::new(),
            frame: None,
        },
    };
    if !spec.order_by.is_empty() {
        resolved.order_by = spec.order_by.iter().collect();
    }
    if spec.frame.is_some() {
        resolved.frame = spec.frame.as_ref();
    }
    Ok(resolved)
}

/// A constant for the purposes of window offsets (`NTILE(n)`, `LAG` offsets,
/// frame offsets): a literal, a parameter, or arithmetic on them.
fn is_constant(e: &BoundExpr) -> bool {
    match &e.kind {
        BoundExprKind::Literal(_) | BoundExprKind::Parameter { .. } => true,
        BoundExprKind::UnaryOp { expr, .. } => is_constant(expr),
        BoundExprKind::BinaryOp { left, right, .. } => is_constant(left) && is_constant(right),
        _ => false,
    }
}

fn literal_is_negative(e: &BoundExpr) -> bool {
    match &e.kind {
        BoundExprKind::Literal(Some(v)) => match v {
            RelationalValue::Integer(n) => *n < 0,
            RelationalValue::Bigint(n) => *n < 0,
            RelationalValue::Real(n) => *n < 0.0,
            RelationalValue::Double(n) => *n < 0.0,
            RelationalValue::Decimal(n, _) => *n < 0,
            _ => false,
        },
        _ => false,
    }
}

impl<'a> ExprBinder<'a> {
    /// A binder for the expressions inside a window call: same scope, outer
    /// chain, aggregate permission and grouping context, but no `env` (no
    /// subqueries) and no window functions.
    fn window_sub_binder(&self) -> ExprBinder<'a> {
        let mut sub = ExprBinder::new(self.scope, self.limits);
        sub.allow_aggregate = self.allow_aggregate;
        sub.outer = self.outer;
        sub.capture_env = self.env.or(self.capture_env);
        sub.group_exprs = self.group_exprs;
        sub.aliases = self.aliases;
        sub
    }

    fn absorb(&mut self, sub: &ExprBinder<'_>) {
        self.max_parameter = self.max_parameter.max(sub.max_parameter);
        self.local_column_refs += sub.local_column_refs;
        self.outer_column_refs += sub.outer_column_refs;
    }

    pub(super) fn bind_window(&mut self, call: &ast::WindowCall) -> Result<BoundExpr> {
        if !self.allow_window {
            return Err(err(
                "window functions are not allowed in this context (they are allowed in the SELECT list, QUALIFY and ORDER BY)",
            ));
        }
        let resolved = match &call.over {
            ast::WindowOver::Named(n) => lookup_named(self.named_windows, &n.value, 0)?,
            ast::WindowOver::Spec(spec) => resolve_spec(self.named_windows, spec, 0)?,
        };
        let mut sub = self.window_sub_binder();
        let result = bind_window_call(&mut sub, call, &resolved);
        self.absorb(&sub);
        let (func, spec, ty, nullable) = result?;
        Ok(BoundExpr {
            kind: BoundExprKind::Window(Box::new(BoundWindowCall { func, spec })),
            ty,
            nullable,
        })
    }

    pub(super) fn bind_grouping(&mut self, args: &[ast::Expr]) -> Result<BoundExpr> {
        let Some(groups) = self.group_exprs else {
            return Err(err("GROUPING is not allowed in this context"));
        };
        // The result is a 32-bit bit string (first argument most significant).
        if args.len() > 31 {
            return Err(err("GROUPING takes at most 31 arguments"));
        }
        let mut sub = self.window_sub_binder();
        sub.allow_aggregate = false;
        let mut indexes = Vec::with_capacity(args.len());
        for a in args {
            let bound = sub.bind(a, None);
            let bound = match bound {
                Err(SqlError::TypeMismatch { detail })
                    if detail == "aggregate functions are not allowed in this context" =>
                {
                    return Err(err(
                        "an aggregate function cannot be an argument of GROUPING",
                    ));
                }
                other => other?,
            };
            let idx = groups.iter().position(|g| *g == bound).ok_or_else(|| {
                err("every argument of GROUPING must be one of the expressions in the GROUP BY clause")
            })?;
            indexes.push(idx);
        }
        self.absorb(&sub);
        Ok(BoundExpr {
            kind: BoundExprKind::Grouping(indexes),
            ty: Some(RelationalType::Integer),
            nullable: false,
        })
    }
}

type BoundCall = (
    BoundWindowFunc,
    BoundWindowSpec,
    Option<RelationalType>,
    bool,
);

fn bind_window_call(
    b: &mut ExprBinder<'_>,
    call: &ast::WindowCall,
    resolved: &Resolved<'_>,
) -> Result<BoundCall> {
    // PARTITION BY / ORDER BY.
    let partition_by = resolved
        .partition_by
        .iter()
        .map(|e| b.bind(e, None))
        .collect::<Result<Vec<_>>>()?;
    let order_by = resolved
        .order_by
        .iter()
        .map(|item| {
            let expr = b.bind(&item.expr, None)?;
            // D5 default, as for the statement's own ORDER BY: NULLS LAST
            // ascending, NULLS FIRST descending.
            let nulls = match item.nulls_first {
                Some(true) => NullsOrder::First,
                Some(false) => NullsOrder::Last,
                None if item.descending => NullsOrder::First,
                None => NullsOrder::Last,
            };
            Ok(BoundOrderByItem {
                expr,
                descending: item.descending,
                nulls,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let explicit_frame = resolved.frame.is_some();
    let (func, ty, nullable, frame_allowed, order_required) = match &call.function {
        ast::WindowFunction::Aggregate {
            func,
            arg,
            distinct,
            filter,
        } => {
            if call.ignore_nulls {
                return Err(err(format!(
                    "IGNORE NULLS is not accepted by {}; it applies to LAG, LEAD, FIRST_VALUE, LAST_VALUE and NTH_VALUE",
                    func.name().to_ascii_uppercase()
                )));
            }
            let (bound_arg, in_ty) = match arg {
                AggregateArg::Wildcard => (AggregateArg::Wildcard, None),
                AggregateArg::Expr(e) => {
                    let bound = bind_window_arg(b, e)?;
                    let ty = bound.ty;
                    (AggregateArg::Expr(Box::new(bound)), ty)
                }
                AggregateArg::Multi(es) => {
                    if *func != crate::aggregate::AggregateFunc::Count {
                        return Err(err(format!(
                            "aggregate function {} takes exactly one argument",
                            func.name()
                        )));
                    }
                    let bound = es
                        .iter()
                        .map(|e| bind_window_arg(b, e).map(Box::new))
                        .collect::<Result<Vec<_>>>()?;
                    (AggregateArg::Multi(bound), None)
                }
            };
            let wildcard = matches!(bound_arg, AggregateArg::Wildcard);
            AggregateContract::check_argument_type(*func, wildcard, in_ty)?;
            let (ty, nullable) = AggregateContract::derive_return_type(*func, wildcard, in_ty)?;
            let filter = match filter {
                None => None,
                Some(f) => {
                    let saved = b.allow_aggregate;
                    b.allow_aggregate = false;
                    let bound = b.bind(f, Some(RelationalType::Boolean));
                    b.allow_aggregate = saved;
                    let bound = bound?;
                    if let Some(t) = bound.ty {
                        if t != RelationalType::Boolean {
                            return Err(err(
                                "the FILTER condition of an aggregate must be boolean",
                            ));
                        }
                    }
                    Some(Box::new(bound))
                }
            };
            (
                BoundWindowFunc::Aggregate {
                    func: *func,
                    arg: bound_arg,
                    distinct: *distinct,
                    filter,
                },
                Some(ty),
                nullable,
                true,
                false,
            )
        }
        ast::WindowFunction::Named { name, args } => {
            bind_named_function(b, name, args, call.ignore_nulls)?
        }
    };

    if explicit_frame && !frame_allowed {
        return Err(err(format!(
            "{} does not take a window frame (ROWS / RANGE); remove the frame clause",
            func.name().to_ascii_uppercase()
        )));
    }
    if order_required && order_by.is_empty() {
        return Err(err(format!(
            "{} requires the window to have an ORDER BY",
            func.name().to_ascii_uppercase()
        )));
    }
    let frame = bind_frame(b, resolved.frame, &order_by)?;
    Ok((
        func,
        BoundWindowSpec {
            partition_by,
            order_by,
            frame,
        },
        ty,
        nullable,
    ))
}

fn bind_window_arg(b: &mut ExprBinder<'_>, e: &ast::Expr) -> Result<BoundExpr> {
    match b.bind(e, None) {
        Err(SqlError::TypeMismatch { detail })
            if detail == "aggregate functions are not allowed in this context" =>
        {
            Err(err("aggregate function calls cannot be nested"))
        }
        other => other,
    }
}

/// Binds a constant integer argument (`NTILE(n)`, `LAG` offset, `NTH_VALUE`
/// position, a ROWS frame offset).
fn bind_constant_integer(
    b: &mut ExprBinder<'_>,
    e: &ast::Expr,
    what: &str,
) -> Result<Box<BoundExpr>> {
    // Bound without an expected type: an integer literal is INTEGER (or BIGINT
    // when it does not fit), a parameter is typed by its value; both are accepted.
    let bound = b.bind(e, None)?;
    if !is_constant(&bound) {
        return Err(err(format!(
            "{what} must be a constant (a literal or a parameter), not an expression over columns"
        )));
    }
    match bound.ty {
        None | Some(RelationalType::Integer) | Some(RelationalType::Bigint) => {}
        Some(t) => return Err(err(format!("{what} must be an integer, found {t:?}"))),
    }
    if literal_is_negative(&bound) {
        return Err(err(format!("{what} must not be negative")));
    }
    Ok(Box::new(bound))
}

type NamedBound = (BoundWindowFunc, Option<RelationalType>, bool, bool, bool);

/// `(function, type, nullable, may take a frame, requires ORDER BY)`.
fn bind_named_function(
    b: &mut ExprBinder<'_>,
    name: &str,
    args: &[ast::Expr],
    ignore_nulls: bool,
) -> Result<NamedBound> {
    if ignore_nulls
        && !matches!(
            name,
            "lag" | "lead" | "first_value" | "last_value" | "nth_value"
        )
    {
        return Err(err(format!(
            "IGNORE NULLS is not accepted by {}; it applies to LAG, LEAD, FIRST_VALUE, LAST_VALUE and NTH_VALUE",
            name.to_ascii_uppercase()
        )));
    }
    let arity = |lo: usize, hi: usize| -> Result<()> {
        if args.len() < lo || args.len() > hi {
            let want = if lo == hi {
                lo.to_string()
            } else {
                format!("{lo} to {hi}")
            };
            return Err(err(format!(
                "{} takes {want} argument(s), found {}",
                name.to_ascii_uppercase(),
                args.len()
            )));
        }
        Ok(())
    };
    Ok(match name {
        "row_number" => {
            arity(0, 0)?;
            (
                BoundWindowFunc::RowNumber,
                Some(RelationalType::Bigint),
                false,
                false,
                false,
            )
        }
        "rank" => {
            arity(0, 0)?;
            (
                BoundWindowFunc::Rank,
                Some(RelationalType::Bigint),
                false,
                false,
                true,
            )
        }
        "dense_rank" => {
            arity(0, 0)?;
            (
                BoundWindowFunc::DenseRank,
                Some(RelationalType::Bigint),
                false,
                false,
                true,
            )
        }
        "percent_rank" => {
            arity(0, 0)?;
            (
                BoundWindowFunc::PercentRank,
                Some(RelationalType::Double),
                false,
                false,
                true,
            )
        }
        "cume_dist" => {
            arity(0, 0)?;
            (
                BoundWindowFunc::CumeDist,
                Some(RelationalType::Double),
                false,
                false,
                true,
            )
        }
        "ntile" => {
            arity(1, 1)?;
            let n = bind_constant_integer(b, &args[0], "the bucket count of NTILE")?;
            (
                BoundWindowFunc::Ntile(n),
                Some(RelationalType::Bigint),
                false,
                false,
                false,
            )
        }
        "lag" | "lead" => {
            arity(1, 3)?;
            let expr = Box::new(bind_window_arg(b, &args[0])?);
            let ty = expr.ty;
            let offset = args
                .get(1)
                .map(|e| {
                    bind_constant_integer(
                        b,
                        e,
                        &format!("the offset of {}", name.to_ascii_uppercase()),
                    )
                })
                .transpose()?;
            let default = match args.get(2) {
                None => None,
                Some(e) => {
                    let d = b.bind(e, ty)?;
                    if let (Some(dt), Some(et)) = (d.ty, ty) {
                        if dt != et {
                            return Err(err(format!(
                                "the default of {} is {dt:?} but the value is {et:?}",
                                name.to_ascii_uppercase()
                            )));
                        }
                    }
                    Some(Box::new(d))
                }
            };
            let f = if name == "lag" {
                BoundWindowFunc::Lag {
                    expr,
                    offset,
                    default,
                    ignore_nulls,
                }
            } else {
                BoundWindowFunc::Lead {
                    expr,
                    offset,
                    default,
                    ignore_nulls,
                }
            };
            (f, ty, true, false, true)
        }
        "first_value" | "last_value" => {
            arity(1, 1)?;
            let expr = Box::new(bind_window_arg(b, &args[0])?);
            let ty = expr.ty;
            let f = if name == "first_value" {
                BoundWindowFunc::FirstValue { expr, ignore_nulls }
            } else {
                BoundWindowFunc::LastValue { expr, ignore_nulls }
            };
            (f, ty, true, true, false)
        }
        "nth_value" => {
            arity(2, 2)?;
            let expr = Box::new(bind_window_arg(b, &args[0])?);
            let ty = expr.ty;
            let n = bind_constant_integer(b, &args[1], "the position of NTH_VALUE")?;
            if let BoundExprKind::Literal(Some(v)) = &n.kind {
                let zero = matches!(v, RelationalValue::Integer(0) | RelationalValue::Bigint(0));
                if zero {
                    return Err(err("the position of NTH_VALUE must be at least 1"));
                }
            }
            (
                BoundWindowFunc::NthValue {
                    expr,
                    n,
                    ignore_nulls,
                },
                ty,
                true,
                true,
                false,
            )
        }
        other => {
            return Err(err(format!("{other} is not a window function")));
        }
    })
}

/// The frame a window uses (`explicit` or the standard default), checked
/// against the standard's rules (ISO/IEC 9075-2 7.15 syntax rules):
/// the start may not be `UNBOUNDED FOLLOWING`, the end may not be `UNBOUNDED
/// PRECEDING`, a start of `CURRENT ROW` may not end at `N PRECEDING`, and a
/// start of `N FOLLOWING` may only end at `N FOLLOWING` or `UNBOUNDED
/// FOLLOWING`. A frame that breaks one of them is empty for every row; it is
/// refused rather than silently producing nothing. (A frame that is merely
/// empty for *some* rows -- `1 FOLLOWING AND 1 FOLLOWING` on the last row -- is
/// legal and evaluates to the aggregate's empty-input value.)
fn bind_frame(
    b: &mut ExprBinder<'_>,
    frame: Option<&ast::FrameSpec>,
    order_by: &[BoundOrderByItem],
) -> Result<BoundFrame> {
    let Some(f) = frame else {
        return Ok(BoundFrame {
            units: FrameUnits::Range,
            start: BoundFrameBound::UnboundedPreceding,
            end: if order_by.is_empty() {
                BoundFrameBound::UnboundedFollowing
            } else {
                BoundFrameBound::CurrentRow
            },
            explicit: false,
        });
    };
    use ast::FrameBound as F;
    match (&f.start, &f.end) {
        (F::UnboundedFollowing, _) => {
            return Err(err("a window frame cannot start at UNBOUNDED FOLLOWING"));
        }
        (_, F::UnboundedPreceding) => {
            return Err(err("a window frame cannot end at UNBOUNDED PRECEDING"));
        }
        (F::CurrentRow, F::Preceding(_)) => {
            return Err(err(
                "a window frame that starts at CURRENT ROW cannot end at N PRECEDING",
            ));
        }
        (F::Following(_), F::CurrentRow) | (F::Following(_), F::Preceding(_)) => {
            return Err(err(
                "a window frame that starts at N FOLLOWING must end at N FOLLOWING or UNBOUNDED FOLLOWING",
            ));
        }
        _ => {}
    }
    let has_offset = |x: &F| matches!(x, F::Preceding(_) | F::Following(_));
    if f.units == FrameUnits::Groups && order_by.is_empty() {
        return Err(err(
            "a GROUPS frame requires the window to have an ORDER BY (peer groups are defined by it)",
        ));
    }
    let mut key_ty = None;
    if f.units == FrameUnits::Range && (has_offset(&f.start) || has_offset(&f.end)) {
        if order_by.len() != 1 {
            return Err(err(
                "a RANGE frame with an offset requires exactly one ORDER BY expression",
            ));
        }
        match order_by[0].expr.ty {
            Some(t) if is_numeric(t) => key_ty = Some(t),
            Some(t) => {
                return Err(err(format!(
                "a RANGE frame with an offset requires a numeric ORDER BY expression, found {t:?}"
            )))
            }
            None => {
                return Err(err(
                    "a RANGE frame with an offset requires a typed ORDER BY expression",
                ))
            }
        }
    }
    let mut bound = |x: &F| -> Result<BoundFrameBound> {
        let mut offset = |e: &ast::Expr| -> Result<Box<BoundExpr>> {
            match f.units {
                FrameUnits::Rows => bind_constant_integer(b, e, "a ROWS frame offset"),
                FrameUnits::Groups => bind_constant_integer(b, e, "a GROUPS frame offset"),
                FrameUnits::Range => {
                    let bound = b.bind(e, key_ty)?;
                    if !is_constant(&bound) {
                        return Err(err(
                            "a RANGE frame offset must be a constant (a literal or a parameter)",
                        ));
                    }
                    if literal_is_negative(&bound) {
                        return Err(err("a RANGE frame offset must not be negative"));
                    }
                    Ok(Box::new(bound))
                }
            }
        };
        Ok(match x {
            F::UnboundedPreceding => BoundFrameBound::UnboundedPreceding,
            F::UnboundedFollowing => BoundFrameBound::UnboundedFollowing,
            F::CurrentRow => BoundFrameBound::CurrentRow,
            F::Preceding(e) => BoundFrameBound::Preceding(offset(e)?),
            F::Following(e) => BoundFrameBound::Following(offset(e)?),
        })
    };
    let start = bound(&f.start)?;
    let end = bound(&f.end)?;
    Ok(BoundFrame {
        units: f.units,
        start,
        end,
        explicit: true,
    })
}
