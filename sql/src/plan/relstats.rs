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
        } => {
            walk_exprs(group_by.iter(), depth, stats);
            walk_exprs(
                aggregates.iter().filter_map(|a| match &a.arg {
                    crate::aggregate::AggregateArg::Expr(e) => Some(e.as_ref()),
                    crate::aggregate::AggregateArg::Wildcard => None,
                }),
                depth,
                stats,
            );
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
