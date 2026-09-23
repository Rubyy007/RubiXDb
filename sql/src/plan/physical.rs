//! The physical plan — chosen access operators, built only after
//! logical planning (item 5). Converts each optimized-logical `Scan`
//! into a real `PhysicalAccess` (`crate::plan::access`), chooses a join
//! algorithm (`NestedLoop`/`IndexNestedLoop`, D18), and decides whether
//! an already-physical access path's own natural ordering makes an
//! `ORDER BY`'s `Sort` node unnecessary (item 15) — conservatively,
//! never by "approximate reasoning."

use std::collections::BTreeSet;

use rubixdb::catalog::CatalogService;

use crate::ast::JoinKind;
use crate::bound::{BoundExpr, BoundExprKind, BoundOrderByItem, BoundSelectItem, NullsOrder};
use crate::error::Result;
use crate::plan::access::{plan_table_access, IndexAccessMode, PhysicalAccess};
use crate::plan::expr_util::{and_all, referenced_table_refs};
use crate::plan::logical::LogicalPlan;
use crate::plan::metrics::PlannerMetrics;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinAlgorithm {
    /// D18's correct baseline for any join shape.
    NestedLoop,
    /// D18: the join predicate maps directly to a usable index (or the
    /// primary key) on the inner relation — a mechanical substitution,
    /// never a cost decision (item 19).
    IndexNestedLoop,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PhysicalPlan {
    EmptyRelation,
    Access(PhysicalAccess),
    Join {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        kind: JoinKind,
        /// The full, original `ON` condition — always evaluated in full
        /// by a future executor regardless of what `right`'s own access
        /// path already consumed as a candidate-narrowing key (item 10's
        /// "an index is an access path, not proof a predicate is fully
        /// satisfied," applied to join keys too).
        on: BoundExpr,
        algorithm: JoinAlgorithm,
    },
    Filter {
        input: Box<PhysicalPlan>,
        predicate: BoundExpr,
    },
    Projection {
        input: Box<PhysicalPlan>,
        items: Vec<BoundSelectItem>,
    },
    Distinct {
        input: Box<PhysicalPlan>,
    },
    /// Retained whenever the input's own access path does not
    /// provably already produce this order (item 15). Never removed by
    /// approximate reasoning — only the specific, checked cases in
    /// `order_satisfied_by_access` eliminate it.
    Sort {
        input: Box<PhysicalPlan>,
        items: Vec<BoundOrderByItem>,
    },
    Limit {
        input: Box<PhysicalPlan>,
        limit: Option<BoundExpr>,
        offset: Option<BoundExpr>,
        /// Set by the optimizer's limit-pushdown rule (item 14) — a
        /// future executor may terminate its pull loop as soon as
        /// `limit` rows are produced, without waiting for `input` to
        /// exhaust, exactly because nothing between here and the base
        /// access path needs to see the whole input first.
        pushable: bool,
    },
}

pub fn build_physical_plan(
    plan: &LogicalPlan,
    catalog: &CatalogService,
    metrics: &PlannerMetrics,
) -> Result<PhysicalPlan> {
    match plan {
        LogicalPlan::EmptyRelation => Ok(PhysicalPlan::EmptyRelation),
        LogicalPlan::Scan {
            table, predicate, ..
        } => {
            let access = plan_table_access(
                table.table_id,
                table.id.0,
                predicate.as_ref(),
                catalog,
                metrics,
            )?;
            Ok(PhysicalPlan::Access(access))
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            on,
        } => {
            let left_phys = build_physical_plan(left, catalog, metrics)?;
            let left_refs = collect_table_refs(left);

            // Item 19: if `right` is a bare scan and the join predicate
            // gives it a usable, safely-constructible correlated key
            // against the outer (`left`) side, plan its access using the
            // ON condition's own equality conjuncts alongside its own
            // local predicate; the resulting access path's own key
            // expressions naturally reveal whether this happened (a key
            // referencing a `left`-side `TableRefId` is what makes this
            // Index Nested Loop, not a separate mechanism).
            let right_phys = match right.as_ref() {
                LogicalPlan::Scan {
                    table, predicate, ..
                } => {
                    let mut combined = predicate.clone();
                    let on_conjuncts_for_right = crate::plan::expr_util::conjuncts(on)
                        .into_iter()
                        .filter(|c| referenced_table_refs(c).contains(&table.id.0));
                    for c in on_conjuncts_for_right {
                        combined = Some(match combined {
                            Some(existing) => and_all(existing, c),
                            None => c,
                        });
                    }
                    let access = plan_table_access(
                        table.table_id,
                        table.id.0,
                        combined.as_ref(),
                        catalog,
                        metrics,
                    )?;
                    PhysicalPlan::Access(access)
                }
                other => build_physical_plan(other, catalog, metrics)?,
            };

            let algorithm = if is_correlated_to(&right_phys, &left_refs) {
                metrics.record_index_nested_loop_join();
                JoinAlgorithm::IndexNestedLoop
            } else {
                JoinAlgorithm::NestedLoop
            };
            metrics.record_join_plan();

            Ok(PhysicalPlan::Join {
                left: Box::new(left_phys),
                right: Box::new(right_phys),
                kind: *kind,
                on: on.clone(),
                algorithm,
            })
        }
        LogicalPlan::Filter { input, predicate } => Ok(PhysicalPlan::Filter {
            input: Box::new(build_physical_plan(input, catalog, metrics)?),
            predicate: predicate.clone(),
        }),
        LogicalPlan::Projection { input, items } => Ok(PhysicalPlan::Projection {
            input: Box::new(build_physical_plan(input, catalog, metrics)?),
            items: items.clone(),
        }),
        LogicalPlan::Distinct { input } => Ok(PhysicalPlan::Distinct {
            input: Box::new(build_physical_plan(input, catalog, metrics)?),
        }),
        LogicalPlan::Sort { input, items } => {
            metrics.record_sort_node();

            // Item 15's ORDER BY-driven case: no `WHERE` predicate chose
            // an index, but an unconstrained ascending scan of a Ready
            // index whose leading columns exactly match `items` would
            // still make `Sort` unnecessary — worth an unbounded
            // `IndexScan` purely for the ordering it provides "for
            // free" over `SeqScan`'s no ordering guarantee at all.
            // Deliberately narrow (item 15: never by approximate
            // reasoning): only when `input` is a bare `Projection`
            // directly over an un-filtered `Scan` — a `Filter`/`Join`
            // in between is not analyzed here.
            if let LogicalPlan::Projection {
                input: scan,
                items: proj_items,
            } = input.as_ref()
            {
                if let LogicalPlan::Scan {
                    table,
                    predicate: None,
                    ..
                } = scan.as_ref()
                {
                    if let Some(access) = order_only_index_access(table, items, catalog)? {
                        metrics.record_index_scan();
                        metrics.record_sort_node_eliminated();
                        return Ok(PhysicalPlan::Projection {
                            input: Box::new(PhysicalPlan::Access(access)),
                            items: proj_items.clone(),
                        });
                    }
                }
            }

            let input_phys = build_physical_plan(input, catalog, metrics)?;
            if let Some(access) = single_table_access(&input_phys) {
                if order_satisfied_by_access(items, access, catalog)? {
                    metrics.record_sort_node_eliminated();
                    return Ok(input_phys);
                }
            }
            Ok(PhysicalPlan::Sort {
                input: Box::new(input_phys),
                items: items.clone(),
            })
        }
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            let pushable = crate::plan::optimize::limit_pushable(input);
            Ok(PhysicalPlan::Limit {
                input: Box::new(build_physical_plan(input, catalog, metrics)?),
                limit: limit.clone(),
                offset: offset.clone(),
                pushable,
            })
        }
    }
}

fn collect_table_refs(plan: &LogicalPlan) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    fn walk(plan: &LogicalPlan, out: &mut BTreeSet<u32>) {
        match plan {
            LogicalPlan::EmptyRelation => {}
            LogicalPlan::Scan { table, .. } => {
                out.insert(table.id.0);
            }
            LogicalPlan::Join { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Projection { input, .. }
            | LogicalPlan::Distinct { input }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Limit { input, .. } => walk(input, out),
        }
    }
    walk(plan, &mut out);
    out
}

fn is_correlated_to(plan: &PhysicalPlan, outer_refs: &BTreeSet<u32>) -> bool {
    let keys: Vec<&BoundExpr> = match plan {
        PhysicalPlan::Access(PhysicalAccess::PkLookup { key_values, .. }) => {
            key_values.iter().collect()
        }
        PhysicalPlan::Access(PhysicalAccess::IndexScan { mode, .. }) => match mode {
            IndexAccessMode::Equality { prefix } => prefix.iter().collect(),
            IndexAccessMode::Range { start, end } => {
                let mut v = Vec::new();
                if let std::ops::Bound::Included(b) | std::ops::Bound::Excluded(b) = start {
                    v.extend(b.iter());
                }
                if let std::ops::Bound::Included(b) | std::ops::Bound::Excluded(b) = end {
                    v.extend(b.iter());
                }
                v
            }
        },
        _ => return false,
    };
    keys.iter().any(|expr| {
        referenced_table_refs(expr)
            .iter()
            .any(|r| outer_refs.contains(r))
    })
}

/// Unwraps a physical-plan chain of `Filter`/`Projection` wrappers to
/// the single `PhysicalAccess` underneath, or `None` if the chain
/// contains a `Join`/`Distinct` (composite/dedup ordering is not
/// analyzed — item 16's own "do not optimize DISTINCT away" caution
/// extends here to not *assuming* an ordering through it either).
fn single_table_access(plan: &PhysicalPlan) -> Option<&PhysicalAccess> {
    match plan {
        PhysicalPlan::Access(access) => Some(access),
        PhysicalPlan::Filter { input, .. } | PhysicalPlan::Projection { input, .. } => {
            single_table_access(input)
        }
        _ => None,
    }
}

/// Item 15's `ORDER BY`-only case (no `WHERE` predicate involved): an
/// unbounded, ascending scan of a `Ready`, non-`Primary` index whose
/// full `column_ordinals` list exactly equals `items`' ordinals (same
/// exact-match, ascending, `NULLS FIRST` requirement as `order_
/// satisfied_by_access`) is a real, storage-backed `IndexBuilder::
/// index_range_scan` call with both bounds `Unbounded` — never invented
/// (item 5).
fn order_only_index_access(
    table: &crate::bound::BoundTableRef,
    items: &[BoundOrderByItem],
    catalog: &CatalogService,
) -> Result<Option<PhysicalAccess>> {
    if items.is_empty() {
        return Ok(None);
    }
    for item in items {
        if item.descending || item.nulls != NullsOrder::First {
            return Ok(None);
        }
    }
    let ordinals: Vec<u16> = items
        .iter()
        .map(|item| match &item.expr.kind {
            BoundExprKind::Column(c) => Ok(c.ordinal),
            _ => Ok(u16::MAX), // never matches a real index; falls through below
        })
        .collect::<Result<_>>()?;

    let indexes = catalog.list_indexes(table.table_id)?;
    for index in &indexes {
        if index.state != rubixdb::catalog::schema::IndexState::Ready
            || index.kind == rubixdb::catalog::schema::IndexKind::Primary
        {
            continue;
        }
        if index.column_ordinals == ordinals {
            return Ok(Some(PhysicalAccess::IndexScan {
                table_id: table.table_id,
                table_ref: table.id.0,
                index_id: index.index_id,
                index_name: index.name.clone(),
                mode: IndexAccessMode::Range {
                    start: std::ops::Bound::Unbounded,
                    end: std::ops::Bound::Unbounded,
                },
                residual: None,
            }));
        }
    }
    Ok(None)
}

/// Item 15: conservative, exact-match-only ordering analysis. `PkLookup`
/// returns at most one row, so any requested order is trivially already
/// satisfied. An `IndexScan` satisfies `ORDER BY` only when every item
/// is a plain ascending column reference, `NULLS FIRST` (the *only*
/// ordering the engine can physically produce — no reverse-scan
/// primitive exists anywhere in `LsmEngine`, confirmed by inspection,
/// not assumed), and the item list, in order, exactly equals the
/// index's own full declared `column_ordinals` list. A `SeqScan` never
/// satisfies an `ORDER BY` (no ordering guarantee at all).
fn order_satisfied_by_access(
    items: &[BoundOrderByItem],
    access: &PhysicalAccess,
    catalog: &CatalogService,
) -> Result<bool> {
    match access {
        PhysicalAccess::PkLookup { .. } => Ok(true),
        PhysicalAccess::IndexScan { index_id, .. } => {
            let Some(index) = catalog.get_index(*index_id)? else {
                return Ok(false);
            };
            if items.len() != index.column_ordinals.len() {
                return Ok(false);
            }
            for (item, &ord) in items.iter().zip(index.column_ordinals.iter()) {
                if item.descending || item.nulls != NullsOrder::First {
                    return Ok(false);
                }
                match &item.expr.kind {
                    BoundExprKind::Column(c) if c.ordinal == ord => {}
                    _ => return Ok(false),
                }
            }
            Ok(true)
        }
        PhysicalAccess::SeqScan { .. } => Ok(false),
    }
}
