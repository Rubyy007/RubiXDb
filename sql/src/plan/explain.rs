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
use crate::bound::{BoundExpr, BoundExprKind, BoundSubqueryExpr, SubqueryKind};
use crate::plan::access::{IndexAccessMode, PhysicalAccess};
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
            let _ = writeln!(
                out,
                "Insert table_id={} rows={}",
                insert.table_id,
                insert.rows.len()
            );
        }
        Plan::Update {
            table,
            assignments,
            access,
            ..
        } => {
            let _ = writeln!(
                out,
                "Update table_id={} assignments={}",
                table.table_id,
                assignments.len()
            );
            indent(out, depth + 1);
            write_access(access, out);
            write_subplans(access_exprs(access), depth + 2, out);
            write_subplans(assignments.iter().map(|a| &a.value), depth + 1, out);
        }
        Plan::Delete { table, access, .. } => {
            let _ = writeln!(out, "Delete table_id={}", table.table_id);
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
        } => {
            let group_s: Vec<String> = group_by.iter().map(fmt_expr).collect();
            let agg_s: Vec<String> = aggregates
                .iter()
                .map(|a| {
                    let arg = match &a.arg {
                        crate::aggregate::AggregateArg::Wildcard => "*".to_string(),
                        crate::aggregate::AggregateArg::Expr(e) => fmt_expr(e),
                    };
                    format!("{}({})", a.func.name(), arg)
                })
                .collect();
            let _ = writeln!(
                out,
                "Aggregate group_by=[{}] aggregates=[{}]",
                group_s.join(", "),
                agg_s.join(", ")
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
        BoundExprKind::Aggregate(agg) => {
            let arg = match &agg.arg {
                crate::aggregate::AggregateArg::Wildcard => "*".to_string(),
                crate::aggregate::AggregateArg::Expr(e) => fmt_expr(e),
            };
            format!("{}({})", agg.func.name(), arg)
        }
        BoundExprKind::AggregateRef(idx) => format!("agg[{idx}]"),
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
        | BoundExprKind::AggregateRef(_) => {}
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
            if let crate::aggregate::AggregateArg::Expr(e) = &agg.arg {
                collect_subqueries(e, out);
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
