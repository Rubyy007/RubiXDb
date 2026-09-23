//! Physical table-access planning — items 6–11, 40, 41: the one
//! algorithm that decides `PkLookup` vs. `IndexScan` vs. `SeqScan` for a
//! single table given a (possibly absent) predicate. Shared, verbatim,
//! by `crate::plan::physical`'s `SELECT` scan planning and `crate::
//! plan::mod`'s `UPDATE`/`DELETE` target-row planning — one
//! implementation, never two independently-maintained copies with a
//! chance to disagree (this project's own established convention,
//! `rubixdb::relational::table_store`'s shared `pub(crate)` helpers
//! being the storage-layer precedent for the same reasoning).
//!
//! **Every variant maps to a real, already-implemented storage
//! primitive** (item 5's "do not produce a physical operator that has
//! no actual storage primitive behind it"): `PkLookup` →
//! `TableStore::get_row`/`Transaction::get_row`; `IndexScan`'s
//! `Equality` mode → `IndexBuilder::index_lookup`; its `Range` mode →
//! `IndexBuilder::index_range_scan`; `SeqScan` →
//! `TableStore::scan_table`.
//!
//! **`IndexKind::Primary` is never selected as an `IndexScan`** — a
//! real, inspected (not guessed) storage fact, not an arbitrary rule:
//! `CatalogService::create_table` auto-creates a `Primary`-kind
//! `IndexRow` purely as a catalog/naming record (`{name}_pkey`), but
//! `TableStore`'s own `maintained_indexes` helper explicitly filters
//! `IndexKind::Primary` out of every row's index-maintenance set (`src/
//! relational/table_store.rs`, `i.kind != IndexKind::Primary`) — no
//! physical entries are ever written for it. Selecting it as an
//! `IndexScan` would silently return zero rows for every lookup. `PK`
//! access is always the dedicated `PkLookup` variant instead.

use std::ops::Bound;

use rubixdb::catalog::schema::IndexKind;
use rubixdb::catalog::CatalogService;

use crate::bound::BoundExpr;
use crate::error::{Result, SqlError};
use crate::plan::expr_util::{as_column_comparison, as_column_equality, conjuncts, recombine};
use crate::plan::metrics::PlannerMetrics;

#[derive(Debug, Clone, PartialEq)]
pub enum IndexAccessMode {
    /// `IndexBuilder::index_lookup` — an exact prefix-equality match on
    /// `prefix.len()` leading columns (item 8's "prefix equality").
    Equality { prefix: Vec<BoundExpr> },
    /// `IndexBuilder::index_range_scan` — inclusive/exclusive/unbounded
    /// bounds over the index's leading columns (item 9). `start`/`end`
    /// vectors may be shorter than the index's full column count (a
    /// prefix bound, per `IndexBuilder::index_range_scan`'s own
    /// contract: "trailing columns are unconstrained within that
    /// bound").
    Range {
        start: Bound<Vec<BoundExpr>>,
        end: Bound<Vec<BoundExpr>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PhysicalAccess {
    PkLookup {
        table_id: u32,
        /// One `BoundExpr` per `pk_ordinals` position, in that order —
        /// never evaluated by the planner (item 23: a `Parameter`
        /// reference here is preserved exactly, not resolved).
        key_values: Vec<BoundExpr>,
        /// Every remaining conjunct not consumed by the lookup key
        /// itself — never dropped (item 10: "the index is an access
        /// path, not automatically proof the predicate is fully
        /// satisfied").
        residual: Option<BoundExpr>,
    },
    IndexScan {
        table_id: u32,
        index_id: u32,
        /// For `EXPLAIN`/diagnostics only — never re-resolved through
        /// it (item 24: the plan already carries the authoritative
        /// `index_id`).
        index_name: String,
        mode: IndexAccessMode,
        residual: Option<BoundExpr>,
    },
    SeqScan {
        table_id: u32,
        predicate: Option<BoundExpr>,
    },
}

impl PhysicalAccess {
    pub fn table_id(&self) -> u32 {
        match self {
            PhysicalAccess::PkLookup { table_id, .. }
            | PhysicalAccess::IndexScan { table_id, .. }
            | PhysicalAccess::SeqScan { table_id, .. } => *table_id,
        }
    }
}

/// Chooses the physical access path for one table given the predicate
/// conjuncts already known to be safe to evaluate directly against it
/// (already filtered for LEFT JOIN safety by the caller — `crate::
/// plan::optimize`'s pushdown rule for a `SELECT` scan, or the target
/// table's own `WHERE` for `UPDATE`/`DELETE`, which has no join-nullable
/// side to worry about since D18 does not support joins in `UPDATE`/
/// `DELETE`).
pub fn plan_table_access(
    table_id: u32,
    table_ref: u32,
    predicate: Option<&BoundExpr>,
    catalog: &CatalogService,
    metrics: &PlannerMetrics,
) -> Result<PhysicalAccess> {
    let Some(predicate) = predicate else {
        metrics.record_seq_scan();
        return Ok(PhysicalAccess::SeqScan {
            table_id,
            predicate: None,
        });
    };

    let table = catalog
        .get_table(table_id)?
        .ok_or_else(|| SqlError::PlanValidation {
            detail: "planned access references a table not present in the catalog".to_string(),
        })?;

    let parts = conjuncts(predicate);
    let mut equality_by_ordinal: std::collections::HashMap<u16, (usize, &BoundExpr)> =
        std::collections::HashMap::new();
    let mut comparisons: Vec<(u16, crate::ast::BinaryOp, &BoundExpr, usize)> = Vec::new();
    for (idx, conjunct) in parts.iter().enumerate() {
        if let Some((ordinal, other)) = as_column_equality(conjunct, table_ref) {
            equality_by_ordinal.entry(ordinal).or_insert((idx, other));
        } else if let Some((ordinal, op, other)) = as_column_comparison(conjunct, table_ref) {
            comparisons.push((ordinal, op, other, idx));
        }
    }

    // --- item 6/7: PRIMARY KEY lookup, only when every PK ordinal has
    // its own equality conjunct (never a partial-PK point lookup). ---
    if !table.pk_ordinals.is_empty()
        && table
            .pk_ordinals
            .iter()
            .all(|ord| equality_by_ordinal.contains_key(ord))
    {
        let mut consumed = vec![false; parts.len()];
        let mut key_values = Vec::with_capacity(table.pk_ordinals.len());
        for ord in &table.pk_ordinals {
            let (idx, expr) = equality_by_ordinal[ord];
            consumed[idx] = true;
            key_values.push(expr.clone());
        }
        let residual = residual_of(&parts, &consumed);
        metrics.record_pk_lookup();
        return Ok(PhysicalAccess::PkLookup {
            table_id,
            key_values,
            residual,
        });
    }

    // --- item 8/9: secondary index selection, leading-column match,
    // `Ready` state only, never `Primary` (see module doc comment). ---
    let indexes = catalog.list_indexes(table_id)?;
    let mut best: Option<(usize, PhysicalAccess, Vec<bool>)> = None;
    for index in &indexes {
        if index.state != rubixdb::catalog::schema::IndexState::Ready
            || index.kind == IndexKind::Primary
        {
            continue;
        }
        let Some((access, consumed_count, consumed)) = candidate_index_access(
            table_id,
            index.index_id,
            &index.name,
            &index.column_ordinals,
            &parts,
            &equality_by_ordinal,
            &comparisons,
        ) else {
            continue;
        };
        let better = match &best {
            None => true,
            // Deterministic tie-break (item 53): strictly more consumed
            // conjuncts wins; on an exact tie, the first-seen (lowest
            // `index_id`, since `list_indexes` returns catalog order)
            // is kept — never a `HashMap`-order-dependent choice.
            Some((best_count, _, _)) => consumed_count > *best_count,
        };
        if better {
            best = Some((consumed_count, access, consumed));
        }
    }

    if let Some((_, access, consumed)) = best {
        let residual = residual_of(&parts, &consumed);
        metrics.record_index_scan();
        return Ok(match access {
            PhysicalAccess::IndexScan {
                table_id,
                index_id,
                index_name,
                mode,
                ..
            } => PhysicalAccess::IndexScan {
                table_id,
                index_id,
                index_name,
                mode,
                residual,
            },
            other => other,
        });
    }

    // --- item 41: no safe index access exists — fall back to SeqScan
    // with the whole predicate retained, never a partial/unsafe index
    // plan (item 40: "the planner must never select an index that can
    // omit valid rows"). ---
    metrics.record_seq_scan();
    Ok(PhysicalAccess::SeqScan {
        table_id,
        predicate: Some(predicate.clone()),
    })
}

fn residual_of(parts: &[BoundExpr], consumed: &[bool]) -> Option<BoundExpr> {
    let remaining: Vec<BoundExpr> = parts
        .iter()
        .zip(consumed)
        .filter(|(_, c)| !**c)
        .map(|(e, _)| e.clone())
        .collect();
    recombine(remaining)
}

/// Builds the best access this one `index`'s declared leading-column
/// order (item 8: "composite-index selection must respect declared
/// column order") supports from the given equality/comparison
/// conjuncts, or `None` if not even the index's first column has a
/// usable predicate. Returns `(access, consumed_conjunct_count,
/// consumed_mask)`.
#[allow(clippy::type_complexity)]
fn candidate_index_access(
    table_id: u32,
    index_id: u32,
    index_name: &str,
    column_ordinals: &[u16],
    parts: &[BoundExpr],
    equality_by_ordinal: &std::collections::HashMap<u16, (usize, &BoundExpr)>,
    comparisons: &[(u16, crate::ast::BinaryOp, &BoundExpr, usize)],
) -> Option<(PhysicalAccess, usize, Vec<bool>)> {
    use crate::ast::BinaryOp;

    let mut consumed = vec![false; parts.len()];
    let mut prefix: Vec<BoundExpr> = Vec::new();
    let mut prefix_len = 0usize;
    for &ord in column_ordinals {
        match equality_by_ordinal.get(&ord) {
            Some((idx, expr)) => {
                prefix.push((*expr).clone());
                consumed[*idx] = true;
                prefix_len += 1;
            }
            None => break,
        }
    }

    if prefix_len == column_ordinals.len() && prefix_len > 0 {
        let count = consumed.iter().filter(|c| **c).count();
        return Some((
            PhysicalAccess::IndexScan {
                table_id,
                index_id,
                index_name: index_name.to_string(),
                mode: IndexAccessMode::Equality { prefix },
                residual: None,
            },
            count,
            consumed,
        ));
    }

    // The column immediately following the equality prefix may still
    // contribute a range bound (item 9: "indexed_column >= X,
    // indexed_column < Y").
    let mut lower: Option<(bool, &BoundExpr, usize)> = None; // (inclusive, expr, conjunct_idx)
    let mut upper: Option<(bool, &BoundExpr, usize)> = None;
    if let Some(&next_ord) = column_ordinals.get(prefix_len) {
        for &(ord, op, expr, idx) in comparisons {
            if ord != next_ord {
                continue;
            }
            match op {
                BinaryOp::Gt => lower = Some((false, expr, idx)),
                BinaryOp::GtEq => lower = Some((true, expr, idx)),
                BinaryOp::Lt => upper = Some((false, expr, idx)),
                BinaryOp::LtEq => upper = Some((true, expr, idx)),
                _ => {}
            }
        }
    }

    if prefix_len == 0 && lower.is_none() && upper.is_none() {
        return None; // not even the leading column has a usable predicate
    }
    if prefix_len == 0 && column_ordinals.is_empty() {
        return None;
    }

    let start = match lower {
        Some((inclusive, expr, idx)) => {
            consumed[idx] = true;
            let mut v = prefix.clone();
            v.push(expr.clone());
            if inclusive {
                Bound::Included(v)
            } else {
                Bound::Excluded(v)
            }
        }
        None if prefix_len > 0 => Bound::Included(prefix.clone()),
        None => Bound::Unbounded,
    };
    let end = match upper {
        Some((inclusive, expr, idx)) => {
            consumed[idx] = true;
            let mut v = prefix.clone();
            v.push(expr.clone());
            if inclusive {
                Bound::Included(v)
            } else {
                Bound::Excluded(v)
            }
        }
        None if prefix_len > 0 => Bound::Included(prefix.clone()),
        None => Bound::Unbounded,
    };

    if prefix_len == 0 && lower.is_none() && upper.is_none() {
        return None;
    }

    let count = consumed.iter().filter(|c| **c).count();
    if count == 0 {
        return None;
    }
    Some((
        PhysicalAccess::IndexScan {
            table_id,
            index_id,
            index_name: index_name.to_string(),
            mode: IndexAccessMode::Range { start, end },
            residual: None,
        },
        count,
        consumed,
    ))
}
