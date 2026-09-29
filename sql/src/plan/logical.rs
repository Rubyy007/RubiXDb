//! The logical plan — relational *intent*, independent of a physical
//! access path (item 4: "Separate: logical meaning from: physical
//! access choice"). Built directly from a `BoundSelect`, before any
//! optimization rule runs. No plan node here embeds raw SQL text (item
//! 3) — every field is either already-bound catalog metadata
//! (`BoundTableRef`) or an already-typed `BoundExpr`.

use crate::ast::JoinKind;
use crate::bound::{
    BoundAggregateExpr, BoundExpr, BoundOrderByItem, BoundSelect, BoundSelectItem, BoundTableRef,
};
use crate::error::{Result, SqlError};
use crate::plan::limits::PlannerLimits;

#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    /// A `FROM`-less `SELECT` (`SELECT 1 + 1`) — the SQL-standard
    /// single implicit row with no columns. No storage primitive is
    /// needed or used (item 5 does not apply — this is not a storage
    /// access node at all).
    EmptyRelation,
    Scan {
        table: BoundTableRef,
        /// Predicate conjuncts pushdown (item 12) has proven safe to
        /// evaluate directly against this table's own rows — `None`
        /// until `crate::plan::optimize` runs (the unoptimized logical
        /// plan never has this populated by `build_logical_plan`
        /// itself, only by the pushdown rule).
        predicate: Option<BoundExpr>,
        /// The column ordinals projection pruning (item 13) has
        /// determined this scan must actually produce — `None` means
        /// "not yet pruned, assume every column is needed" (the safe
        /// default `build_logical_plan` itself uses).
        required_columns: Option<Vec<u16>>,
    },
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        kind: JoinKind,
        on: BoundExpr,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: BoundExpr,
    },
    Projection {
        input: Box<LogicalPlan>,
        items: Vec<BoundSelectItem>,
    },
    Distinct {
        input: Box<LogicalPlan>,
    },
    /// `GROUP BY`/aggregate functions (Increment 11,
    /// `PHASE_RELATIONAL_AGGREGATION_ARCHITECTURE.md`). Sits between
    /// `WHERE`'s `Filter` and the final `Projection` — evaluation-order-
    /// correct (`WHERE` filters input rows *before* grouping; `HAVING`
    /// filters *groups*, so it is represented as an ordinary `Filter`
    /// wrapped directly around this node, reusing the same three-valued-
    /// logic `Filter` semantics rather than inventing a second one, item
    /// 21). `group_by` empty (no `GROUP BY` clause) with a non-empty
    /// `aggregates` means "the whole input is one implicit group" (item
    /// 11/44's "a single aggregate result row, even over zero qualifying
    /// rows").
    Aggregate {
        input: Box<LogicalPlan>,
        group_by: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregateExpr>,
    },
    Sort {
        input: Box<LogicalPlan>,
        items: Vec<BoundOrderByItem>,
    },
    Limit {
        input: Box<LogicalPlan>,
        limit: Option<BoundExpr>,
        offset: Option<BoundExpr>,
    },
}

/// Builds the raw, unoptimized logical plan from a bound `SELECT`.
/// Evaluation-order-correct node nesting (`FROM`/`JOIN` → `WHERE` →
/// `SELECT`/`DISTINCT` → `ORDER BY` → `LIMIT`/`OFFSET`) — matches
/// standard SQL clause evaluation order exactly (`DISTINCT` de-
/// duplicates the *projected* output before `ORDER BY` sees it, and
/// `ORDER BY` may reference an output alias `DISTINCT` has already
/// collapsed duplicates for).
pub fn build_logical_plan(select: &BoundSelect, limits: &PlannerLimits) -> Result<LogicalPlan> {
    if select.from.len() > limits.max_joins {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "statement references {} tables; planner max_joins is {}",
                select.from.len(),
                limits.max_joins
            ),
        });
    }

    let mut plan = match select.from.first() {
        None => LogicalPlan::EmptyRelation,
        Some(first) => LogicalPlan::Scan {
            table: first.table.clone(),
            predicate: None,
            required_columns: None,
        },
    };

    for item in select.from.iter().skip(1) {
        let (kind, on) = item
            .join
            .clone()
            .expect("crate::bind always sets `join: Some(..)` on every non-first BoundFromItem");
        plan = LogicalPlan::Join {
            left: Box::new(plan),
            right: Box::new(LogicalPlan::Scan {
                table: item.table.clone(),
                predicate: None,
                required_columns: None,
            }),
            kind,
            on,
        };
    }

    if let Some(predicate) = &select.selection {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: predicate.clone(),
        };
    }

    // item 16: the same three conditions `crate::bind::select` uses to
    // decide `is_aggregated` — kept independently re-derivable here
    // (rather than carried as a redundant flag) so this function's own
    // output is provably a pure function of `BoundSelect`'s real fields.
    let is_aggregated =
        !select.group_by.is_empty() || select.having.is_some() || !select.aggregates.is_empty();
    if is_aggregated {
        plan = LogicalPlan::Aggregate {
            input: Box::new(plan),
            group_by: select.group_by.clone(),
            aggregates: select.aggregates.clone(),
        };
        // HAVING = an ordinary Filter directly above Aggregate — item 20/
        // 21: reuses Filter's own three-valued-logic semantics verbatim,
        // never a second boolean model, and its placement (above, never
        // pushed below, Aggregate) is exactly what makes it evaluate
        // once per *group* rather than once per input row.
        if let Some(having) = &select.having {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: having.clone(),
            };
        }
    }

    plan = LogicalPlan::Projection {
        input: Box::new(plan),
        items: select.projection.clone(),
    };

    if select.distinct {
        plan = LogicalPlan::Distinct {
            input: Box::new(plan),
        };
    }

    if !select.order_by.is_empty() {
        plan = LogicalPlan::Sort {
            input: Box::new(plan),
            items: select.order_by.clone(),
        };
    }

    if select.limit.is_some() || select.offset.is_some() {
        plan = LogicalPlan::Limit {
            input: Box::new(plan),
            limit: select.limit.clone(),
            offset: select.offset.clone(),
        };
    }

    Ok(plan)
}

/// Total node count — used by `crate::plan::optimize`/`physical` to
/// enforce `PlannerLimits::max_plan_nodes` (item 26) without needing a
/// separate visitor per pass.
pub fn node_count(plan: &LogicalPlan) -> usize {
    match plan {
        LogicalPlan::EmptyRelation | LogicalPlan::Scan { .. } => 1,
        LogicalPlan::Join { left, right, .. } => 1 + node_count(left) + node_count(right),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Projection { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => 1 + node_count(input),
    }
}
