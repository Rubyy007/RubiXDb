//! Bounded-cardinality planner metrics — item 50. Same discipline as
//! `crate::metrics::SqlMetrics`: counters only, every recorder method
//! takes only already-classified, small-enum-shaped arguments, so there
//! is no way to pass a raw SQL string, table/index name, principal, or
//! parameter value into a label — structurally, not by convention.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct PlannerMetrics {
    plans_built: AtomicU64,
    plan_errors: AtomicU64,
    optimized_plans: AtomicU64,
    seq_scans_selected: AtomicU64,
    index_scans_selected: AtomicU64,
    pk_lookups_selected: AtomicU64,
    predicate_pushdowns: AtomicU64,
    projection_prunes: AtomicU64,
    limit_pushdowns: AtomicU64,
    join_plans: AtomicU64,
    index_nested_loop_joins: AtomicU64,
    sort_nodes: AtomicU64,
    sort_nodes_eliminated: AtomicU64,
    plan_resource_limit_hits: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlannerMetricsSnapshot {
    pub plans_built: u64,
    pub plan_errors: u64,
    pub optimized_plans: u64,
    pub seq_scans_selected: u64,
    pub index_scans_selected: u64,
    pub pk_lookups_selected: u64,
    pub predicate_pushdowns: u64,
    pub projection_prunes: u64,
    pub limit_pushdowns: u64,
    pub join_plans: u64,
    pub index_nested_loop_joins: u64,
    pub sort_nodes: u64,
    pub sort_nodes_eliminated: u64,
    pub plan_resource_limit_hits: u64,
}

impl PlannerMetrics {
    pub fn record_plan_built(&self) {
        self.plans_built.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_plan_error(&self) {
        self.plan_errors.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_optimized_plan(&self) {
        self.optimized_plans.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_seq_scan(&self) {
        self.seq_scans_selected.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_index_scan(&self) {
        self.index_scans_selected.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_pk_lookup(&self) {
        self.pk_lookups_selected.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_predicate_pushdown(&self) {
        self.predicate_pushdowns.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_projection_prune(&self) {
        self.projection_prunes.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_limit_pushdown(&self) {
        self.limit_pushdowns.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_join_plan(&self) {
        self.join_plans.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_index_nested_loop_join(&self) {
        self.index_nested_loop_joins.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_sort_node(&self) {
        self.sort_nodes.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_sort_node_eliminated(&self) {
        self.sort_nodes_eliminated.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_plan_resource_limit_hit(&self) {
        self.plan_resource_limit_hits
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> PlannerMetricsSnapshot {
        PlannerMetricsSnapshot {
            plans_built: self.plans_built.load(Ordering::Relaxed),
            plan_errors: self.plan_errors.load(Ordering::Relaxed),
            optimized_plans: self.optimized_plans.load(Ordering::Relaxed),
            seq_scans_selected: self.seq_scans_selected.load(Ordering::Relaxed),
            index_scans_selected: self.index_scans_selected.load(Ordering::Relaxed),
            pk_lookups_selected: self.pk_lookups_selected.load(Ordering::Relaxed),
            predicate_pushdowns: self.predicate_pushdowns.load(Ordering::Relaxed),
            projection_prunes: self.projection_prunes.load(Ordering::Relaxed),
            limit_pushdowns: self.limit_pushdowns.load(Ordering::Relaxed),
            join_plans: self.join_plans.load(Ordering::Relaxed),
            index_nested_loop_joins: self.index_nested_loop_joins.load(Ordering::Relaxed),
            sort_nodes: self.sort_nodes.load(Ordering::Relaxed),
            sort_nodes_eliminated: self.sort_nodes_eliminated.load(Ordering::Relaxed),
            plan_resource_limit_hits: self.plan_resource_limit_hits.load(Ordering::Relaxed),
        }
    }
}
