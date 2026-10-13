//! The query planner — `PHASE_RELATIONAL_QUERY_PLANNER_ARCHITECTURE.md`.
//! `BoundStatement → LogicalPlan → (rule-based optimization) →
//! PhysicalPlan`, for `SELECT`; a single shared `PhysicalAccess`
//! decision (`crate::plan::access`) for `UPDATE`/`DELETE`'s target row
//! set; a structural pass-through for `INSERT`/DDL/transaction control
//! (item 47 — no execution nodes fabricated for statement kinds this
//! increment does not plan). **No execution happens anywhere in this
//! module** — `Plan` is a data structure a future executor consumes,
//! never run here.

pub mod access;
pub mod explain;
pub mod expr_util;
pub mod limits;
pub mod logical;
pub mod metrics;
pub mod optimize;
pub mod physical;
pub mod relstats;
pub mod validate;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use rubixdb::catalog::CatalogService;

use crate::bound::{
    BoundAssignment, BoundCte, BoundExpr, BoundInsert, BoundRelation, BoundSelect, BoundStatement,
    BoundSubqueryExpr, BoundTableRef,
};
use crate::error::Result;
use access::PhysicalAccess;
pub use explain::explain;
pub use limits::PlannerLimits;
pub use logical::LogicalPlan;
pub use metrics::{PlannerMetrics, PlannerMetricsSnapshot};
pub use physical::{CtePlan, JoinAlgorithm, PhysicalPlan};

/// The top-level planner output for one `BoundStatement`. Every variant
/// documents its own EXECUTABLE/NOT-EXECUTABLE-YET boundary the same
/// way `BoundStatement` itself does (item 5/61: this crate never claims
/// to execute anything).
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// A planned, optimized `SELECT` — the only statement kind this
    /// increment does real logical/physical planning for.
    Query {
        physical: PhysicalPlan,
        max_parameter: u32,
        /// Every `WITH` definition in the statement, referenced or not
        /// (SQL-mission-1 Slice C) -- what `max_cte_definitions` bounds.
        defined_ctes: u32,
    },
    /// `INSERT` has no access-path or ordering decision to make (every
    /// row's `VALUES` are already fully bound); carried through
    /// unchanged for a future executor, per item 47.
    Insert(BoundInsert),
    /// `UPDATE`'s target row set, planned with the exact same
    /// `PhysicalAccess` decision (`crate::plan::access`) a `SELECT`
    /// scan would use for the same predicate — one algorithm, reused,
    /// never duplicated.
    Update {
        table: BoundTableRef,
        assignments: Vec<BoundAssignment>,
        access: PhysicalAccess,
        max_parameter: u32,
        defined_ctes: u32,
    },
    Delete {
        table: BoundTableRef,
        access: PhysicalAccess,
        max_parameter: u32,
        defined_ctes: u32,
    },
    /// `CREATE`/`DROP TABLE`/`INDEX`/`SCHEMA`/`DATABASE` — item 47: "do
    /// not fabricate execution nodes... at most produce the planner-
    /// level representation needed by the future executor," which for
    /// DDL is simply the already-bound statement itself; `CatalogService`
    /// owns DDL's own atomicity (its `ddl_lock`), not this planner.
    Ddl(BoundStatement),
    Begin,
    Commit,
    Rollback,
    Explain(Box<Plan>),
}

/// Builds and validates a `Plan` from an already-bound statement.
/// **Never re-resolves an identifier, never re-runs authorization**
/// (item 24: every relation/column/index in the output plan originates
/// from `bound` itself, verbatim or relocated, never freshly looked up
/// by name).
pub fn build_plan(
    bound: &BoundStatement,
    catalog: &CatalogService,
    limits: &PlannerLimits,
    metrics: &PlannerMetrics,
) -> Result<Plan> {
    let result = build_plan_inner(bound, catalog, limits, metrics);
    match &result {
        Ok(_) => metrics.record_plan_built(),
        Err(_) => metrics.record_plan_error(),
    }
    let plan = result?;
    validate::validate_plan(&plan, catalog)?;
    Ok(plan)
}

/// Everything the planning of one statement threads through its recursion:
/// the handles every step needs plus the CTE definitions planned so far
/// (a definition is planned once and shared by all of its references).
struct Cx<'a> {
    catalog: &'a CatalogService,
    limits: &'a PlannerLimits,
    metrics: &'a PlannerMetrics,
    ctes: RefCell<HashMap<u32, Arc<CtePlan>>>,
}

fn build_plan_inner(
    bound: &BoundStatement,
    catalog: &CatalogService,
    limits: &PlannerLimits,
    metrics: &PlannerMetrics,
) -> Result<Plan> {
    let cx = Cx {
        catalog,
        limits,
        metrics,
        ctes: RefCell::new(HashMap::new()),
    };
    build_plan_cx(bound, &cx)
}

fn build_plan_cx(bound: &BoundStatement, cx: &Cx<'_>) -> Result<Plan> {
    let (catalog, metrics) = (cx.catalog, cx.metrics);
    match bound {
        BoundStatement::Select(select) => {
            let physical = plan_select(select, cx)?;
            Ok(Plan::Query {
                physical,
                max_parameter: select.max_parameter,
                defined_ctes: cx.ctes.borrow().len() as u32,
            })
        }
        BoundStatement::Insert(insert) => Ok(Plan::Insert(insert.clone())),
        BoundStatement::Update(update) => {
            plan_ctes(&update.ctes, cx)?;
            let selection = plan_optional_expr(update.selection.clone(), cx)?;
            let mut assignments = update.assignments.clone();
            for assignment in &mut assignments {
                plan_expr_subqueries(&mut assignment.value, cx)?;
            }
            let access = access::plan_table_access(
                update.table.table_id,
                update.table.id.0,
                selection.as_ref(),
                catalog,
                metrics,
            )?;
            Ok(Plan::Update {
                table: update.table.clone(),
                assignments,
                access,
                max_parameter: update.max_parameter,
                defined_ctes: cx.ctes.borrow().len() as u32,
            })
        }
        BoundStatement::Delete(delete) => {
            plan_ctes(&delete.ctes, cx)?;
            let selection = plan_optional_expr(delete.selection.clone(), cx)?;
            let access = access::plan_table_access(
                delete.table.table_id,
                delete.table.id.0,
                selection.as_ref(),
                catalog,
                metrics,
            )?;
            Ok(Plan::Delete {
                table: delete.table.clone(),
                access,
                max_parameter: delete.max_parameter,
                defined_ctes: cx.ctes.borrow().len() as u32,
            })
        }
        BoundStatement::CreateDatabase(_)
        | BoundStatement::CreateSchema(_)
        | BoundStatement::CreateTable(_)
        | BoundStatement::DropTable(_)
        | BoundStatement::CreateIndex(_)
        | BoundStatement::DropIndex(_) => Ok(Plan::Ddl(bound.clone())),
        BoundStatement::Begin => Ok(Plan::Begin),
        BoundStatement::Commit => Ok(Plan::Commit),
        BoundStatement::Rollback => Ok(Plan::Rollback),
        BoundStatement::Explain(inner) => {
            let inner_plan = build_plan_cx(inner, cx)?;
            Ok(Plan::Explain(Box::new(inner_plan)))
        }
    }
}

// =======================================================================
// Subquery planning (SQL-mission-1)
//
// A subquery expression is planned by the same planner as the statement
// that contains it: its `BoundSelect` goes through logical planning,
// optimization and physical planning, and the resulting `PhysicalPlan` is
// attached to the expression node (`BoundSubqueryExpr::plan`). The
// executor then evaluates the node by running that plan as a nested
// operator tree -- one mechanism for scalar, `EXISTS` and `IN` forms.
// =======================================================================

/// Plans one `SELECT` (the statement itself or a subquery body): first its
/// own CTEs, then a physical plan for every derived table, CTE reference and
/// subquery expression it contains, then the query itself.
fn plan_select(select: &BoundSelect, cx: &Cx<'_>) -> Result<PhysicalPlan> {
    let mut select = select.clone();
    plan_ctes(&select.ctes, cx)?;
    plan_select_subqueries(&mut select, cx)?;
    let logical = logical::build_logical_plan(&select, cx.limits)?;
    let optimized = optimize::optimize(logical, cx.limits, cx.metrics)?;
    physical::build_physical_plan(&optimized, cx.catalog, cx.metrics)
}

/// Plans a `WITH` list in order, once each (a later definition may use an
/// earlier one). A CTE nobody references is still planned (so a bad one is an
/// error) but is never executed.
fn plan_ctes(ctes: &[BoundCte], cx: &Cx<'_>) -> Result<()> {
    for cte in ctes {
        let plan = match &cte.recursion {
            Some(recursion) => plan_recursive_cte(cte, recursion, cx)?,
            None => {
                let plan = plan_select(&cte.query, cx)?;
                validate::validate_subplan(&plan, cte.query.max_parameter, cx.catalog)?;
                plan
            }
        };
        cx.ctes.borrow_mut().insert(
            cte.id,
            Arc::new(CtePlan {
                id: cte.id,
                name: cte.name.clone(),
                plan,
            }),
        );
    }
    Ok(())
}

/// A recursive CTE (SQL-mission-1 Slice D): its seed and its recursive term are
/// planned separately -- each as the ordinary query it is -- and joined in a
/// `RecursiveCte` node that keeps them apart. The shell's left operand is the
/// seed, its right operand the term (whose `WorkingTable` leaf reads the
/// previous iteration).
fn plan_recursive_cte(
    cte: &BoundCte,
    recursion: &crate::bound::BoundRecursion,
    cx: &Cx<'_>,
) -> Result<PhysicalPlan> {
    let set = cte.query.set_op.as_ref().ok_or_else(|| crate::error::SqlError::PlanValidation {
        detail: "a recursive CTE reached planning without its seed and recursive term".to_string(),
    })?;
    let seed = plan_select(&set.left, cx)?;
    validate::validate_subplan(&seed, set.left.max_parameter, cx.catalog)?;
    let term = plan_select(&set.right, cx)?;
    validate::validate_subplan(&term, set.right.max_parameter, cx.catalog)?;
    Ok(PhysicalPlan::RecursiveCte(physical::RecursiveCteNode {
        name: cte.name.clone(),
        cte_id: cte.id,
        work_id: recursion.work_id,
        distinct: !recursion.all,
        seed_plan: Box::new(seed),
        recursive_plan: Box::new(term),
    }))
}

fn plan_one_subquery(sq: &mut BoundSubqueryExpr, cx: &Cx<'_>) -> Result<()> {
    let physical = plan_select(&sq.query, cx)?;
    validate::validate_subplan(&physical, sq.query.max_parameter, cx.catalog)?;
    sq.plan = Some(physical);
    Ok(())
}

fn plan_expr_subqueries(expr: &mut BoundExpr, cx: &Cx<'_>) -> Result<()> {
    expr_util::for_each_subquery_mut(expr, &mut |sq| plan_one_subquery(sq, cx))
}

fn plan_optional_expr(expr: Option<BoundExpr>, cx: &Cx<'_>) -> Result<Option<BoundExpr>> {
    match expr {
        None => Ok(None),
        Some(mut e) => {
            plan_expr_subqueries(&mut e, cx)?;
            Ok(Some(e))
        }
    }
}

fn plan_select_subqueries(select: &mut BoundSelect, cx: &Cx<'_>) -> Result<()> {
    // SQL-mission-1 Slice E: the operands of a set operation are whole
    // queries; each is planned (and validated) on its own, like a derived
    // table's body, and the operation's node carries the results.
    if let Some(set) = &mut select.set_op {
        let left = plan_select(&set.left, cx)?;
        validate::validate_subplan(&left, set.left.max_parameter, cx.catalog)?;
        set.left_plan = Some(left);
        let right = plan_select(&set.right, cx)?;
        validate::validate_subplan(&right, set.right.max_parameter, cx.catalog)?;
        set.right_plan = Some(right);
    }
    for item in &mut select.from {
        match &mut item.relation {
            BoundRelation::Table | BoundRelation::Working(_) => {}
            BoundRelation::Derived(d) => {
                let plan = plan_select(&d.query, cx)?;
                validate::validate_subplan(&plan, d.query.max_parameter, cx.catalog)?;
                d.plan = Some(plan);
            }
            BoundRelation::Cte(r) => {
                let planned = cx.ctes.borrow().get(&r.cte_id).cloned();
                r.plan = Some(planned.ok_or_else(|| crate::error::SqlError::PlanValidation {
                    detail: "a CTE reference has no planned definition".to_string(),
                })?);
            }
        }
        if let Some((_, on)) = &mut item.join {
            plan_expr_subqueries(on, cx)?;
        }
    }
    for item in &mut select.projection {
        plan_expr_subqueries(&mut item.expr, cx)?;
    }
    if let Some(e) = &mut select.selection {
        plan_expr_subqueries(e, cx)?;
    }
    for e in &mut select.group_by {
        plan_expr_subqueries(e, cx)?;
    }
    for agg in &mut select.aggregates {
        if let crate::aggregate::AggregateArg::Expr(e) = &mut agg.arg {
            plan_expr_subqueries(e, cx)?;
        }
        if let Some(f) = &mut agg.filter {
            plan_expr_subqueries(f, cx)?;
        }
    }
    if let Some(e) = &mut select.having {
        plan_expr_subqueries(e, cx)?;
    }
    if let Some(e) = &mut select.qualify {
        plan_expr_subqueries(e, cx)?;
    }
    for item in &mut select.order_by {
        plan_expr_subqueries(&mut item.expr, cx)?;
    }
    if let Some(e) = &mut select.limit {
        plan_expr_subqueries(e, cx)?;
    }
    if let Some(e) = &mut select.offset {
        plan_expr_subqueries(e, cx)?;
    }
    Ok(())
}
