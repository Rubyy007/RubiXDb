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
pub mod validate;

use rubixdb::catalog::CatalogService;

use crate::bound::{BoundAssignment, BoundInsert, BoundStatement, BoundTableRef};
use crate::error::Result;
use access::PhysicalAccess;
pub use explain::explain;
pub use limits::PlannerLimits;
pub use logical::LogicalPlan;
pub use metrics::{PlannerMetrics, PlannerMetricsSnapshot};
pub use physical::{JoinAlgorithm, PhysicalPlan};

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
    },
    Delete {
        table: BoundTableRef,
        access: PhysicalAccess,
        max_parameter: u32,
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

fn build_plan_inner(
    bound: &BoundStatement,
    catalog: &CatalogService,
    limits: &PlannerLimits,
    metrics: &PlannerMetrics,
) -> Result<Plan> {
    match bound {
        BoundStatement::Select(select) => {
            let logical = logical::build_logical_plan(select, limits)?;
            let optimized = optimize::optimize(logical, limits, metrics)?;
            let physical = physical::build_physical_plan(&optimized, catalog, metrics)?;
            Ok(Plan::Query {
                physical,
                max_parameter: select.max_parameter,
            })
        }
        BoundStatement::Insert(insert) => Ok(Plan::Insert(insert.clone())),
        BoundStatement::Update(update) => {
            let access = access::plan_table_access(
                update.table.table_id,
                update.table.id.0,
                update.selection.as_ref(),
                catalog,
                metrics,
            )?;
            Ok(Plan::Update {
                table: update.table.clone(),
                assignments: update.assignments.clone(),
                access,
                max_parameter: update.max_parameter,
            })
        }
        BoundStatement::Delete(delete) => {
            let access = access::plan_table_access(
                delete.table.table_id,
                delete.table.id.0,
                delete.selection.as_ref(),
                catalog,
                metrics,
            )?;
            Ok(Plan::Delete {
                table: delete.table.clone(),
                access,
                max_parameter: delete.max_parameter,
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
            let inner_plan = build_plan_inner(inner, catalog, limits, metrics)?;
            Ok(Plan::Explain(Box::new(inner_plan)))
        }
    }
}
