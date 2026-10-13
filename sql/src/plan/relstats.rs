//! Whole-statement counts over a finished plan (SQL-mission-1 Slice C): how
//! many CTEs it defines and references, and how deeply derived tables nest.
//! The executor checks them against `ExecLimits` before it starts any work.

use std::collections::BTreeSet;

use crate::bound::{BoundExpr, BoundSubqueryExpr};
use crate::plan::explain::{access_exprs, collect_subqueries};
use crate::plan::physical::PhysicalPlan;
use crate::plan::Plan;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RelationStats {
    /// Ids of the distinct CTE definitions reachable from the plan.
    pub cte_definitions: BTreeSet<u32>,
    /// `table_ref`s of the distinct CTE references (each `FROM name` site).
    pub cte_references: BTreeSet<u32>,
    /// Deepest nesting of derived tables.
    pub max_derived_depth: usize,
    /// Operand queries of the set operations the plan executes (SQL-mission-1
    /// Slice E): for every set-operation node, one per operand that is not
    /// itself directly a set-operation node. `A UNION B INTERSECT C` is 3;
    /// a CTE read twice is counted once; a CTE nobody reads, not at all.
    pub set_op_branches: u64,
    /// Ids of the distinct recursive CTEs the plan runs (SQL-mission-1 Slice D);
    /// one nobody reads is bound and planned but never runs, so is not counted.
    pub recursive_cte_definitions: BTreeSet<u32>,
    /// Window function calls the plan evaluates (SQL-mission-2), over the whole
    /// statement including subqueries and CTEs (a CTE read twice counts once).
    pub window_functions: u64,
    /// Grouping sets the plan's aggregations expand to (SQL-mission-2), summed
    /// over every aggregation of the statement; a plain `GROUP BY` counts as
    /// none. Saturating, because `CUBE` of many columns is astronomically large.
    pub grouping_sets: u128,
}

/// Every `WITH` definition the statement contains, referenced or not.
pub fn defined_ctes(plan: &Plan) -> u32 {
    match plan {
        Plan::Query { defined_ctes, .. }
        | Plan::Update { defined_ctes, .. }
        | Plan::Delete { defined_ctes, .. } => *defined_ctes,
        Plan::Explain(inner) => defined_ctes(inner),
        Plan::Insert(_) | Plan::Ddl(_) | Plan::Begin | Plan::Commit | Plan::Rollback => 0,
    }
}

pub fn plan_stats(plan: &Plan) -> RelationStats {
    let mut stats = RelationStats::default();
    match plan {
        Plan::Query { physical, .. } => walk(physical, 0, &mut stats),
        Plan::Update {
            assignments,
            access,
            ..
        } => {
            walk_exprs(access_exprs(access), 0, &mut stats);
            walk_exprs(assignments.iter().map(|a| &a.value), 0, &mut stats);
        }
        Plan::Delete { access, .. } => walk_exprs(access_exprs(access), 0, &mut stats),
        Plan::Explain(inner) => return plan_stats(inner),
        Plan::Insert(_) | Plan::Ddl(_) | Plan::Begin | Plan::Commit | Plan::Rollback => {}
    }
    stats
}

fn walk_exprs<'a>(
    exprs: impl IntoIterator<Item = &'a BoundExpr>,
    depth: usize,
    stats: &mut RelationStats,
) {
    let mut subs: Vec<&BoundSubqueryExpr> = Vec::new();
    for e in exprs {
        collect_subqueries(e, &mut subs);
    }
    for sq in subs {
        if let Some(plan) = &sq.plan {
            walk(plan, depth, stats);
        }
    }
}

fn walk(plan: &PhysicalPlan, depth: usize, stats: &mut RelationStats) {
    stats.max_derived_depth = stats.max_derived_depth.max(depth);
    match plan {
        PhysicalPlan::EmptyRelation => {}
        PhysicalPlan::Access(access) => walk_exprs(access_exprs(access), depth, stats),
        PhysicalPlan::Derived { plan, .. } => walk(plan, depth + 1, stats),
        PhysicalPlan::WorkingTable { .. } => {}
        PhysicalPlan::RecursiveCte(node) => {
            stats.recursive_cte_definitions.insert(node.cte_id);
            walk(&node.seed_plan, depth, stats);
            walk(&node.recursive_plan, depth, stats);
        }
        PhysicalPlan::SetOp(node) => {
            for operand in [&node.left, &node.right] {
                if !matches!(operand.as_ref(), PhysicalPlan::SetOp(_)) {
                    stats.set_op_branches += 1;
                }
                walk(operand, depth, stats);
            }
        }
        PhysicalPlan::CteNode { table_ref, cte } => {
            stats.cte_references.insert(*table_ref);
            // A definition is walked once, however many references it has.
            if stats.cte_definitions.insert(cte.id) {
                walk(&cte.plan, depth, stats);
            }
        }
        PhysicalPlan::Join {
            left, right, on, ..
        } => {
            walk_exprs([on], depth, stats);
            walk(left, depth, stats);
            walk(right, depth, stats);
        }
        PhysicalPlan::Filter { input, predicate } => {
            walk_exprs([predicate], depth, stats);
            walk(input, depth, stats);
        }
        PhysicalPlan::Projection { input, items } => {
            walk_exprs(items.iter().map(|i| &i.expr), depth, stats);
            walk(input, depth, stats);
        }
        PhysicalPlan::Distinct { input } => walk(input, depth, stats),
        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
            grouping,
        } => {
            if let Some(g) = grouping {
                stats.grouping_sets = stats.grouping_sets.saturating_add(g.set_count());
            }
            walk_exprs(group_by.iter(), depth, stats);
            walk_exprs(
                aggregates.iter().flat_map(|a| {
                    let arg = match &a.arg {
                        crate::aggregate::AggregateArg::Expr(e) => Some(e.as_ref()),
                        crate::aggregate::AggregateArg::Wildcard => None,
                    };
                    arg.into_iter().chain(a.filter.as_deref())
                }),
                depth,
                stats,
            );
            walk(input, depth, stats);
        }
        PhysicalPlan::Window {
            input, functions, ..
        } => {
            stats.window_functions += functions.len() as u64;
            walk(input, depth, stats);
        }
        PhysicalPlan::Sort { input, items } => {
            walk_exprs(items.iter().map(|i| &i.expr), depth, stats);
            walk(input, depth, stats);
        }
        PhysicalPlan::Limit {
            input,
            limit,
            offset,
            ..
        } => {
            walk_exprs(limit.iter().chain(offset.iter()), depth, stats);
            walk(input, depth, stats);
        }
    }
}
