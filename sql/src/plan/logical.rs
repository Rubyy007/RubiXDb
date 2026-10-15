//! The logical plan — relational *intent*, independent of a physical
//! access path (item 4: "Separate: logical meaning from: physical
//! access choice"). Built directly from a `BoundSelect`, before any
//! optimization rule runs. No plan node here embeds raw SQL text (item
//! 3) — every field is either already-bound catalog metadata
//! (`BoundTableRef`) or an already-typed `BoundExpr`.

use crate::ast::{JoinKind, SetOperator};
use rubixdb::relational::RelationalType;
use std::sync::Arc;

use crate::bound::{
    BoundAggregateExpr, BoundExpr, BoundGrouping, BoundOrderByItem, BoundRelation, BoundSelect,
    BoundSelectItem, BoundTableRef, BoundWindowFunc, BoundWindowSpec,
};
use crate::plan::physical::{CtePlan, PhysicalPlan};
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
    /// A derived table, `(SELECT ...) AS alias` (SQL-mission-1 Slice C): a
    /// leaf whose rows are the already-planned body's output, exposed as the
    /// relation `table`. No predicate is pushed into it.
    Derived {
        table: BoundTableRef,
        id: u32,
        plan: Box<PhysicalPlan>,
        /// `LATERAL` and actually reading an earlier `FROM` item.
        correlated: bool,
    },
    /// A reference to a CTE: a leaf reading the CTE's materialized rows.
    Cte {
        table: BoundTableRef,
        cte: Arc<CtePlan>,
    },
    /// The working table of a recursive CTE, read inside its recursive term
    /// (SQL-mission-1 Slice D): a leaf whose rows are the previous iteration's.
    Working {
        table: BoundTableRef,
        work_id: u32,
        name: String,
    },
    /// A set operation (SQL-mission-1 Slice E): a leaf whose rows are the
    /// result of combining its two already-planned operands. It is never a
    /// join; `ORDER BY` / `LIMIT` of the operation sit above it as `Sort` /
    /// `Limit` nodes that read its output relation.
    SetOp(SetOpNode),
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
        /// SQL-mission-2: `Some` for `ROLLUP` / `CUBE` / `GROUPING SETS`; then
        /// `group_by` lists the distinct grouping expressions and the output has
        /// one row per group of every grouping set.
        grouping: Option<BoundGrouping>,
    },
    /// Window functions (SQL-mission-2). Sits above `GROUP BY` / `HAVING` (or the
    /// `WHERE` filter) and below `QUALIFY` and the final projection: it keeps
    /// every input row and adds, for each of `functions`, the value of that
    /// function for the row (read back through `WindowRef(slot)`). One node per
    /// distinct window (same `PARTITION BY`, `ORDER BY` and frame); the functions
    /// of provably identical windows share a node.
    Window {
        input: Box<LogicalPlan>,
        spec: BoundWindowSpec,
        functions: Vec<WindowSlot>,
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

/// One window function of a `Window` node and the slot (index into
/// `BoundSelect::windows`) its result is stored in.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSlot {
    pub slot: usize,
    pub func: BoundWindowFunc,
}

/// The operands and shape of a set operation, shared by the logical and the
/// physical plan.
#[derive(Debug, Clone, PartialEq)]
pub struct SetOpNode {
    pub op: SetOperator,
    pub all: bool,
    /// The operation's output relation (`BoundSetOp::set_ref`).
    pub set_ref: u32,
    pub left: Box<PhysicalPlan>,
    pub right: Box<PhysicalPlan>,
    /// Unified type of each output column; operand values are widened to it.
    pub column_types: Vec<Option<RelationalType>>,
    /// `UNION BY NAME`: the operands' column alignment (`None`: positional).
    pub by_name: Option<crate::bound::NameAlignment>,
    /// The output columns (names, unified types): what the operation yields.
    pub items: Vec<BoundSelectItem>,
}

/// Builds the raw, unoptimized logical plan from a bound `SELECT`.
/// Evaluation-order-correct node nesting (`FROM`/`JOIN` → `WHERE` →
/// `SELECT`/`DISTINCT` → `ORDER BY` → `LIMIT`/`OFFSET`) — matches
/// standard SQL clause evaluation order exactly (`DISTINCT` de-
/// duplicates the *projected* output before `ORDER BY` sees it, and
/// `ORDER BY` may reference an output alias `DISTINCT` has already
/// collapsed duplicates for).
pub fn build_logical_plan(select: &BoundSelect, limits: &PlannerLimits) -> Result<LogicalPlan> {
    // A set operation has no FROM / WHERE / GROUP BY / SELECT list of its own:
    // its rows come from the operation, and only its ORDER BY / LIMIT remain.
    if let Some(set) = &select.set_op {
        return Ok(finish_order_and_limit(
            LogicalPlan::SetOp(set_op_node(set, select)?),
            select,
        ));
    }
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
        Some(first) => relation_leaf(first)?,
    };

    for item in select.from.iter().skip(1) {
        let (kind, on) = item
            .join
            .clone()
            .expect("crate::bind always sets `join: Some(..)` on every non-first BoundFromItem");
        plan = LogicalPlan::Join {
            left: Box::new(plan),
            right: Box::new(relation_leaf(item)?),
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
    let is_aggregated = !select.group_by.is_empty()
        || select.grouping.is_some()
        || select.having.is_some()
        || !select.aggregates.is_empty();
    if is_aggregated {
        plan = LogicalPlan::Aggregate {
            input: Box::new(plan),
            group_by: select.group_by.clone(),
            aggregates: select.aggregates.clone(),
            grouping: select.grouping.clone(),
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

    // SQL-mission-2: window functions, one node per distinct window, then
    // QUALIFY -- after the window functions it filters on, before the
    // projection, DISTINCT, ORDER BY and LIMIT.
    let mut nodes: Vec<(BoundWindowSpec, Vec<WindowSlot>)> = Vec::new();
    for (slot, call) in select.windows.iter().enumerate() {
        let entry = WindowSlot {
            slot,
            func: call.func.clone(),
        };
        // Merged only when the window is provably the same one.
        match nodes.iter_mut().find(|(spec, _)| spec.same_window(&call.spec)) {
            Some((_, functions)) => functions.push(entry),
            None => nodes.push((call.spec.clone(), vec![entry])),
        }
    }
    for (spec, functions) in nodes {
        plan = LogicalPlan::Window {
            input: Box::new(plan),
            spec,
            functions,
        };
    }
    if let Some(qualify) = &select.qualify {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: qualify.clone(),
        };
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

    Ok(finish_order_and_limit(plan, select))
}

/// `ORDER BY` then `LIMIT` / `OFFSET` on top of `plan`.
fn finish_order_and_limit(mut plan: LogicalPlan, select: &BoundSelect) -> LogicalPlan {
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

    plan
}

fn set_op_node(set: &crate::bound::BoundSetOp, shell: &BoundSelect) -> Result<SetOpNode> {
    let missing = |side: &str| SqlError::PlanValidation {
        detail: format!("a set operation reached logical planning without a physical plan for its {side} operand"),
    };
    Ok(SetOpNode {
        op: set.op,
        all: set.all,
        set_ref: set.set_ref,
        left: Box::new(set.left_plan.clone().ok_or_else(|| missing("left"))?),
        right: Box::new(set.right_plan.clone().ok_or_else(|| missing("right"))?),
        column_types: set.column_types.clone(),
        by_name: set.by_name.clone(),
        items: shell.projection.clone(),
    })
}

/// The leaf node for one `FROM` item: a `Scan` of a catalog table, or a
/// derived table / CTE reference carrying its already-planned body.
fn relation_leaf(item: &crate::bound::BoundFromItem) -> Result<LogicalPlan> {
    Ok(match &item.relation {
        BoundRelation::Table => LogicalPlan::Scan {
            table: item.table.clone(),
            predicate: None,
            required_columns: None,
        },
        BoundRelation::Derived(d) => LogicalPlan::Derived {
            table: item.table.clone(),
            id: d.id,
            plan: Box::new(d.plan.clone().ok_or_else(|| SqlError::PlanValidation {
                detail: "a derived table reached logical planning without a physical plan".to_string(),
            })?),
            correlated: d.lateral && !d.outer_refs.is_empty(),
        },
        BoundRelation::Working(w) => LogicalPlan::Working {
            table: item.table.clone(),
            work_id: w.work_id,
            name: w.name.clone(),
        },
        BoundRelation::Cte(r) => LogicalPlan::Cte {
            table: item.table.clone(),
            cte: r.plan.clone().ok_or_else(|| SqlError::PlanValidation {
                detail: "a CTE reference reached logical planning without a planned definition".to_string(),
            })?,
        },
    })
}

/// Total node count — used by `crate::plan::optimize`/`physical` to
/// enforce `PlannerLimits::max_plan_nodes` (item 26) without needing a
/// separate visitor per pass.
pub fn node_count(plan: &LogicalPlan) -> usize {
    match plan {
        LogicalPlan::EmptyRelation
        | LogicalPlan::Scan { .. }
        | LogicalPlan::Derived { .. }
        | LogicalPlan::Cte { .. }
        | LogicalPlan::Working { .. }
        | LogicalPlan::SetOp(_) => 1,
        LogicalPlan::Join { left, right, .. } => 1 + node_count(left) + node_count(right),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Projection { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. } => 1 + node_count(input),
    }
}
