//! Shared `BoundExpr` analysis helpers used by both the optimizer
//! (`crate::plan::optimize`) and the physical planner (`crate::plan::
//! access`) — one implementation of "decompose a predicate into
//! `AND`-conjuncts" and "which tables does this expression reference,"
//! never two independently-maintained copies with a chance to disagree.
//!
//! **Never simplifies expression logic itself** — only ever decides
//! *where* an already-unmodified `BoundExpr` subtree is evaluated. This
//! is what makes NULL/three-valued-logic preservation (item 21)
//! automatic rather than something each rule has to re-prove: no rule
//! in this crate rewrites `AND`/`OR`/`NOT`/`IS NULL` algebra, so the
//! only way to violate 3VL would be to move a conjunct somewhere it can
//! observe different inputs (item 20's LEFT JOIN case) — which is
//! exactly what `referenced_table_refs` + the null-extension check in
//! `crate::plan::optimize` exists to prevent.

use std::collections::BTreeSet;

use crate::ast::BinaryOp;
use crate::bound::{BoundExpr, BoundExprKind};

/// Splits a predicate into its top-level `AND`-connected conjuncts.
/// Recurses through nested `AND`s only — never descends into `OR`,
/// `NOT`, `CASE`, or any other construct, since splitting there would
/// change which rows the reassembled predicate matches (`(a AND b) OR c`
/// is not `(a OR c) AND (b OR c)`'s own conjunct set). A predicate with
/// no top-level `AND` is its own single-element conjunct list.
pub fn conjuncts(expr: &BoundExpr) -> Vec<BoundExpr> {
    let mut out = Vec::new();
    collect_conjuncts(expr, &mut out);
    out
}

fn collect_conjuncts(expr: &BoundExpr, out: &mut Vec<BoundExpr>) {
    if let BoundExprKind::BinaryOp {
        left,
        op: BinaryOp::And,
        right,
    } = &expr.kind
    {
        collect_conjuncts(left, out);
        collect_conjuncts(right, out);
    } else {
        out.push(expr.clone());
    }
}

/// Re-combines conjuncts with `AND`, in order — the exact inverse of
/// `conjuncts` (`recombine(conjuncts(e)) == e`'s own logical meaning,
/// verified by `plan_tests::conjuncts_round_trip_property`). `None` for
/// an empty list (no predicate at all, not a vacuous `TRUE`  — callers
/// treat `None` as "no filter needed," identical in meaning).
pub fn recombine(mut items: Vec<BoundExpr>) -> Option<BoundExpr> {
    let first = items.pop()?;
    Some(items.into_iter().rev().fold(first, |acc, next| BoundExpr {
        ty: acc.ty,
        nullable: acc.nullable || next.nullable,
        kind: BoundExprKind::BinaryOp {
            left: Box::new(next),
            op: BinaryOp::And,
            right: Box::new(acc),
        },
    }))
}

/// Every distinct `TableRefId` a `BoundExpr` subtree reads a column
/// from — a `Parameter`/`Literal`-only expression references none.
pub fn referenced_table_refs(expr: &BoundExpr) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    collect_table_refs(expr, &mut out);
    out
}

fn collect_table_refs(expr: &BoundExpr, out: &mut BTreeSet<u32>) {
    match &expr.kind {
        BoundExprKind::Literal(_) | BoundExprKind::Parameter { .. } => {}
        BoundExprKind::Column(c) => {
            out.insert(c.table_ref.0);
        }
        BoundExprKind::UnaryOp { expr, .. } => collect_table_refs(expr, out),
        BoundExprKind::BinaryOp { left, right, .. } => {
            collect_table_refs(left, out);
            collect_table_refs(right, out);
        }
        BoundExprKind::IsNull { expr, .. } => collect_table_refs(expr, out),
        BoundExprKind::Between {
            expr, low, high, ..
        } => {
            collect_table_refs(expr, out);
            collect_table_refs(low, out);
            collect_table_refs(high, out);
        }
        BoundExprKind::InList { expr, list, .. } => {
            collect_table_refs(expr, out);
            for item in list {
                collect_table_refs(item, out);
            }
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            collect_table_refs(expr, out);
            collect_table_refs(pattern, out);
        }
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_table_refs(operand, out);
            }
            for (when, then) in branches {
                collect_table_refs(when, out);
                collect_table_refs(then, out);
            }
            if let Some(else_result) = else_result {
                collect_table_refs(else_result, out);
            }
        }
        BoundExprKind::Function { args, .. } => {
            for arg in args {
                collect_table_refs(arg, out);
            }
        }
        BoundExprKind::Aggregate(agg) => {
            if let crate::aggregate::AggregateArg::Expr(e) = &agg.arg {
                collect_table_refs(e, out);
            }
            if let Some(f) = &agg.filter {
                collect_table_refs(f, out);
            }
        }
        BoundExprKind::AggregateRef(_)
        | BoundExprKind::WindowRef(_)
        | BoundExprKind::GroupKeyRef(_)
        | BoundExprKind::Grouping(_) => {}
        BoundExprKind::Window(call) => {
            for e in call.exprs() {
                collect_table_refs(e, out);
            }
        }
        // A subquery reads the *enclosing* query's tables only through its
        // outer references (and its `IN` operand); its own tables are its
        // own. This is what keeps predicate pushdown from moving a
        // correlated conjunct somewhere its outer columns are not bound.
        BoundExprKind::Subquery(sq) => {
            if let Some(operand) = &sq.operand {
                collect_table_refs(operand, out);
            }
            for c in &sq.outer_refs {
                out.insert(c.table_ref.0);
            }
        }
    }
}

/// Every distinct column ordinal (for a single already-known
/// `TableRefId`) a `BoundExpr` subtree reads — used by projection
/// pruning (item 13) to compute a scan's required column set. Ordinals
/// belonging to a *different* table are ignored (a caller filters by
/// `referenced_table_refs` first, or calls this once per relevant
/// table).
pub fn referenced_ordinals(expr: &BoundExpr, table_ref: u32) -> BTreeSet<u16> {
    let mut out = BTreeSet::new();
    collect_ordinals(expr, table_ref, &mut out);
    out
}

fn collect_ordinals(expr: &BoundExpr, table_ref: u32, out: &mut BTreeSet<u16>) {
    match &expr.kind {
        BoundExprKind::Literal(_) | BoundExprKind::Parameter { .. } => {}
        BoundExprKind::Column(c) => {
            if c.table_ref.0 == table_ref {
                out.insert(c.ordinal);
            }
        }
        BoundExprKind::UnaryOp { expr, .. } => collect_ordinals(expr, table_ref, out),
        BoundExprKind::BinaryOp { left, right, .. } => {
            collect_ordinals(left, table_ref, out);
            collect_ordinals(right, table_ref, out);
        }
        BoundExprKind::IsNull { expr, .. } => collect_ordinals(expr, table_ref, out),
        BoundExprKind::Between {
            expr, low, high, ..
        } => {
            collect_ordinals(expr, table_ref, out);
            collect_ordinals(low, table_ref, out);
            collect_ordinals(high, table_ref, out);
        }
        BoundExprKind::InList { expr, list, .. } => {
            collect_ordinals(expr, table_ref, out);
            for item in list {
                collect_ordinals(item, table_ref, out);
            }
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            collect_ordinals(expr, table_ref, out);
            collect_ordinals(pattern, table_ref, out);
        }
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_ordinals(operand, table_ref, out);
            }
            for (when, then) in branches {
                collect_ordinals(when, table_ref, out);
                collect_ordinals(then, table_ref, out);
            }
            if let Some(else_result) = else_result {
                collect_ordinals(else_result, table_ref, out);
            }
        }
        BoundExprKind::Function { args, .. } => {
            for arg in args {
                collect_ordinals(arg, table_ref, out);
            }
        }
        BoundExprKind::Aggregate(agg) => {
            if let crate::aggregate::AggregateArg::Expr(e) = &agg.arg {
                collect_ordinals(e, table_ref, out);
            }
            if let Some(f) = &agg.filter {
                collect_ordinals(f, table_ref, out);
            }
        }
        BoundExprKind::AggregateRef(_)
        | BoundExprKind::WindowRef(_)
        | BoundExprKind::GroupKeyRef(_)
        | BoundExprKind::Grouping(_) => {}
        BoundExprKind::Window(call) => {
            for e in call.exprs() {
                collect_ordinals(e, table_ref, out);
            }
        }
        BoundExprKind::Subquery(sq) => {
            if let Some(operand) = &sq.operand {
                collect_ordinals(operand, table_ref, out);
            }
            for c in &sq.outer_refs {
                if c.table_ref.0 == table_ref {
                    out.insert(c.ordinal);
                }
            }
        }
    }
}

/// Visits every subquery expression directly contained in `expr` (not
/// those nested inside another subquery's own query -- the visitor decides
/// how to treat those), including through the `IN` operand of a subquery.
/// The visitor gets mutable access so the planner can attach each
/// subquery's physical plan in place.
pub fn for_each_subquery_mut<F>(expr: &mut BoundExpr, f: &mut F) -> crate::error::Result<()>
where
    F: FnMut(&mut crate::bound::BoundSubqueryExpr) -> crate::error::Result<()>,
{
    match &mut expr.kind {
        BoundExprKind::Literal(_)
        | BoundExprKind::Parameter { .. }
        | BoundExprKind::Column(_)
        | BoundExprKind::AggregateRef(_)
        | BoundExprKind::WindowRef(_)
        | BoundExprKind::GroupKeyRef(_)
        | BoundExprKind::Grouping(_)
        // The expressions inside a window call are bound without subquery
        // support, so there is none to visit.
        | BoundExprKind::Window(_) => Ok(()),
        BoundExprKind::UnaryOp { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            for_each_subquery_mut(expr, f)
        }
        BoundExprKind::BinaryOp { left, right, .. } => {
            for_each_subquery_mut(left, f)?;
            for_each_subquery_mut(right, f)
        }
        BoundExprKind::Between {
            expr, low, high, ..
        } => {
            for_each_subquery_mut(expr, f)?;
            for_each_subquery_mut(low, f)?;
            for_each_subquery_mut(high, f)
        }
        BoundExprKind::InList { expr, list, .. } => {
            for_each_subquery_mut(expr, f)?;
            for item in list {
                for_each_subquery_mut(item, f)?;
            }
            Ok(())
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            for_each_subquery_mut(expr, f)?;
            for_each_subquery_mut(pattern, f)
        }
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => {
            if let Some(operand) = operand {
                for_each_subquery_mut(operand, f)?;
            }
            for (when, then) in branches {
                for_each_subquery_mut(when, f)?;
                for_each_subquery_mut(then, f)?;
            }
            if let Some(else_result) = else_result {
                for_each_subquery_mut(else_result, f)?;
            }
            Ok(())
        }
        BoundExprKind::Function { args, .. } => {
            for arg in args {
                for_each_subquery_mut(arg, f)?;
            }
            Ok(())
        }
        BoundExprKind::Aggregate(agg) => {
            if let crate::aggregate::AggregateArg::Expr(e) = &mut agg.arg {
                for_each_subquery_mut(e, f)?;
            }
            if let Some(filter) = &mut agg.filter {
                for_each_subquery_mut(filter, f)?;
            }
            Ok(())
        }
        BoundExprKind::Subquery(sq) => {
            if let Some(operand) = &mut sq.operand {
                for_each_subquery_mut(operand, f)?;
            }
            f(sq)
        }
    }
}

/// `true` iff `expr` contains a subquery expression anywhere in its own tree
/// (a subquery nested inside another subquery's query does not count -- that
/// one is reached through the outer subquery node itself).
pub fn contains_subquery(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::Subquery(_) => true,
        BoundExprKind::Literal(_)
        | BoundExprKind::Parameter { .. }
        | BoundExprKind::Column(_)
        | BoundExprKind::AggregateRef(_)
        | BoundExprKind::WindowRef(_)
        | BoundExprKind::GroupKeyRef(_)
        | BoundExprKind::Grouping(_)
        | BoundExprKind::Window(_) => false,
        BoundExprKind::UnaryOp { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            contains_subquery(expr)
        }
        BoundExprKind::BinaryOp { left, right, .. } => {
            contains_subquery(left) || contains_subquery(right)
        }
        BoundExprKind::Between {
            expr, low, high, ..
        } => contains_subquery(expr) || contains_subquery(low) || contains_subquery(high),
        BoundExprKind::InList { expr, list, .. } => {
            contains_subquery(expr) || list.iter().any(contains_subquery)
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            contains_subquery(expr) || contains_subquery(pattern)
        }
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => {
            operand.as_deref().is_some_and(contains_subquery)
                || branches
                    .iter()
                    .any(|(w, t)| contains_subquery(w) || contains_subquery(t))
                || else_result.as_deref().is_some_and(contains_subquery)
        }
        BoundExprKind::Function { args, .. } => args.iter().any(contains_subquery),
        BoundExprKind::Aggregate(agg) => {
            (match &agg.arg {
                crate::aggregate::AggregateArg::Expr(e) => contains_subquery(e),
                crate::aggregate::AggregateArg::Wildcard => false,
            }) || agg.filter.as_deref().is_some_and(contains_subquery)
        }
    }
}

/// If `expr` is `column = other` or `other = column` (a top-level
/// equality with exactly one side a plain column reference and the
/// other side free of that same column), returns
/// `(ordinal, other_side)`. Used by PK-lookup/index-selection (items 6/
/// 8) to find equality conjuncts usable as a lookup key. Deliberately
/// narrow: does not fire for `a = a` (self-equality, not a lookup key),
/// nor for `f(column) = x` (only a *plain* column reference counts —
/// firing on a wrapped expression would require evaluating `f` inside
/// the planner, which item 22 forbids).
pub fn as_column_equality(expr: &BoundExpr, table_ref: u32) -> Option<(u16, &BoundExpr)> {
    let BoundExprKind::BinaryOp {
        left,
        op: BinaryOp::Eq,
        right,
    } = &expr.kind
    else {
        return None;
    };
    match (&left.kind, &right.kind) {
        (BoundExprKind::Column(c), _)
            if c.table_ref.0 == table_ref && !references_table(right, table_ref) =>
        {
            Some((c.ordinal, right.as_ref()))
        }
        (_, BoundExprKind::Column(c))
            if c.table_ref.0 == table_ref && !references_table(left, table_ref) =>
        {
            Some((c.ordinal, left.as_ref()))
        }
        _ => None,
    }
}

fn references_table(expr: &BoundExpr, table_ref: u32) -> bool {
    referenced_table_refs(expr).contains(&table_ref)
}

/// If `expr` is a top-level `column <op> other` or `other <op> column`
/// comparison (`<`, `<=`, `>`, `>=`) with exactly one side a plain
/// column reference of `table_ref` and the other side free of it,
/// returns `(ordinal, normalized_op, other_side)` — `normalized_op` is
/// always expressed as `column <op> other` regardless of which side the
/// column appeared on (flipping `X > column` to `column < X`), so a
/// caller never has to handle both orientations. Same narrowness as
/// `as_column_equality` and for the same reason (item 22).
pub fn as_column_comparison(
    expr: &BoundExpr,
    table_ref: u32,
) -> Option<(u16, BinaryOp, &BoundExpr)> {
    let BoundExprKind::BinaryOp { left, op, right } = &expr.kind else {
        return None;
    };
    let op = *op;
    if !matches!(
        op,
        BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
    ) {
        return None;
    }
    match (&left.kind, &right.kind) {
        (BoundExprKind::Column(c), _)
            if c.table_ref.0 == table_ref && !references_table(right, table_ref) =>
        {
            Some((c.ordinal, op, right.as_ref()))
        }
        (_, BoundExprKind::Column(c))
            if c.table_ref.0 == table_ref && !references_table(left, table_ref) =>
        {
            Some((c.ordinal, flip(op), left.as_ref()))
        }
        _ => None,
    }
}

/// `a AND b`, built directly (no simplification — see this module's own
/// doc comment on why nothing here rewrites expression logic).
pub fn and_all(a: BoundExpr, b: BoundExpr) -> BoundExpr {
    BoundExpr {
        ty: a.ty,
        nullable: a.nullable || b.nullable,
        kind: BoundExprKind::BinaryOp {
            left: Box::new(a),
            op: BinaryOp::And,
            right: Box::new(b),
        },
    }
}

fn flip(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other,
    }
}
