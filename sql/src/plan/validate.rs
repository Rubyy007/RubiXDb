//! Structural plan validation (item 29) — a defensive self-check on
//! this crate's *own* output, run once after `build_physical_plan`.
//! Every property checked here is one `crate::plan`'s own
//! transformations must already guarantee by construction (this crate
//! never invents a table/column/index reference — it only relocates or
//! discards already-bound ones); the checks exist as a should-never-
//! fire safety net against a bug in a rule's own implementation, not
//! because the binder's guarantees are in doubt. **Structural only**
//! (item 29's own text) — this never re-resolves an identifier through
//! the catalog by name, never re-runs authorization (D25 stays entirely
//! at the binder), and never re-infers a type.

use std::collections::BTreeSet;

use rubixdb::catalog::CatalogService;

use crate::bound::{BoundExpr, BoundExprKind};
use crate::error::{Result, SqlError};
use crate::plan::access::{IndexAccessMode, PhysicalAccess};
use crate::plan::physical::PhysicalPlan;
use crate::plan::Plan;

pub fn validate_plan(plan: &Plan, catalog: &CatalogService) -> Result<()> {
    match plan {
        Plan::Query {
            physical,
            max_parameter,
        } => {
            validate_physical(physical, catalog)?;
            let mut seen_params = BTreeSet::new();
            collect_parameters_physical(physical, &mut seen_params);
            check_parameter_bound(&seen_params, *max_parameter)
        }
        Plan::Update {
            access,
            max_parameter,
            ..
        }
        | Plan::Delete {
            access,
            max_parameter,
            ..
        } => {
            validate_access(access, catalog)?;
            let mut seen_params = BTreeSet::new();
            collect_parameters_expr_opt(access_predicate(access), &mut seen_params);
            check_parameter_bound(&seen_params, *max_parameter)
        }
        Plan::Insert(_) | Plan::Ddl(_) | Plan::Begin | Plan::Commit | Plan::Rollback => Ok(()),
        Plan::Explain(inner) => validate_plan(inner, catalog),
    }
}

fn access_predicate(access: &PhysicalAccess) -> Option<&BoundExpr> {
    match access {
        PhysicalAccess::PkLookup { residual, .. } => residual.as_ref(),
        PhysicalAccess::IndexScan { residual, .. } => residual.as_ref(),
        PhysicalAccess::SeqScan { predicate, .. } => predicate.as_ref(),
    }
}

fn check_parameter_bound(seen: &BTreeSet<u32>, max_parameter: u32) -> Result<()> {
    if let Some(&highest) = seen.iter().next_back() {
        if highest > max_parameter {
            return Err(SqlError::PlanValidation {
                detail:
                    "plan references a parameter index beyond the statement's own declared maximum"
                        .to_string(),
            });
        }
    }
    Ok(())
}

fn validate_physical(plan: &PhysicalPlan, catalog: &CatalogService) -> Result<()> {
    match plan {
        PhysicalPlan::EmptyRelation => Ok(()),
        PhysicalPlan::Access(access) => validate_access(access, catalog),
        PhysicalPlan::Join { left, right, .. } => {
            validate_physical(left, catalog)?;
            validate_physical(right, catalog)
        }
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Projection { input, .. }
        | PhysicalPlan::Distinct { input }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Limit { input, .. } => validate_physical(input, catalog),
    }
}

/// Item 29's two most concrete, real checks: the accessed table must
/// still exist, and — for an `IndexScan` — the chosen index must
/// actually belong to that same table (never a stale or mismatched
/// `index_id`).
fn validate_access(access: &PhysicalAccess, catalog: &CatalogService) -> Result<()> {
    let table_id = access.table_id();
    if catalog.get_table(table_id)?.is_none() {
        return Err(SqlError::PlanValidation {
            detail: "plan references a table that no longer resolves in the catalog".to_string(),
        });
    }
    if let PhysicalAccess::IndexScan { index_id, .. } = access {
        let index = catalog
            .get_index(*index_id)?
            .ok_or_else(|| SqlError::PlanValidation {
                detail: "plan references an index that no longer resolves in the catalog"
                    .to_string(),
            })?;
        if index.table_id != table_id {
            return Err(SqlError::PlanValidation {
                detail: "plan selected an index that does not belong to the scanned table"
                    .to_string(),
            });
        }
    }
    Ok(())
}

fn collect_parameters_physical(plan: &PhysicalPlan, out: &mut BTreeSet<u32>) {
    match plan {
        PhysicalPlan::EmptyRelation => {}
        PhysicalPlan::Access(access) => collect_parameters_access(access, out),
        PhysicalPlan::Join {
            left, right, on, ..
        } => {
            collect_parameters_expr(on, out);
            collect_parameters_physical(left, out);
            collect_parameters_physical(right, out);
        }
        PhysicalPlan::Filter { input, predicate } => {
            collect_parameters_expr(predicate, out);
            collect_parameters_physical(input, out);
        }
        PhysicalPlan::Projection { input, items } => {
            for item in items {
                collect_parameters_expr(&item.expr, out);
            }
            collect_parameters_physical(input, out);
        }
        PhysicalPlan::Distinct { input } => collect_parameters_physical(input, out),
        PhysicalPlan::Sort { input, items } => {
            for item in items {
                collect_parameters_expr(&item.expr, out);
            }
            collect_parameters_physical(input, out);
        }
        PhysicalPlan::Limit {
            input,
            limit,
            offset,
            ..
        } => {
            collect_parameters_expr_opt(limit.as_ref(), out);
            collect_parameters_expr_opt(offset.as_ref(), out);
            collect_parameters_physical(input, out);
        }
    }
}

fn collect_parameters_access(access: &PhysicalAccess, out: &mut BTreeSet<u32>) {
    match access {
        PhysicalAccess::PkLookup {
            key_values,
            residual,
            ..
        } => {
            for v in key_values {
                collect_parameters_expr(v, out);
            }
            collect_parameters_expr_opt(residual.as_ref(), out);
        }
        PhysicalAccess::IndexScan { mode, residual, .. } => {
            match mode {
                IndexAccessMode::Equality { prefix } => {
                    for v in prefix {
                        collect_parameters_expr(v, out);
                    }
                }
                IndexAccessMode::Range { start, end } => {
                    for bound in [start, end] {
                        if let std::ops::Bound::Included(v) | std::ops::Bound::Excluded(v) = bound {
                            for e in v {
                                collect_parameters_expr(e, out);
                            }
                        }
                    }
                }
            }
            collect_parameters_expr_opt(residual.as_ref(), out);
        }
        PhysicalAccess::SeqScan { predicate, .. } => {
            collect_parameters_expr_opt(predicate.as_ref(), out)
        }
    }
}

fn collect_parameters_expr_opt(expr: Option<&BoundExpr>, out: &mut BTreeSet<u32>) {
    if let Some(e) = expr {
        collect_parameters_expr(e, out);
    }
}

fn collect_parameters_expr(expr: &BoundExpr, out: &mut BTreeSet<u32>) {
    match &expr.kind {
        BoundExprKind::Literal(_) | BoundExprKind::Column(_) => {}
        BoundExprKind::Parameter { index } => {
            out.insert(*index);
        }
        BoundExprKind::UnaryOp { expr, .. } => collect_parameters_expr(expr, out),
        BoundExprKind::BinaryOp { left, right, .. } => {
            collect_parameters_expr(left, out);
            collect_parameters_expr(right, out);
        }
        BoundExprKind::IsNull { expr, .. } => collect_parameters_expr(expr, out),
        BoundExprKind::Between {
            expr, low, high, ..
        } => {
            collect_parameters_expr(expr, out);
            collect_parameters_expr(low, out);
            collect_parameters_expr(high, out);
        }
        BoundExprKind::InList { expr, list, .. } => {
            collect_parameters_expr(expr, out);
            for item in list {
                collect_parameters_expr(item, out);
            }
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            collect_parameters_expr(expr, out);
            collect_parameters_expr(pattern, out);
        }
        BoundExprKind::Case {
            operand,
            branches,
            else_result,
        } => {
            if let Some(operand) = operand {
                collect_parameters_expr(operand, out);
            }
            for (when, then) in branches {
                collect_parameters_expr(when, out);
                collect_parameters_expr(then, out);
            }
            if let Some(else_result) = else_result {
                collect_parameters_expr(else_result, out);
            }
        }
        BoundExprKind::Function { args, .. } => {
            for arg in args {
                collect_parameters_expr(arg, out);
            }
        }
    }
}
