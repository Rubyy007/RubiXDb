//! Planner resource bounds — item 26. Distinct from `crate::limits::
//! SqlLimits` (the parser/binder boundary's own limits, already applied
//! before a `BoundStatement` exists) because the planner has its own
//! pathological-growth surfaces a well-formed-but-adversarial bound
//! statement can still hit: many joins, many predicate conjuncts, many
//! `OR` branches, deep expression trees the binder allowed through
//! (`SqlLimits::max_expression_depth`) but that an optimization pass
//! could still visit more than once if written carelessly. Every limit
//! here is checked *before* the corresponding planner work, never after
//! (item 26's own "do not create exponential rewrite explosions").

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerLimits {
    /// Most `FROM`/`JOIN` items one statement's logical plan may chain.
    pub max_joins: usize,
    /// Most top-level `AND`-conjuncts one `WHERE`/`ON`/predicate
    /// expression may decompose into during pushdown analysis.
    pub max_predicate_conjuncts: usize,
    /// Most plan-tree nodes `build_logical_plan`/`optimize`/
    /// `build_physical_plan` may construct for one statement — a
    /// blanket backstop independent of the more specific limits above,
    /// covering any node-growth shape not individually named.
    pub max_plan_nodes: usize,
}

impl Default for PlannerLimits {
    fn default() -> Self {
        PlannerLimits {
            // Matches `SqlLimits::default().max_columns`'s own order of
            // magnitude reasoning (D27-class round bound) — no join count
            // this large is realistic, but the bound exists to make
            // pathological input fail fast and cleanly rather than grow
            // unboundedly.
            max_joins: 64,
            max_predicate_conjuncts: 1_000,
            max_plan_nodes: 10_000,
        }
    }
}
