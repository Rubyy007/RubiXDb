//! Rule-based logical-plan optimization — D16/item 27: deterministic,
//! isolated, order-aware passes, no cost model, no invented constants.
//!
//! **Fixpoint/termination (item 28)**: each rule runs **exactly once**,
//! in one fixed order (pushdown → pruning → limit-pushdown), never
//! iterated and never re-triggering an earlier rule. This is not "prove
//! convergence of an iterative loop" — there is no loop, by
//! construction, so there is nothing that could cycle (`A → B → A`) or
//! hang on a malformed query. This is the simplest choice that
//! satisfies item 28's requirement and is preferred over an iterative
//! fixpoint absent any evidence one is needed (this project's
//! established "do not build unjustified complexity ahead of evidence"
//! convention, D16's own citation of it).
//!
//! **Every rule is provably result-preserving** (D16's own requirement)
//! by construction, not by per-rule case analysis: no rule here ever
//! rewrites the *logic* of a `BoundExpr` (no algebraic simplification,
//! no `AND`/`OR`/`NOT` rewriting) — every rule only ever decides *where*
//! an unmodified expression subtree is evaluated, or discards
//! optimizer-internal metadata (a computed required-column set) that no
//! executor semantics depend on. Three-valued logic (item 21) is
//! therefore automatically preserved: nothing here can turn `UNKNOWN`
//! into `FALSE` or vice versa, because nothing here evaluates or
//! rewrites a predicate's own truth table at all.

use std::collections::{BTreeSet, HashMap};

use crate::bound::BoundExpr;
use crate::error::{Result, SqlError};
use crate::plan::expr_util::{
    and_all, conjuncts, recombine, referenced_ordinals, referenced_table_refs,
};
use crate::plan::limits::PlannerLimits;
use crate::plan::logical::{node_count, LogicalPlan};
use crate::plan::metrics::PlannerMetrics;

pub fn optimize(
    plan: LogicalPlan,
    limits: &PlannerLimits,
    metrics: &PlannerMetrics,
) -> Result<LogicalPlan> {
    check_node_budget(&plan, limits, metrics)?;
    let plan = pushdown_predicates(plan, limits, metrics)?;
    let plan = prune_projections(plan);
    let plan = mark_pushable_limits(plan, metrics);
    metrics.record_optimized_plan();
    Ok(plan)
}

fn check_node_budget(
    plan: &LogicalPlan,
    limits: &PlannerLimits,
    metrics: &PlannerMetrics,
) -> Result<()> {
    let count = node_count(plan);
    if count > limits.max_plan_nodes {
        metrics.record_plan_resource_limit_hit();
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "logical plan has {count} node(s); planner max_plan_nodes is {}",
                limits.max_plan_nodes
            ),
        });
    }
    Ok(())
}

// =======================================================================
// Predicate pushdown (item 12) — LEFT JOIN-safe (item 20).
// =======================================================================

fn pushdown_predicates(
    plan: LogicalPlan,
    limits: &PlannerLimits,
    metrics: &PlannerMetrics,
) -> Result<LogicalPlan> {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            let input = pushdown_predicates(*input, limits, metrics)?;
            let parts = conjuncts(&predicate);
            if parts.len() > limits.max_predicate_conjuncts {
                metrics.record_plan_resource_limit_hit();
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "predicate decomposes into {} conjunct(s); planner max_predicate_conjuncts is {}",
                        parts.len(),
                        limits.max_predicate_conjuncts
                    ),
                });
            }
            let mut residual = Vec::new();
            let mut input = input;
            for conjunct in parts {
                let refs = referenced_table_refs(&conjunct);
                let pushed = match refs.len() {
                    1 => {
                        let table_ref = *refs.iter().next().expect("len == 1");
                        try_push_into_scan(&mut input, table_ref, &conjunct)
                    }
                    _ => false,
                };
                if pushed {
                    metrics.record_predicate_pushdown();
                } else {
                    residual.push(conjunct);
                }
            }
            Ok(match recombine(residual) {
                Some(predicate) => LogicalPlan::Filter {
                    input: Box::new(input),
                    predicate,
                },
                None => input,
            })
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            on,
        } => Ok(LogicalPlan::Join {
            left: Box::new(pushdown_predicates(*left, limits, metrics)?),
            right: Box::new(pushdown_predicates(*right, limits, metrics)?),
            kind,
            on,
        }),
        LogicalPlan::Projection { input, items } => Ok(LogicalPlan::Projection {
            input: Box::new(pushdown_predicates(*input, limits, metrics)?),
            items,
        }),
        LogicalPlan::Distinct { input } => Ok(LogicalPlan::Distinct {
            input: Box::new(pushdown_predicates(*input, limits, metrics)?),
        }),
        LogicalPlan::Sort { input, items } => Ok(LogicalPlan::Sort {
            input: Box::new(pushdown_predicates(*input, limits, metrics)?),
            items,
        }),
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => Ok(LogicalPlan::Limit {
            input: Box::new(pushdown_predicates(*input, limits, metrics)?),
            limit,
            offset,
        }),
        leaf @ (LogicalPlan::EmptyRelation | LogicalPlan::Scan { .. }) => Ok(leaf),
    }
}

/// Attempts to merge `conjunct` into the `Scan` node for `table_ref`,
/// found by walking the `Join`/`Scan` tree. Refuses (`false`) when that
/// table is `null_extended` (item 20 — the LEFT JOIN correctness gate:
/// a predicate on the nullable side must never move below the join,
/// because doing so would silently convert unmatched rows that should
/// still appear with `NULL`s into rows excluded entirely before the
/// join ever runs).
fn try_push_into_scan(plan: &mut LogicalPlan, table_ref: u32, conjunct: &BoundExpr) -> bool {
    match plan {
        LogicalPlan::Scan {
            table, predicate, ..
        } => {
            if table.id.0 != table_ref || table.null_extended {
                return false;
            }
            *predicate = Some(match predicate.take() {
                Some(existing) => and_all(existing, conjunct.clone()),
                None => conjunct.clone(),
            });
            true
        }
        LogicalPlan::Join { left, right, .. } => {
            try_push_into_scan(left, table_ref, conjunct)
                || try_push_into_scan(right, table_ref, conjunct)
        }
        _ => false,
    }
}

// =======================================================================
// Projection pruning (item 13) — metadata only, see `crate::plan::
// access`'s own doc comment for why this cannot yet reduce physical
// decode cost (`TableStore` has no partial-column-decode primitive).
// =======================================================================

fn prune_projections(plan: LogicalPlan) -> LogicalPlan {
    let mut required: HashMap<u32, BTreeSet<u16>> = HashMap::new();
    collect_required_columns(&plan, &mut required);
    annotate_required_columns(plan, &required)
}

fn collect_required_columns(plan: &LogicalPlan, acc: &mut HashMap<u32, BTreeSet<u16>>) {
    let note = |expr: &BoundExpr, acc: &mut HashMap<u32, BTreeSet<u16>>| {
        for table_ref in referenced_table_refs(expr) {
            acc.entry(table_ref)
                .or_default()
                .extend(referenced_ordinals(expr, table_ref));
        }
    };
    match plan {
        LogicalPlan::EmptyRelation => {}
        LogicalPlan::Scan {
            table, predicate, ..
        } => {
            if let Some(p) = predicate {
                note(p, acc);
            }
            // A predicate pushed onto this scan always needs at least an
            // empty entry present so a table referenced only by its own
            // pushed-down filter still gets a (possibly empty-beyond-
            // filter-columns) annotation rather than being left `None`.
            acc.entry(table.id.0).or_default();
        }
        LogicalPlan::Join {
            left, right, on, ..
        } => {
            note(on, acc);
            collect_required_columns(left, acc);
            collect_required_columns(right, acc);
        }
        LogicalPlan::Filter { input, predicate } => {
            note(predicate, acc);
            collect_required_columns(input, acc);
        }
        LogicalPlan::Projection { input, items } => {
            for item in items {
                note(&item.expr, acc);
            }
            collect_required_columns(input, acc);
        }
        LogicalPlan::Distinct { input } => collect_required_columns(input, acc),
        LogicalPlan::Sort { input, items } => {
            for item in items {
                note(&item.expr, acc);
            }
            collect_required_columns(input, acc);
        }
        LogicalPlan::Limit { input, .. } => collect_required_columns(input, acc),
    }
}

fn annotate_required_columns(
    plan: LogicalPlan,
    required: &HashMap<u32, BTreeSet<u16>>,
) -> LogicalPlan {
    match plan {
        LogicalPlan::Scan {
            table,
            predicate,
            required_columns: _,
        } => {
            let cols: Vec<u16> = required
                .get(&table.id.0)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();
            LogicalPlan::Scan {
                table,
                predicate,
                required_columns: Some(cols),
            }
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            on,
        } => LogicalPlan::Join {
            left: Box::new(annotate_required_columns(*left, required)),
            right: Box::new(annotate_required_columns(*right, required)),
            kind,
            on,
        },
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(annotate_required_columns(*input, required)),
            predicate,
        },
        LogicalPlan::Projection { input, items } => LogicalPlan::Projection {
            input: Box::new(annotate_required_columns(*input, required)),
            items,
        },
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(annotate_required_columns(*input, required)),
        },
        LogicalPlan::Sort { input, items } => LogicalPlan::Sort {
            input: Box::new(annotate_required_columns(*input, required)),
            items,
        },
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => LogicalPlan::Limit {
            input: Box::new(annotate_required_columns(*input, required)),
            limit,
            offset,
        },
        leaf @ LogicalPlan::EmptyRelation => leaf,
    }
}

// =======================================================================
// Limit pushdown (item 14) — conservative by construction. Per item 14's
// own text, only the one explicitly-blessed shape is marked pushable: a
// `Limit` directly above zero-or-more `Filter`/`Projection` wrappers
// directly above a single `Scan` (no `Join`, `Sort`, or `Distinct` in
// between — every one of those is named explicitly in item 14 as a
// default blocker). This does not *move* the `Limit` node (the pull-
// based executor model already stops iterating once satisfied,
// architecture doc §11 — there is no separate physical "scan with
// limit" primitive to move it onto); it only *marks* the existing node
// as provably safe to terminate early on, for `EXPLAIN`/executor use.
// =======================================================================

fn mark_pushable_limits(plan: LogicalPlan, metrics: &PlannerMetrics) -> LogicalPlan {
    match plan {
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            let input = mark_pushable_limits(*input, metrics);
            if limit_pushable(&input) {
                metrics.record_limit_pushdown();
            }
            LogicalPlan::Limit {
                input: Box::new(input),
                limit,
                offset,
            }
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            on,
        } => LogicalPlan::Join {
            left: Box::new(mark_pushable_limits(*left, metrics)),
            right: Box::new(mark_pushable_limits(*right, metrics)),
            kind,
            on,
        },
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(mark_pushable_limits(*input, metrics)),
            predicate,
        },
        LogicalPlan::Projection { input, items } => LogicalPlan::Projection {
            input: Box::new(mark_pushable_limits(*input, metrics)),
            items,
        },
        LogicalPlan::Distinct { input } => LogicalPlan::Distinct {
            input: Box::new(mark_pushable_limits(*input, metrics)),
        },
        LogicalPlan::Sort { input, items } => LogicalPlan::Sort {
            input: Box::new(mark_pushable_limits(*input, metrics)),
            items,
        },
        leaf @ (LogicalPlan::EmptyRelation | LogicalPlan::Scan { .. }) => leaf,
    }
}

/// `true` iff `plan` is a `Scan` reached through zero or more `Filter`/
/// `Projection` wrappers only.
pub fn limit_pushable(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Scan { .. } => true,
        LogicalPlan::Filter { input, .. } | LogicalPlan::Projection { input, .. } => {
            limit_pushable(input)
        }
        _ => false,
    }
}
