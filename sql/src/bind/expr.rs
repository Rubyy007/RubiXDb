//! Expression binding — item 21/22: type resolution, no arbitrary
//! implicit coercion (an expression's operand types must match exactly,
//! `NULL` always assignable), every bound node carries its own resolved
//! type/nullability so a future planner never re-infers them (item 22).

use rubixdb::relational::{RelationalType, RelationalValue};

use crate::ast::{self, BinaryOp, Expr, Literal, UnaryOp};
use crate::bind::scope::Scope;
use crate::bound::{BoundExpr, BoundExprKind, ColumnRef};
use crate::error::{Result, SqlError};
use crate::functions::{self, ReturnType};
use crate::limits::SqlLimits;
use crate::temporal;

fn type_mismatch(detail: impl Into<String>) -> SqlError {
    SqlError::TypeMismatch {
        detail: detail.into(),
    }
}

/// D21: exact-match only, `NULL`/untyped is always assignable — the
/// "safest consistent behavior" this increment's own ADR amendment
/// documents in place of an unwritten general coercion table.
fn check_assignable(actual: Option<RelationalType>, expected: RelationalType) -> Result<()> {
    match actual {
        None => Ok(()),
        Some(t) if t == expected => Ok(()),
        Some(t) => Err(type_mismatch(format!("expected {expected:?}, found {t:?}"))),
    }
}

pub struct ExprBinder<'a> {
    pub scope: Option<&'a Scope>,
    pub limits: &'a SqlLimits,
    pub max_parameter: u32,
}

impl<'a> ExprBinder<'a> {
    pub fn new(scope: Option<&'a Scope>, limits: &'a SqlLimits) -> Self {
        ExprBinder {
            scope,
            limits,
            max_parameter: 0,
        }
    }

    pub fn bind(&mut self, expr: &Expr, expected: Option<RelationalType>) -> Result<BoundExpr> {
        match expr {
            Expr::Literal(lit) => self.bind_literal(lit, expected),
            Expr::Parameter(idx) => self.bind_parameter(*idx, expected),
            Expr::Column(col_ref) => self.bind_column(col_ref, expected),
            Expr::UnaryOp { op, expr } => self.bind_unary(*op, expr, expected),
            Expr::BinaryOp { left, op, right } => self.bind_binary(left, *op, right),
            Expr::IsNull { expr, negated } => {
                let inner = self.bind(expr, None)?;
                Ok(BoundExpr {
                    kind: BoundExprKind::IsNull {
                        expr: Box::new(inner),
                        negated: *negated,
                    },
                    ty: Some(RelationalType::Boolean),
                    nullable: false,
                })
            }
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let bound = self.bind_shared(&[expr, low, high], None)?;
                let [e, l, h]: [BoundExpr; 3] =
                    bound.try_into().expect("bind_shared preserves length");
                Ok(BoundExpr {
                    kind: BoundExprKind::Between {
                        expr: Box::new(e),
                        negated: *negated,
                        low: Box::new(l),
                        high: Box::new(h),
                    },
                    ty: Some(RelationalType::Boolean),
                    nullable: false,
                })
            }
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                if list.len() > self.limits.max_list_elements {
                    return Err(SqlError::ResourceLimit {
                        detail: format!(
                            "IN list exceeds max_list_elements ({})",
                            self.limits.max_list_elements
                        ),
                    });
                }
                let mut all: Vec<&Expr> = vec![expr];
                all.extend(list.iter());
                let mut bound = self.bind_shared(&all, None)?;
                let e = bound.remove(0);
                Ok(BoundExpr {
                    kind: BoundExprKind::InList {
                        expr: Box::new(e),
                        list: bound,
                        negated: *negated,
                    },
                    ty: Some(RelationalType::Boolean),
                    nullable: false,
                })
            }
            Expr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                let e = self.bind(expr, Some(RelationalType::Text))?;
                let p = self.bind(pattern, Some(RelationalType::Text))?;
                Ok(BoundExpr {
                    kind: BoundExprKind::Like {
                        expr: Box::new(e),
                        pattern: Box::new(p),
                        negated: *negated,
                        case_insensitive: *case_insensitive,
                    },
                    ty: Some(RelationalType::Boolean),
                    nullable: false,
                })
            }
            Expr::Case {
                operand,
                branches,
                else_result,
            } => self.bind_case(operand.as_deref(), branches, else_result.as_deref()),
            Expr::Function { name, args } => self.bind_function(name, args),
        }
    }

    fn bind_parameter(&mut self, idx: u32, expected: Option<RelationalType>) -> Result<BoundExpr> {
        if idx as usize > self.limits.max_parameters {
            return Err(SqlError::InvalidParameter {
                detail: format!(
                    "parameter index exceeds max_parameters ({})",
                    self.limits.max_parameters
                ),
            });
        }
        self.max_parameter = self.max_parameter.max(idx);
        Ok(BoundExpr {
            kind: BoundExprKind::Parameter { index: idx },
            ty: expected,
            nullable: true,
        })
    }

    fn bind_column(
        &mut self,
        col_ref: &ast::ColumnRef,
        expected: Option<RelationalType>,
    ) -> Result<BoundExpr> {
        let scope = self.scope.ok_or_else(|| SqlError::UnknownObject {
            kind: "column",
            detail: col_ref
                .parts
                .iter()
                .map(|p| p.value.as_str())
                .collect::<Vec<_>>()
                .join("."),
        })?;
        let (entry, column) = scope.resolve_column(col_ref)?;
        let ty = crate::bind::ddl::relational_type_of(column)?;
        check_assignable(Some(ty), expected.unwrap_or(ty))?;
        let nullable = column.nullable || entry.null_extended;
        Ok(BoundExpr {
            kind: BoundExprKind::Column(ColumnRef {
                table_ref: crate::bound::TableRefId(entry.table_ref_id),
                table_id: entry.resolved.table_id,
                ordinal: column.ordinal,
            }),
            ty: Some(ty),
            nullable,
        })
    }

    fn bind_unary(
        &mut self,
        op: UnaryOp,
        expr: &Expr,
        expected: Option<RelationalType>,
    ) -> Result<BoundExpr> {
        match op {
            UnaryOp::Not => {
                let inner = self.bind(expr, Some(RelationalType::Boolean))?;
                Ok(BoundExpr {
                    nullable: inner.nullable,
                    kind: BoundExprKind::UnaryOp {
                        op,
                        expr: Box::new(inner),
                    },
                    ty: Some(RelationalType::Boolean),
                })
            }
            UnaryOp::Neg => {
                let inner = self.bind(expr, expected)?;
                if let Some(ty) = inner.ty {
                    if !is_numeric(ty) {
                        return Err(type_mismatch(format!(
                            "unary - requires a numeric operand, found {ty:?}"
                        )));
                    }
                }
                let ty = inner.ty;
                let nullable = inner.nullable;
                Ok(BoundExpr {
                    kind: BoundExprKind::UnaryOp {
                        op,
                        expr: Box::new(inner),
                    },
                    ty,
                    nullable,
                })
            }
        }
    }

    fn bind_binary(&mut self, left: &Expr, op: BinaryOp, right: &Expr) -> Result<BoundExpr> {
        let bound = self.bind_shared(&[left, right], None)?;
        let [l, r]: [BoundExpr; 2] = bound.try_into().expect("bind_shared preserves length");
        let is_comparison = matches!(
            op,
            BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
        );
        let is_logical = matches!(op, BinaryOp::And | BinaryOp::Or);
        if is_logical {
            check_assignable(l.ty, RelationalType::Boolean)?;
            check_assignable(r.ty, RelationalType::Boolean)?;
        } else if let Some(ty) = l.ty.or(r.ty) {
            if !is_comparison && !is_numeric(ty) {
                return Err(type_mismatch(format!(
                    "arithmetic operator requires a numeric operand, found {ty:?}"
                )));
            }
        }
        let ty = if is_comparison || is_logical {
            Some(RelationalType::Boolean)
        } else {
            l.ty.or(r.ty)
        };
        let nullable = l.nullable || r.nullable;
        Ok(BoundExpr {
            kind: BoundExprKind::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            },
            ty,
            nullable,
        })
    }

    /// Binds every expression in `exprs` under one shared, mutually
    /// consistent type. A first pass binds each with `hint`. A bare
    /// literal's type is *flexible* (`bind_numeric_literal`'s own
    /// context-free default is only a fallback guess, never
    /// authoritative); a column/parameter/function result's type is
    /// *rigid* (fixed by the catalog or the function registry,
    /// independent of any hint). The shared type is the first **rigid**
    /// type found (regardless of which side it's on — `5 = orders.id`
    /// and `orders.id = 5` must behave identically), falling back to the
    /// first literal's own default only when every expression is a bare
    /// literal. Every expression is then re-bound against that shared
    /// type — a flexible literal conforms to it; a rigid expression is
    /// re-validated against it, and `bind()`'s own `check_assignable`
    /// raises `TypeMismatch` if it genuinely disagrees (D21 — no
    /// cross-type coercion). Used everywhere this crate needs "these N
    /// expressions must agree on one type" (`BETWEEN`, `IN (...)`,
    /// binary comparisons, `CASE` results) — one implementation, not N
    /// ad hoc ones.
    fn bind_shared(
        &mut self,
        exprs: &[&Expr],
        hint: Option<RelationalType>,
    ) -> Result<Vec<BoundExpr>> {
        let first_pass: Vec<BoundExpr> = exprs
            .iter()
            .map(|e| self.bind(e, hint))
            .collect::<Result<_>>()?;
        let rigid_ty = first_pass.iter().find_map(|b| {
            if matches!(b.kind, BoundExprKind::Literal(_)) {
                None
            } else {
                b.ty
            }
        });
        let ty = rigid_ty.or_else(|| first_pass.iter().find_map(|b| b.ty));
        match ty {
            Some(ty) => {
                // Only a flexible *literal* actually needs a second bind
                // to conform to the now-known shared type -- a rigid
                // expression's own type can never change on a second
                // pass, so re-binding it again would be pure waste.
                // Critically, "waste" compounds: `bind_shared` is called
                // from `bind_binary`, which is on the path `bind` itself
                // recurses through, so unconditionally re-binding *every*
                // operand here made a deeply left-nested chain of binary
                // operators (`a = 1 OR a = 2 OR ... OR a = N`) cost
                // `O(2^N)` instead of `O(N)` (each level doubled the work
                // of the level below it) -- a real, exploitable CPU-
                // exhaustion vector for any adversarial predicate with a
                // few dozen terms, found by `plan_tests::deeply_nested_
                // or_predicate_within_sql_limits_does_not_overflow_the_
                // planner`. A rigid expression still has its type
                // re-verified against `ty` (preserving D21's "no
                // implicit coercion, exact match only" semantics for two
                // *different* rigid types, e.g. `orders.id = t.id` across
                // a `BIGINT`/`INTEGER` mismatch) without paying to
                // re-walk it.
                let mut out = Vec::with_capacity(first_pass.len());
                for (e, bound) in exprs.iter().zip(first_pass) {
                    if matches!(bound.kind, BoundExprKind::Literal(_)) {
                        out.push(self.bind(e, Some(ty))?);
                    } else {
                        check_assignable(bound.ty, ty)?;
                        out.push(bound);
                    }
                }
                Ok(out)
            }
            None => Ok(first_pass),
        }
    }

    fn bind_case(
        &mut self,
        operand: Option<&Expr>,
        branches: &[(Expr, Expr)],
        else_result: Option<&Expr>,
    ) -> Result<BoundExpr> {
        let bound_operand = operand.map(|e| self.bind(e, None)).transpose()?;
        let operand_ty = bound_operand.as_ref().and_then(|o| o.ty);

        let mut bound_branches = Vec::with_capacity(branches.len());
        for (cond, result) in branches {
            let bound_cond = match &bound_operand {
                Some(_) => self.bind(cond, operand_ty)?,
                None => self.bind(cond, Some(RelationalType::Boolean))?,
            };
            bound_branches.push((bound_cond, result));
        }

        let mut result_exprs: Vec<&Expr> = bound_branches.iter().map(|(_, r)| *r).collect();
        if let Some(e) = else_result {
            result_exprs.push(e);
        }
        let bound_results = self.bind_shared(&result_exprs, None)?;
        let ty = bound_results.iter().find_map(|b| b.ty);
        let nullable = bound_results.iter().any(|b| b.nullable) || else_result.is_none();

        let has_else = else_result.is_some();
        let mut results_iter = bound_results.into_iter();
        let branches: Vec<(BoundExpr, BoundExpr)> = bound_branches
            .into_iter()
            .map(|(cond, _)| (cond, results_iter.next().expect("one result per branch")))
            .collect();
        let else_bound = if has_else {
            Some(Box::new(results_iter.next().expect("else result present")))
        } else {
            None
        };

        Ok(BoundExpr {
            kind: BoundExprKind::Case {
                operand: bound_operand.map(Box::new),
                branches,
                else_result: else_bound,
            },
            ty,
            nullable,
        })
    }

    fn bind_function(&mut self, name: &ast::ObjectName, args: &[Expr]) -> Result<BoundExpr> {
        if name.0.len() != 1 {
            return Err(SqlError::Unsupported {
                detail: "qualified function names are not supported".to_string(),
            });
        }
        let def = functions::lookup(&name.0[0].value).ok_or_else(|| SqlError::UnknownObject {
            kind: "function",
            detail: name.0[0].value.clone(),
        })?;
        if args.len() != def.arg_types.len() {
            return Err(SqlError::InvalidParameter {
                detail: format!(
                    "{} expects {} argument(s), got {}",
                    def.name,
                    def.arg_types.len(),
                    args.len()
                ),
            });
        }
        let bound_args = args
            .iter()
            .zip(def.arg_types.iter())
            .map(|(arg, expected)| {
                let bound = self.bind(arg, None)?;
                if let Some(ty) = bound.ty {
                    if !expected.accepts(ty) {
                        return Err(type_mismatch(format!(
                            "{}: argument type {ty:?} is not accepted here",
                            def.name
                        )));
                    }
                }
                Ok(bound)
            })
            .collect::<Result<Vec<_>>>()?;
        let ty = match def.return_type {
            ReturnType::Fixed(t) => Some(t),
            ReturnType::SameAsArg(i) => bound_args[i].ty,
        };
        let nullable = def.null_propagates && bound_args.iter().any(|a| a.nullable);
        Ok(BoundExpr {
            kind: BoundExprKind::Function {
                name: def.name,
                args: bound_args,
            },
            ty,
            nullable,
        })
    }

    fn bind_literal(
        &mut self,
        lit: &Literal,
        expected: Option<RelationalType>,
    ) -> Result<BoundExpr> {
        match lit {
            Literal::Null => Ok(BoundExpr {
                kind: BoundExprKind::Literal(None),
                ty: expected,
                nullable: true,
            }),
            Literal::Boolean(b) => {
                check_assignable(
                    Some(RelationalType::Boolean),
                    expected.unwrap_or(RelationalType::Boolean),
                )?;
                Ok(typed_literal(
                    RelationalValue::Boolean(*b),
                    RelationalType::Boolean,
                ))
            }
            Literal::Text(s) => {
                let ty = expected.unwrap_or(RelationalType::Text);
                check_assignable(Some(RelationalType::Text), ty)?;
                Ok(typed_literal(
                    RelationalValue::Text(s.clone()),
                    RelationalType::Text,
                ))
            }
            Literal::Blob(b) => {
                let ty = expected.unwrap_or(RelationalType::Blob);
                check_assignable(Some(RelationalType::Blob), ty)?;
                Ok(typed_literal(
                    RelationalValue::Blob(b.clone()),
                    RelationalType::Blob,
                ))
            }
            Literal::Number { text, is_integer } => {
                bind_numeric_literal(text, *is_integer, expected)
            }
            Literal::Typed { data_type, text } => bind_typed_literal(*data_type, text, expected),
        }
    }
}

fn typed_literal(value: RelationalValue, ty: RelationalType) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Literal(Some(value)),
        ty: Some(ty),
        nullable: false,
    }
}

pub fn is_numeric(ty: RelationalType) -> bool {
    matches!(
        ty,
        RelationalType::Integer
            | RelationalType::Bigint
            | RelationalType::Real
            | RelationalType::Double
            | RelationalType::Decimal { .. }
    )
}

/// item 21: no context => the safest default this crate documents (not
/// silently invented per call site) — an integer-looking literal prefers
/// `INTEGER`, falling back to `BIGINT` only if it doesn't fit; a
/// decimal-looking literal (has a `.`/exponent) defaults to `DOUBLE`
/// (a raw literal has no column to inherit a `DECIMAL` scale from).
fn bind_numeric_literal(
    text: &str,
    is_integer: bool,
    expected: Option<RelationalType>,
) -> Result<BoundExpr> {
    let target = match expected {
        Some(t) => t,
        None if is_integer => {
            if text.parse::<i32>().is_ok() {
                RelationalType::Integer
            } else {
                RelationalType::Bigint
            }
        }
        None => RelationalType::Double,
    };
    match target {
        RelationalType::Integer => {
            let v: i32 = text.parse().map_err(|_| {
                type_mismatch(format!("numeric literal {text:?} does not fit in INTEGER"))
            })?;
            Ok(typed_literal(RelationalValue::Integer(v), target))
        }
        RelationalType::Bigint => {
            let v: i64 = text.parse().map_err(|_| {
                type_mismatch(format!("numeric literal {text:?} does not fit in BIGINT"))
            })?;
            Ok(typed_literal(RelationalValue::Bigint(v), target))
        }
        RelationalType::Real => {
            let v: f32 = text.parse().map_err(|_| {
                type_mismatch(format!("numeric literal {text:?} is not a valid REAL"))
            })?;
            Ok(typed_literal(RelationalValue::Real(v), target))
        }
        RelationalType::Double => {
            let v: f64 = text.parse().map_err(|_| {
                type_mismatch(format!("numeric literal {text:?} is not a valid DOUBLE"))
            })?;
            Ok(typed_literal(RelationalValue::Double(v), target))
        }
        RelationalType::Decimal { scale } => {
            let v = parse_decimal_text(text, scale)?;
            // Sanity bound only (max representable precision) -- the
            // exact column-declared precision is enforced separately
            // where that context exists (`bind::dml`/`bind::ddl`, which
            // have the real `type_params`).
            rubixdb::relational::value::validate_decimal(v, 38, scale)
                .map_err(|e| type_mismatch(e.to_string()))?;
            Ok(typed_literal(RelationalValue::Decimal(v, scale), target))
        }
        other => Err(type_mismatch(format!(
            "a numeric literal cannot bind to {other:?}"
        ))),
    }
}

pub fn parse_decimal_text(text: &str, scale: u8) -> Result<i128> {
    let negative = text.starts_with('-');
    let unsigned = text.trim_start_matches('-');
    let (int_part, frac_part) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if frac_part.len() > scale as usize {
        return Err(type_mismatch(format!(
            "literal {text:?} has more fractional digits than the target scale ({scale})"
        )));
    }
    let padded_frac = format!("{frac_part:0<width$}", width = scale as usize);
    let digits = format!("{int_part}{padded_frac}");
    let digits = if digits.is_empty() { "0" } else { &digits };
    let magnitude: i128 = digits
        .parse()
        .map_err(|_| type_mismatch(format!("literal {text:?} is not a valid DECIMAL")))?;
    Ok(if negative { -magnitude } else { magnitude })
}

fn bind_typed_literal(
    data_type: ast::SqlDataType,
    text: &str,
    expected: Option<RelationalType>,
) -> Result<BoundExpr> {
    let ty = crate::bind::ddl::sql_data_type_to_relational(data_type)?;
    check_assignable(Some(ty), expected.unwrap_or(ty))?;
    match data_type {
        ast::SqlDataType::Date => {
            let days = temporal::parse_date(text)?;
            Ok(typed_literal(RelationalValue::Date(days), ty))
        }
        ast::SqlDataType::Time => {
            let micros = temporal::parse_time(text)?;
            Ok(typed_literal(RelationalValue::Time(micros), ty))
        }
        ast::SqlDataType::Timestamp => {
            let micros = temporal::parse_timestamp(text)?;
            Ok(typed_literal(RelationalValue::Timestamp(micros), ty))
        }
        other => Err(type_mismatch(format!("typed string literal for {other:?}"))),
    }
}
