//! Deterministic plan formatter (items 30/31). The structured `Plan`/
//! `PhysicalPlan` enums (`crate::plan`) are the primary representation
//! this crate produces; `explain` is a secondary, derived, deterministic
//! text rendering for tests and a future `EXPLAIN`/CLI/API consumer —
//! never the only representation (item 30), and never containing a
//! parameter *value* (none exist at plan time — only the structural
//! `$n` marker), credential, raw filesystem path, or anything beyond
//! what the already-bound plan itself carries (item 30's own list).

use std::fmt::Write as _;

use crate::ast::{BinaryOp, JoinKind, UnaryOp};
use crate::bound::{
    BoundExpr, BoundExprKind, BoundFrame, BoundFrameBound, BoundGroupElement, BoundGrouping,
    BoundOrderByItem, BoundSubqueryExpr, BoundWindowFunc, NullsOrder, SubqueryKind,
};
use crate::plan::access::{IndexAccessMode, PhysicalAccess};
use crate::plan::logical::SetOpNode;
use crate::plan::physical::{JoinAlgorithm, PhysicalPlan};
use crate::plan::Plan;

pub fn explain(plan: &Plan) -> String {
    let mut out = String::new();
    write_plan(plan, 0, &mut out);
    out
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_plan(plan: &Plan, depth: usize, out: &mut String) {
    indent(out, depth);
    match plan {
        Plan::Query {
            physical,
            max_parameter,
            ..
        } => {
            let _ = writeln!(out, "Query (max_parameter={max_parameter})");
            write_physical(physical, depth + 1, out);
        }
        Plan::Insert(insert) => {
            let source = match &insert.select {
                Some(_) => "source=select".to_string(),
                None => format!("rows={}", insert.rows.len()),
            };
            let _ = writeln!(
                out,
                "Insert table_id={} {source}{}{}",
                insert.table_id,
                fmt_on_conflict(insert.on_conflict.as_ref()),
                fmt_returning(insert.returning.as_ref())
            );
            if let Some(plan) = insert.select.as_ref().and_then(|s| s.plan.as_ref()) {
                write_physical(plan, depth + 1, out);
            }
        }
        Plan::Update {
            table,
            assignments,
            access,
            returning,
            ..
        } => {
            let _ = writeln!(
                out,
                "Update table_id={} assignments={}{}",
                table.table_id,
                assignments.len(),
                fmt_returning(returning.as_ref())
            );
            indent(out, depth + 1);
            write_access(access, out);
            write_subplans(access_exprs(access), depth + 2, out);
            write_subplans(assignments.iter().map(|a| &a.value), depth + 1, out);
        }
        Plan::Delete {
            table,
            access,
            returning,
            ..
        } => {
            let _ = writeln!(
                out,
                "Delete table_id={}{}",
                table.table_id,
                fmt_returning(returning.as_ref())
            );
            indent(out, depth + 1);
            write_access(access, out);
            write_subplans(access_exprs(access), depth + 2, out);
        }
        Plan::Ddl(_) => {
            let _ = writeln!(out, "Ddl");
        }
        Plan::Begin => {
            let _ = writeln!(out, "Begin");
        }
        Plan::Commit => {
            let _ = writeln!(out, "Commit");
        }
        Plan::Rollback => {
            let _ = writeln!(out, "Rollback");
        }
        Plan::Explain(inner) => {
            let _ = writeln!(out, "Explain");
            write_plan(inner, depth + 1, out);
        }
    }
}

/// ` returning=[a, b]` -- the output names of a `RETURNING` list.
fn fmt_returning(r: Option<&crate::bound::BoundReturning>) -> String {
    match r {
        None => String::new(),
        Some(r) => format!(
            " returning=[{}]",
            r.items
                .iter()
                .map(|i| i.output_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// ` on_conflict=do_nothing(arbiters=2)` / ` on_conflict=do_update(arbiter=ix3)`.
fn fmt_on_conflict(oc: Option<&crate::bound::BoundOnConflict>) -> String {
    use crate::bound::BoundConflictAction;
    let Some(oc) = oc else {
        return String::new();
    };
    let arbiters: Vec<String> = oc
        .arbiters
        .iter()
        .map(|a| match a.index_id {
            None => "pk".to_string(),
            Some(id) => format!("index#{id}"),
        })
        .collect();
    match &oc.action {
        BoundConflictAction::DoNothing => {
            format!(" on_conflict=do_nothing(arbiters=[{}])", arbiters.join(", "))
        }
        BoundConflictAction::DoUpdate {
            assignments,
            selection,
        } => format!(
            " on_conflict=do_update(arbiter=[{}], set={}{})",
            arbiters.join(", "),
            assignments.len(),
            if selection.is_some() { ", where" } else { "" }
        ),
    }
}

fn write_physical(plan: &PhysicalPlan, depth: usize, out: &mut String) {
    indent(out, depth);
    match plan {
        PhysicalPlan::EmptyRelation => {
            let _ = writeln!(out, "EmptyRelation");
        }
        PhysicalPlan::Derived {
            table_ref,
            id,
            alias,
            plan,
            correlated,
            rescanned,
        } => {
            let how = if *correlated {
                "outer_row_input=true (LATERAL: re-run for each row of the preceding FROM items)"
            } else if *rescanned {
                "outer_row_input=false (materialized once, re-read for each left row)"
            } else {
                "outer_row_input=false"
            };
            let _ = writeln!(
                out,
                "Derived #{} alias={alias} as t{table_ref} {how}",
                id
            );
            write_physical(plan, depth + 1, out);
        }
        PhysicalPlan::SetOp(node) => write_set_op(node, depth, out, true),
        PhysicalPlan::WorkingTable {
            table_ref, name, ..
        } => {
            let _ = writeln!(
                out,
                "WorkingTable name={name} as t{table_ref} (the rows of the previous iteration)"
            );
        }
        PhysicalPlan::RecursiveCte(node) => {
            let _ = writeln!(
                out,
                "RecursiveCte name={} distinct={} ({})",
                node.name,
                node.distinct,
                if node.distinct {
                    "UNION: stops when an iteration produces no new distinct row"
                } else {
                    "UNION ALL: stops when an iteration produces no row"
                }
            );
            indent(out, depth + 1);
            let _ = writeln!(out, "seed (runs once):");
            write_physical(&node.seed_plan, depth + 2, out);
            indent(out, depth + 1);
            let _ = writeln!(out, "recursive (runs once per iteration, reads the working table '{}'):", node.name);
            write_physical(&node.recursive_plan, depth + 2, out);
        }
        PhysicalPlan::CteNode { table_ref, cte } => {
            let marker = format!("cte#{} defined here", cte.id);
            if out.contains(&marker) {
                let _ = writeln!(
                    out,
                    "CteNode name={} as t{table_ref} cte#{} (reads the rows materialized above)",
                    cte.name, cte.id
                );
            } else {
                let _ = writeln!(
                    out,
                    "CteNode name={} as t{table_ref} {marker} (materialized once per statement)",
                    cte.name
                );
                write_physical(&cte.plan, depth + 1, out);
            }
        }
        PhysicalPlan::Access(access) => {
            write_access(access, out);
            write_subplans(access_exprs(access), depth + 1, out);
        }
        PhysicalPlan::Join {
            left,
            right,
            kind,
            on,
            algorithm,
        } => {
            let kind = match kind {
                JoinKind::Inner => "INNER",
                JoinKind::Left => "LEFT",
            };
            let algo = match algorithm {
                JoinAlgorithm::NestedLoop => "NestedLoop",
                JoinAlgorithm::IndexNestedLoop => "IndexNestedLoop",
            };
            let _ = writeln!(out, "{kind} Join ({algo}) on {}", fmt_expr(on));
            write_subplans([on], depth + 1, out);
            write_physical(left, depth + 1, out);
            write_physical(right, depth + 1, out);
        }
        PhysicalPlan::Filter { input, predicate } => {
            let _ = writeln!(out, "Filter {}", fmt_expr(predicate));
            write_subplans([predicate], depth + 1, out);
            write_physical(input, depth + 1, out);
        }
        PhysicalPlan::Projection { input, items } => {
            let names: Vec<&str> = items.iter().map(|i| i.output_name.as_str()).collect();
            let _ = writeln!(out, "Projection [{}]", names.join(", "));
            write_subplans(items.iter().map(|i| &i.expr), depth + 1, out);
            write_physical(input, depth + 1, out);
        }
        PhysicalPlan::Distinct { input } => {
            let _ = writeln!(out, "Distinct");
            write_physical(input, depth + 1, out);
        }
        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
            grouping,
        } => {
            let group_s: Vec<String> = group_by.iter().map(fmt_expr).collect();
            let agg_s: Vec<String> = aggregates.iter().map(fmt_aggregate).collect();
            let _ = write!(
                out,
                "Aggregate group_by=[{}] aggregates=[{}]",
                group_s.join(", "),
                agg_s.join(", ")
            );
            if let Some(g) = grouping {
                let count = g.set_count();
                let _ = write!(
                    out,
                    " grouping=[{}] grouping_sets={}",
                    fmt_grouping(g),
                    if count == u128::MAX {
                        "more than 2^120".to_string()
                    } else {
                        count.to_string()
                    }
                );
            }
            out.push('\n');
            write_physical(input, depth + 1, out);
        }
        PhysicalPlan::Window {
            input,
            spec,
            functions,
        } => {
            let part: Vec<String> = spec.partition_by.iter().map(fmt_expr).collect();
            let order: Vec<String> = spec.order_by.iter().map(fmt_order_item).collect();
            let funcs: Vec<String> = functions
                .iter()
                .map(|f| format!("{} -> window[{}]", fmt_window_func(&f.func), f.slot))
                .collect();
            let _ = writeln!(
                out,
                "Window partition_by=[{}] order_by=[{}] frame={} functions=[{}]",
                part.join(", "),
                order.join(", "),
                fmt_frame(&spec.frame),
                funcs.join(", ")
            );
            write_physical(input, depth + 1, out);
        }
        PhysicalPlan::Sort { input, items } => {
            let parts: Vec<String> = items
                .iter()
                .map(|i| {
                    format!(
                        "{}{}",
                        fmt_expr(&i.expr),
                        if i.descending { " DESC" } else { " ASC" }
                    )
                })
                .collect();
            let _ = writeln!(out, "Sort [{}]", parts.join(", "));
            write_subplans(items.iter().map(|i| &i.expr), depth + 1, out);
            write_physical(input, depth + 1, out);
        }
        PhysicalPlan::Limit {
            input,
            limit,
            offset,
            pushable,
        } => {
            let limit_s = limit
                .as_ref()
                .map(fmt_expr)
                .unwrap_or_else(|| "-".to_string());
            let offset_s = offset
                .as_ref()
                .map(fmt_expr)
                .unwrap_or_else(|| "-".to_string());
            let _ = writeln!(
                out,
                "Limit limit={limit_s} offset={offset_s} pushable={pushable}"
            );
            write_physical(input, depth + 1, out);
        }
    }
}

/// The operands of a set operation in order, with a left-nested chain of one
/// operator and quantifier (`A UNION B UNION C`) flattened into one list:
/// applying them left to right is exactly what the chain means.
fn set_op_operands(node: &SetOpNode) -> Vec<&PhysicalPlan> {
    let mut out = Vec::new();
    fn gather<'p>(node: &'p SetOpNode, out: &mut Vec<&'p PhysicalPlan>) {
        match node.left.as_ref() {
            PhysicalPlan::SetOp(inner) if inner.op == node.op && inner.all == node.all => {
                gather(inner, out)
            }
            other => out.push(other),
        }
        out.push(node.right.as_ref());
    }
    gather(node, &mut out);
    out
}

fn set_op_name(node: &SetOpNode) -> String {
    format!("{}{}", node.op.name(), if node.all { " ALL" } else { "" })
}

/// `UNION(#1, INTERSECT(#2, #3))`: the tree of operators over the numbered
/// operand queries -- the precedence of the statement, readable at a glance.
fn set_op_shape(node: &SetOpNode, next: &mut usize) -> String {
    let parts: Vec<String> = set_op_operands(node)
        .into_iter()
        .map(|operand| match operand {
            PhysicalPlan::SetOp(inner) => set_op_shape(inner, next),
            _ => {
                *next += 1;
                format!("#{}", *next)
            }
        })
        .collect();
    format!("{}({})", set_op_name(node), parts.join(", "))
}

fn write_set_op(node: &SetOpNode, depth: usize, out: &mut String, root: bool) {
    let operands = set_op_operands(node);
    let names: Vec<&str> = node.items.iter().map(|i| i.output_name.as_str()).collect();
    let shape = if root {
        format!(" shape={}", set_op_shape(node, &mut 0))
    } else {
        String::new()
    };
    let _ = writeln!(
        out,
        "SetOp {} branches={}{shape} as t{} output=[{}]",
        set_op_name(node),
        operands.len(),
        node.set_ref,
        names.join(", ")
    );
    for operand in operands {
        match operand {
            PhysicalPlan::SetOp(inner) => {
                indent(out, depth + 1);
                write_set_op(inner, depth + 1, out, false);
            }
            other => write_physical(other, depth + 1, out),
        }
    }
}

fn write_access(access: &PhysicalAccess, out: &mut String) {
    match access {
        PhysicalAccess::PkLookup {
            table_id,
            table_ref,
            key_values,
            residual,
        } => {
            let keys: Vec<String> = key_values.iter().map(fmt_expr).collect();
            let _ = write!(
                out,
                "PkLookup table_id={table_id} table_ref=t{table_ref} key=({})",
                keys.join(", ")
            );
            write_residual(residual, out);
        }
        PhysicalAccess::IndexScan {
            table_id,
            table_ref,
            index_id,
            index_name,
            mode,
            residual,
            ..
        } => {
            let mode_s = match mode {
                IndexAccessMode::Equality { prefix } => {
                    format!(
                        "Equality({})",
                        prefix.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
                    )
                }
                IndexAccessMode::Range { start, end } => {
                    format!("Range(start={}, end={})", fmt_bound(start), fmt_bound(end))
                }
            };
            let _ = write!(
                out,
                "IndexScan table_id={table_id} table_ref=t{table_ref} index_id={index_id} index_name={index_name} mode={mode_s}"
            );
            write_residual(residual, out);
        }
        PhysicalAccess::SeqScan {
            table_id,
            table_ref,
            predicate,
        } => {
            let _ = write!(out, "SeqScan table_id={table_id} table_ref=t{table_ref}");
            write_residual(predicate, out);
        }
        PhysicalAccess::PkRangeScan {
            table_id,
            table_ref,
            start,
            end,
            residual,
            ..
        } => {
            let _ = write!(
                out,
                "PkRangeScan table_id={table_id} table_ref=t{table_ref} start={} end={}",
                fmt_bound(start),
                fmt_bound(end)
            );
            write_residual(residual, out);
        }
    }
    out.push('\n');
}

fn write_residual(residual: &Option<BoundExpr>, out: &mut String) {
    if let Some(r) = residual {
        let _ = write!(out, " residual={}", fmt_expr(r));
    }
}

fn fmt_bound(bound: &std::ops::Bound<Vec<BoundExpr>>) -> String {
    match bound {
        std::ops::Bound::Unbounded => "unbounded".to_string(),
        std::ops::Bound::Included(v) => format!(
            "incl({})",
            v.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
        ),
        std::ops::Bound::Excluded(v) => format!(
            "excl({})",
            v.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn fmt_aggregate_arg(arg: &crate::bound::BoundAggregateArg) -> String {
    match arg {
        crate::aggregate::AggregateArg::Wildcard => "*".to_string(),
        crate::aggregate::AggregateArg::Expr(e) => fmt_expr(e),
        crate::aggregate::AggregateArg::Multi(es) => {
            es.iter().map(|e| fmt_expr(e)).collect::<Vec<_>>().join(", ")
        }
    }
}

fn fmt_aggregate(a: &crate::bound::BoundAggregateExpr) -> String {
    let arg = fmt_aggregate_arg(&a.arg);
    let mut s = format!(
        "{}({}{})",
        a.func.name(),
        if a.distinct { "DISTINCT " } else { "" },
        arg
    );
    if let Some(f) = &a.filter {
        let _ = write!(s, " FILTER (WHERE {})", fmt_expr(f));
    }
    s
}

fn fmt_order_item(i: &BoundOrderByItem) -> String {
    format!(
        "{} {} NULLS {}",
        fmt_expr(&i.expr),
        if i.descending { "DESC" } else { "ASC" },
        match i.nulls {
            NullsOrder::First => "FIRST",
            NullsOrder::Last => "LAST",
        }
    )
}

fn fmt_frame_bound(b: &BoundFrameBound) -> String {
    match b {
        BoundFrameBound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_string(),
        BoundFrameBound::Preceding(e) => format!("{} PRECEDING", fmt_expr(e)),
        BoundFrameBound::CurrentRow => "CURRENT ROW".to_string(),
        BoundFrameBound::Following(e) => format!("{} FOLLOWING", fmt_expr(e)),
        BoundFrameBound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_string(),
    }
}

fn fmt_frame(f: &BoundFrame) -> String {
    format!(
        "{} BETWEEN {} AND {}{}",
        match f.units {
            crate::ast::FrameUnits::Rows => "ROWS",
            crate::ast::FrameUnits::Range => "RANGE",
            crate::ast::FrameUnits::Groups => "GROUPS",
        },
        fmt_frame_bound(&f.start),
        fmt_frame_bound(&f.end),
        if f.explicit { "" } else { " (default)" }
    )
}

fn nulls_clause(ignore_nulls: bool) -> &'static str {
    if ignore_nulls {
        " IGNORE NULLS"
    } else {
        ""
    }
}

fn fmt_window_func(f: &BoundWindowFunc) -> String {
    let opt = |e: &Option<Box<BoundExpr>>| {
        e.as_ref()
            .map(|e| format!(", {}", fmt_expr(e)))
            .unwrap_or_default()
    };
    match f {
        BoundWindowFunc::Aggregate {
            func,
            arg,
            distinct,
            filter,
        } => {
            let arg = fmt_aggregate_arg(arg);
            let mut s = format!(
                "{}({}{arg})",
                func.name(),
                if *distinct { "DISTINCT " } else { "" }
            );
            if let Some(f) = filter {
                let _ = write!(s, " FILTER (WHERE {})", fmt_expr(f));
            }
            s
        }
        BoundWindowFunc::RowNumber
        | BoundWindowFunc::Rank
        | BoundWindowFunc::DenseRank
        | BoundWindowFunc::PercentRank
        | BoundWindowFunc::CumeDist => format!("{}()", f.name()),
        BoundWindowFunc::Ntile(n) => format!("ntile({})", fmt_expr(n)),
        BoundWindowFunc::Lag {
            expr,
            offset,
            default,
            ignore_nulls,
        }
        | BoundWindowFunc::Lead {
            expr,
            offset,
            default,
            ignore_nulls,
        } => format!(
            "{}({}{}{}){}",
            f.name(),
            fmt_expr(expr),
            opt(offset),
            opt(default),
            nulls_clause(*ignore_nulls)
        ),
        BoundWindowFunc::FirstValue { expr, ignore_nulls }
        | BoundWindowFunc::LastValue { expr, ignore_nulls } => {
            format!("{}({}){}", f.name(), fmt_expr(expr), nulls_clause(*ignore_nulls))
        }
        BoundWindowFunc::NthValue {
            expr,
            n,
            ignore_nulls,
        } => {
            format!(
                "nth_value({}, {}){}",
                fmt_expr(expr),
                fmt_expr(n),
                nulls_clause(*ignore_nulls)
            )
        }
    }
}

fn fmt_grouping(g: &BoundGrouping) -> String {
    let members = |sets: &[Vec<usize>]| {
        sets.iter()
            .map(|s| {
                let parts: Vec<String> = s.iter().map(|i| format!("key[{i}]")).collect();
                if parts.len() == 1 {
                    parts[0].clone()
                } else {
                    format!("({})", parts.join(", "))
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    g.elements
        .iter()
        .map(|e| match e {
            BoundGroupElement::Expr(i) => format!("key[{i}]"),
            BoundGroupElement::Rollup(m) => format!("ROLLUP({})", members(m)),
            BoundGroupElement::Cube(m) => format!("CUBE({})", members(m)),
            BoundGroupElement::Sets(s) => format!(
                "GROUPING SETS({})",
                s.iter()
                    .map(|set| {
                        format!(
                            "({})",
                            set.iter().map(|i| format!("key[{i}]")).collect::<Vec<_>>().join(", ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A compact, deterministic, non-SQL-reconstructing rendering of a
/// `BoundExpr` — column references by `(table_ref, ordinal)`, literals
/// by their own typed `Debug` (never raw user-supplied SQL text, since
/// there is none left in a `BoundExpr` to echo), parameters as `$n`.
fn fmt_expr(expr: &BoundExpr) -> String {
    match &expr.kind {
        BoundExprKind::Literal(None) => "NULL".to_string(),
        BoundExprKind::Literal(Some(v)) => format!("{v:?}"),
        BoundExprKind::Parameter { index } => format!("${index}"),
        BoundExprKind::Column(c) => format!("col(t{}.{})", c.table_ref.0, c.ordinal),
        BoundExprKind::UnaryOp { op, expr } => {
            let op = match op {
                UnaryOp::Neg => "-",
                UnaryOp::Not => "NOT ",
            };
            format!("({op}{})", fmt_expr(expr))
        }
        BoundExprKind::BinaryOp { left, op, right } => {
            let op = match op {
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::Eq => "=",
                BinaryOp::NotEq => "<>",
                BinaryOp::Lt => "<",
                BinaryOp::LtEq => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::GtEq => ">=",
                BinaryOp::And => "AND",
                BinaryOp::Or => "OR",
            };
            format!("({} {op} {})", fmt_expr(left), fmt_expr(right))
        }
        BoundExprKind::IsNull { expr, negated } => {
            format!(
                "({} IS {}NULL)",
                fmt_expr(expr),
                if *negated { "NOT " } else { "" }
            )
        }
        BoundExprKind::Between {
            expr,
            negated,
            low,
            high,
        } => format!(
            "({} {}BETWEEN {} AND {})",
            fmt_expr(expr),
            if *negated { "NOT " } else { "" },
            fmt_expr(low),
            fmt_expr(high)
        ),
        BoundExprKind::InList {
            expr,
            list,
            negated,
        } => format!(
            "({} {}IN ({}))",
            fmt_expr(expr),
            if *negated { "NOT " } else { "" },
            list.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
        ),
        BoundExprKind::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => format!(
            "({} {}{} {})",
            fmt_expr(expr),
            if *negated { "NOT " } else { "" },
            if *case_insensitive { "ILIKE" } else { "LIKE" },
            fmt_expr(pattern)
        ),
        BoundExprKind::Case { .. } => "CASE(...)".to_string(),
        BoundExprKind::Function { name, args } => {
            format!(
                "{name}({})",
                args.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
            )
        }
        BoundExprKind::Aggregate(agg) => fmt_aggregate(agg),
        BoundExprKind::AggregateRef(idx) => format!("agg[{idx}]"),
        BoundExprKind::WindowRef(idx) => format!("window[{idx}]"),
        BoundExprKind::GroupKeyRef(idx) => format!("key[{idx}]"),
        BoundExprKind::Grouping(idxs) => format!(
            "GROUPING({})",
            idxs.iter().map(|i| format!("key[{i}]")).collect::<Vec<_>>().join(", ")
        ),
        BoundExprKind::Window(call) => format!(
            "{} OVER (...)",
            fmt_window_func(&call.func)
        ),
        BoundExprKind::Subquery(sq) => match sq.kind {
            SubqueryKind::Scalar => format!("subquery#{}", sq.id),
            SubqueryKind::Exists { negated } => {
                format!("{}EXISTS(subquery#{})", if negated { "NOT " } else { "" }, sq.id)
            }
            SubqueryKind::In { negated } => format!(
                "({} {}IN subquery#{})",
                sq.operand
                    .as_ref()
                    .map(fmt_expr)
                    .unwrap_or_else(|| "?".to_string()),
                if negated { "NOT " } else { "" },
                sq.id
            ),
            SubqueryKind::Quantified { op, all } => format!(
                "({} {} {} subquery#{})",
                sq.operand
                    .as_ref()
                    .map(fmt_expr)
                    .unwrap_or_else(|| "?".to_string()),
                match op {
                    BinaryOp::Eq => "=",
                    BinaryOp::NotEq => "<>",
                    BinaryOp::Lt => "<",
                    BinaryOp::LtEq => "<=",
                    BinaryOp::Gt => ">",
                    BinaryOp::GtEq => ">=",
                    _ => "?",
                },
                if all { "ALL" } else { "ANY" },
                sq.id
            ),
        },
    }
}

/// Every subquery expression directly contained in `expr` (not those nested
/// inside another subquery's own query -- those are rendered when that
/// subquery's plan is).
pub(super) fn collect_subqueries<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundSubqueryExpr>) {
    match &expr.kind {
        BoundExprKind::Literal(_)
        | BoundExprKind::Parameter { .. }
        | BoundExprKind::Column(_)
        | BoundExprKind::AggregateRef(_)
        | BoundExprKind::WindowRef(_)
        | BoundExprKind::GroupKeyRef(_)
        | BoundExprKind::Grouping(_)
        | BoundExprKind::Window(_) => {}
        BoundExprKind::UnaryOp { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            collect_subqueries(expr, out)
        }
        BoundExprKind::BinaryOp { left, right, .. } => {
            collect_subqueries(left, out);
            collect_subqueries(right, out);
        }
        BoundExprKind::Between {
            expr, low, high, ..
        } => {
            collect_subqueries(expr, out);
            collect_subqueries(low, out);
            collect_subqueries(high, out);
        }
        BoundExprKind::InList { expr, list, .. } => {
            collect_subqueries(expr, out);
            list.iter().for_each(|e| collect_subqueries(e, out));
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            collect_subqueries(expr, out);
            collect_subqueries(pattern, out);
        }
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => {
            if let Some(o) = operand {
                collect_subqueries(o, out);
            }
            for (w, t) in branches {
                collect_subqueries(w, out);
                collect_subqueries(t, out);
            }
            if let Some(e) = else_result {
                collect_subqueries(e, out);
            }
        }
        BoundExprKind::Function { args, .. } => args.iter().for_each(|e| collect_subqueries(e, out)),
        BoundExprKind::Aggregate(agg) => {
            for e in agg.arg.exprs() {
                collect_subqueries(e, out);
            }
            if let Some(f) = &agg.filter {
                collect_subqueries(f, out);
            }
        }
        BoundExprKind::Subquery(sq) => {
            if let Some(o) = &sq.operand {
                collect_subqueries(o, out);
            }
            out.push(sq);
        }
    }
}

/// Renders the plan of every subquery contained in `exprs`, one level below
/// the node that evaluates them.
fn write_subplans<'a>(
    exprs: impl IntoIterator<Item = &'a BoundExpr>,
    depth: usize,
    out: &mut String,
) {
    let mut subs = Vec::new();
    for e in exprs {
        collect_subqueries(e, &mut subs);
    }
    for sq in subs {
        indent(out, depth);
        if sq.outer_refs.is_empty() {
            let _ = writeln!(out, "Subquery #{} uncorrelated (executed once)", sq.id);
        } else {
            // The plan below reads these outer columns for every row of the
            // enclosing query; EXPLAIN shows that and runs none of it.
            let refs: Vec<String> = sq
                .outer_refs
                .iter()
                .map(|c| format!("col(t{}.{})", c.table_ref.0, c.ordinal))
                .collect();
            let _ = writeln!(
                out,
                "Subquery #{} correlated outer_row_input=true (re-executed per outer row; outer columns: {})",
                sq.id,
                refs.join(", ")
            );
        }
        match &sq.plan {
            Some(plan) => write_physical(plan, depth + 1, out),
            None => {
                indent(out, depth + 1);
                let _ = writeln!(out, "(unplanned)");
            }
        }
    }
}

pub(super) fn access_exprs(access: &PhysicalAccess) -> Vec<&BoundExpr> {
    let mut v: Vec<&BoundExpr> = Vec::new();
    match access {
        PhysicalAccess::PkLookup {
            key_values,
            residual,
            ..
        } => {
            v.extend(key_values.iter());
            v.extend(residual.iter());
        }
        PhysicalAccess::IndexScan { mode, residual, .. } => {
            match mode {
                IndexAccessMode::Equality { prefix } => v.extend(prefix.iter()),
                IndexAccessMode::Range { start, end } => {
                    for b in [start, end] {
                        if let std::ops::Bound::Included(x) | std::ops::Bound::Excluded(x) = b {
                            v.extend(x.iter());
                        }
                    }
                }
            }
            v.extend(residual.iter());
        }
        PhysicalAccess::SeqScan { predicate, .. } => v.extend(predicate.iter()),
        PhysicalAccess::PkRangeScan {
            start,
            end,
            residual,
            ..
        } => {
            for b in [start, end] {
                if let std::ops::Bound::Included(x) | std::ops::Bound::Excluded(x) = b {
                    v.extend(x.iter());
                }
            }
            v.extend(residual.iter());
        }
    }
    v
}
