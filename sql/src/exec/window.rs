//! Window functions at execution (SQL-mission-2): the `Window` operator.
//!
//! A `Window` operator is a *distinct* operator -- window functions are never
//! computed inside `AggregateOp`. It is blocking: it buffers its whole input
//! (bounded by `max_materialized_rows` / `max_materialized_bytes`), splits it
//! into partitions by the window's `PARTITION BY` key, orders each partition by
//! its `ORDER BY`, computes each function for every row, and emits every input
//! row -- **in its input order** -- with the function values attached
//! (`RowContext::set_window_value`). The window's `ORDER BY` determines what the
//! functions compute, not the order of the output; an outer `ORDER BY` is what
//! gives a result an order.
//!
//! * **Partition keys** are compared with SQL's "not distinct" rule
//!   ([`SetKey`], reused from the set operations): rows whose `PARTITION BY`
//!   values are all `NULL` (or `NULL` in the same places) form one partition,
//!   exactly as `GROUP BY` groups `NULL`s.
//! * **Peers** are rows whose `ORDER BY` values are not distinct from each
//!   other (`NULL`s are peers). Without an `ORDER BY` every row of a partition
//!   is a peer of every other.
//! * **`RANGE` offsets and `NULL` sort keys.** `NULL` keys are ordered by the
//!   window's `NULLS FIRST` / `NULLS LAST` (default: last ascending, first
//!   descending) like any other key, and a value-offset bound (`N PRECEDING` /
//!   `N FOLLOWING`) is a *position* in that order: a `NULL` that sorts before the
//!   data lies before every target value and one that sorts after the data lies
//!   after it, so `UNBOUNDED PRECEDING AND 1 PRECEDING` over `NULLS FIRST` keys
//!   includes the leading `NULL`s. The `NULL`s are peers of each other, so the
//!   frame of a `NULL` row is its `NULL` peers for an offset bound, and the
//!   `UNBOUNDED` / `CURRENT ROW` bounds behave as everywhere. (PostgreSQL's,
//!   Oracle's and DuckDB's treatment; ISO/IEC 9075-2 7.15 orders `NULL`s by the
//!   sort specification.) A `NaN` sort key in a `RANGE` offset frame is an error.
//! * **Frames** (`ROWS` / `RANGE`) are resolved per row to a half-open position
//!   range `[lo, hi)` of the ordered partition, clamped to the partition; a
//!   frame that clamps to nothing is empty (an empty frame aggregates like an
//!   empty input: `COUNT` is 0, the rest `NULL`; `FIRST_VALUE` etc. are `NULL`).
//!
//! | function | sees | result |
//! |---|---|---|
//! | `ROW_NUMBER` | the ordered partition | position + 1 (ties broken by input order) |
//! | `RANK` | peers | the 1-based position of the first peer (gaps after ties) |
//! | `DENSE_RANK` | peers | the number of peer groups up to this row's (no gaps) |
//! | `PERCENT_RANK` | peers | `(rank - 1) / (rows - 1)`; 0 for a single-row partition |
//! | `CUME_DIST` | peers | rows up to and including the last peer, over rows |
//! | `NTILE(n)` | the ordered partition | the bucket, the first `rows % n` buckets one row larger |
//! | `LAG` / `LEAD` | the ordered partition (**not** the frame) | the value `offset` rows before / after, else the default |
//! | `FIRST_VALUE` / `LAST_VALUE` / `NTH_VALUE` | the **frame** | the frame's first / last / n-th row's value |
//! | aggregates | the frame | the aggregate over the frame's rows (and `FILTER`) |
//!
//! `LAST_VALUE` with the default frame (`RANGE BETWEEN UNBOUNDED PRECEDING AND
//! CURRENT ROW`, what an `ORDER BY` without a frame means) is the value of the
//! **current row's last peer** -- the current row itself when there are no ties
//! -- *not* the last row of the partition. Spell the frame out
//! (`ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING`) to get that.
//!
//! Sliding frames recompute the aggregate over the frame for every row
//! (`O(rows x frame width)`); a frame that starts at the partition start is
//! computed incrementally in one pass (`O(rows)`), and a frame that spans the
//! partition is computed once. The deadline and cancellation are checked per
//! row throughout.

use std::cmp::Ordering;
use std::collections::HashMap;

use rubixdb::relational::{RelationalType, RelationalValue};

use crate::aggregate::{AggregateAccumulator, AggregateArg, AggregateState};
use crate::ast::FrameUnits;
use crate::bound::{BoundExpr, BoundFrameBound, BoundWindowFunc, BoundWindowSpec};
use crate::error::{Result, SqlError};
use crate::exec::expr_eval::{eval, eval_predicate};
use crate::exec::operators::{compare_sort_keys, Operator};
use crate::exec::setop::{row_bytes, rows_not_distinct, SetKey};
use crate::exec::{ExecCtx, RowContext, Tuple};
use crate::plan::logical::WindowSlot;

type Val = Option<RelationalValue>;

pub(super) struct WindowOp<'a> {
    input: Box<dyn Operator<'a> + 'a>,
    spec: BoundWindowSpec,
    functions: Vec<WindowSlot>,
    output: Option<std::vec::IntoIter<Tuple>>,
}

impl<'a> WindowOp<'a> {
    pub(super) fn build(
        input: Box<dyn Operator<'a> + 'a>,
        spec: &BoundWindowSpec,
        functions: &[WindowSlot],
    ) -> Self {
        WindowOp {
            input,
            spec: spec.clone(),
            functions: functions.to_vec(),
            output: None,
        }
    }
}

impl<'a> Operator<'a> for WindowOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        if self.output.is_none() {
            let out = self.materialize(ec)?;
            self.output = Some(out.into_iter());
        }
        Ok(self.output.as_mut().expect("materialized above").next())
    }
}

// =======================================================================
// Constants and numbers
// =======================================================================

fn runtime(detail: impl Into<String>) -> SqlError {
    SqlError::ExecutionParameter {
        detail: detail.into(),
    }
}

/// A constant integer argument (NTILE bucket count, LAG offset, NTH_VALUE
/// position, ROWS frame offset), evaluated once against an empty row.
fn const_i64(expr: &BoundExpr, what: &str, ec: &ExecCtx) -> Result<i64> {
    match eval(expr, &RowContext::new(), ec)? {
        Some(RelationalValue::Integer(n)) => Ok(i64::from(n)),
        Some(RelationalValue::Bigint(n)) => Ok(n),
        None => Err(runtime(format!("{what} must not be NULL"))),
        Some(_) => Err(runtime(format!("{what} must be an integer"))),
    }
}

/// A numeric `ORDER BY` key or `RANGE` frame offset, exact for the integer and
/// decimal types (all values of one column share a scale, so unscaled integers
/// compare and add correctly) and `f64` for `REAL` / `DOUBLE`.
#[derive(Clone, Copy, Debug)]
enum Num {
    Int(i128),
    Float(f64),
}

fn to_num(v: &RelationalValue) -> Result<Num> {
    Ok(match v {
        RelationalValue::Integer(n) => Num::Int(i128::from(*n)),
        RelationalValue::Bigint(n) => Num::Int(i128::from(*n)),
        RelationalValue::Decimal(n, _) => Num::Int(*n),
        RelationalValue::Real(f) => Num::Float(float_key(f64::from(*f))?),
        RelationalValue::Double(f) => Num::Float(float_key(*f)?),
        _ => return Err(runtime("a RANGE frame needs a numeric ORDER BY value")),
    })
}

fn float_key(f: f64) -> Result<f64> {
    if f.is_nan() {
        Err(runtime(
            "NaN cannot be used as the ORDER BY key of a RANGE frame",
        ))
    } else {
        Ok(f)
    }
}

fn num_cmp(a: Num, b: Num) -> Ordering {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => x.cmp(&y),
        (Num::Float(x), Num::Float(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        // Cannot happen: the offset is bound with the key's type.
        (Num::Int(x), Num::Float(y)) => (x as f64).partial_cmp(&y).unwrap_or(Ordering::Equal),
        (Num::Float(x), Num::Int(y)) => x.partial_cmp(&(y as f64)).unwrap_or(Ordering::Equal),
    }
}

/// `key + sign * offset` (`sign` is +1 or -1), saturating for integers.
fn num_shift(key: Num, offset: Num, sign: i8) -> Num {
    match (key, offset) {
        (Num::Int(k), Num::Int(o)) => Num::Int(if sign >= 0 {
            k.saturating_add(o)
        } else {
            k.saturating_sub(o)
        }),
        (Num::Float(k), Num::Float(o)) => Num::Float(if sign >= 0 { k + o } else { k - o }),
        (Num::Int(k), Num::Float(o)) => Num::Float(if sign >= 0 {
            k as f64 + o
        } else {
            k as f64 - o
        }),
        (Num::Float(k), Num::Int(o)) => Num::Float(if sign >= 0 {
            k + o as f64
        } else {
            k - o as f64
        }),
    }
}

// =======================================================================
// The resolved frame
// =======================================================================

#[derive(Clone, Copy, Debug)]
enum RBound {
    UnboundedPreceding,
    Preceding(Offset),
    CurrentRow,
    Following(Offset),
    UnboundedFollowing,
}

#[derive(Clone, Copy, Debug)]
enum Offset {
    Rows(u64),
    Range(Num),
}

struct ResolvedFrame {
    units: FrameUnits,
    start: RBound,
    end: RBound,
}

fn resolve_bound(b: &BoundFrameBound, units: FrameUnits, ec: &ExecCtx) -> Result<RBound> {
    let offset = |e: &BoundExpr| -> Result<Offset> {
        match units {
            FrameUnits::Rows | FrameUnits::Groups => {
                let what = if units == FrameUnits::Rows {
                    "a ROWS frame offset"
                } else {
                    "a GROUPS frame offset"
                };
                let n = const_i64(e, what, ec)?;
                if n < 0 {
                    return Err(runtime(format!("{what} must not be negative")));
                }
                Ok(Offset::Rows(n as u64))
            }
            FrameUnits::Range => {
                let v = eval(e, &RowContext::new(), ec)?
                    .ok_or_else(|| runtime("a RANGE frame offset must not be NULL"))?;
                let n = to_num(&v)?;
                let negative = match n {
                    Num::Int(i) => i < 0,
                    Num::Float(f) => f < 0.0,
                };
                if negative {
                    return Err(runtime("a RANGE frame offset must not be negative"));
                }
                Ok(Offset::Range(n))
            }
        }
    };
    Ok(match b {
        BoundFrameBound::UnboundedPreceding => RBound::UnboundedPreceding,
        BoundFrameBound::CurrentRow => RBound::CurrentRow,
        BoundFrameBound::UnboundedFollowing => RBound::UnboundedFollowing,
        BoundFrameBound::Preceding(e) => RBound::Preceding(offset(e)?),
        BoundFrameBound::Following(e) => RBound::Following(offset(e)?),
    })
}

/// One ordered partition: the input row index at each position, the peer groups
/// and (for `RANGE` offsets) the numeric order key at each position.
struct Partition {
    /// Input row index of the row at each position.
    rows: Vec<usize>,
    /// First position of the peer group of the row at each position.
    peer_start: Vec<usize>,
    /// One past the last position of that peer group.
    peer_end: Vec<usize>,
    /// The single `ORDER BY` key as a number at each position (`None` for
    /// `NULL`); empty unless the frame has a `RANGE` offset.
    keys: Vec<Option<Num>>,
    /// Positions `[nn_lo, nn_hi)` hold non-`NULL` keys (the `NULL`s are one
    /// block at either end).
    nn_lo: usize,
    nn_hi: usize,
    /// `ORDER BY` direction of the (single) key.
    ascending: bool,
    /// `GROUPS` frames: the peer-group number of the row at each position, and
    /// the first position of each peer group (empty unless the frame is
    /// `GROUPS`). A group ends where the next begins (or at the partition's end).
    group_of: Vec<usize>,
    group_start: Vec<usize>,
}

impl Partition {
    fn len(&self) -> usize {
        self.rows.len()
    }

    /// The position range `[lo, hi)` of the frame of the row at position `p`.
    fn frame(&self, p: usize, f: &ResolvedFrame) -> (usize, usize) {
        let n = self.len();
        if f.units == FrameUnits::Groups {
            return self.groups_frame(p, f);
        }
        let lo = match f.start {
            RBound::UnboundedPreceding => 0,
            RBound::CurrentRow => match f.units {
                FrameUnits::Rows | FrameUnits::Groups => p,
                FrameUnits::Range => self.peer_start[p],
            },
            RBound::Preceding(off) => self.offset_start(p, off, -1, f.units),
            RBound::Following(off) => self.offset_start(p, off, 1, f.units),
            // Refused by the binder as a frame start.
            RBound::UnboundedFollowing => n,
        };
        let hi = match f.end {
            RBound::UnboundedFollowing => n,
            RBound::CurrentRow => match f.units {
                FrameUnits::Rows | FrameUnits::Groups => p + 1,
                FrameUnits::Range => self.peer_end[p],
            },
            RBound::Preceding(off) => self.offset_end(p, off, -1, f.units),
            RBound::Following(off) => self.offset_end(p, off, 1, f.units),
            // Refused by the binder as a frame end.
            RBound::UnboundedPreceding => 0,
        };
        (lo.min(n), hi.min(n))
    }

    /// The frame of the row at position `p` of a `GROUPS` frame: bounds count
    /// peer groups from the group of the current row. A start that reaches
    /// before the first group begins at the partition start, one past the last
    /// group is empty (`n`); an end before the first group is empty (`0`), one
    /// past the last group runs to the partition end.
    fn groups_frame(&self, p: usize, f: &ResolvedFrame) -> (usize, usize) {
        let n = self.len();
        let groups = self.group_start.len();
        let g = self.group_of[p] as i128;
        let group_end = |gi: usize| -> usize {
            if gi + 1 < groups {
                self.group_start[gi + 1]
            } else {
                n
            }
        };
        let offset = |o: Offset| -> i128 {
            match o {
                Offset::Rows(k) => i128::from(k),
                // Cannot happen: GROUPS offsets are integers.
                Offset::Range(_) => 0,
            }
        };
        let lo = match f.start {
            RBound::UnboundedPreceding => 0,
            RBound::CurrentRow => self.peer_start[p],
            RBound::Preceding(o) => {
                let target = g - offset(o);
                if target <= 0 {
                    0
                } else {
                    self.group_start[target as usize]
                }
            }
            RBound::Following(o) => {
                let target = g + offset(o);
                if target >= groups as i128 {
                    n
                } else {
                    self.group_start[target as usize]
                }
            }
            RBound::UnboundedFollowing => n,
        };
        let hi = match f.end {
            RBound::UnboundedFollowing => n,
            RBound::CurrentRow => self.peer_end[p],
            RBound::Preceding(o) => {
                let target = g - offset(o);
                if target < 0 {
                    0
                } else {
                    group_end(target as usize)
                }
            }
            RBound::Following(o) => {
                let target = g + offset(o);
                if target >= groups as i128 {
                    n
                } else {
                    group_end(target as usize)
                }
            }
            RBound::UnboundedPreceding => 0,
        };
        (lo.min(n), hi.min(n))
    }

    /// The first position of the frame for an `N PRECEDING` (`dir` = -1) or
    /// `N FOLLOWING` (`dir` = +1) start bound.
    fn offset_start(&self, p: usize, off: Offset, dir: i8, units: FrameUnits) -> usize {
        match (units, off) {
            (FrameUnits::Rows, Offset::Rows(k)) => {
                if dir < 0 {
                    p.saturating_sub(k.min(usize::MAX as u64) as usize)
                } else {
                    p.saturating_add(k.min(usize::MAX as u64) as usize)
                }
            }
            (_, Offset::Range(o)) => match self.keys[p] {
                // A NULL key has only its peers (the NULLs) within any offset.
                None => self.peer_start[p],
                Some(k) => {
                    let target = num_shift(k, o, dir * self.direction());
                    let asc = self.ascending;
                    // First position at or after `target` in the ordering.
                    let before = |q: usize| match self.keys[q] {
                        Some(kq) => {
                            let c = num_cmp(kq, target);
                            if asc {
                                c == Ordering::Less
                            } else {
                                c == Ordering::Greater
                            }
                        }
                        None => true,
                    };
                    self.nn_lo
                        + partition_point(self.nn_hi - self.nn_lo, |i| before(self.nn_lo + i))
                }
            },
            _ => p,
        }
    }

    /// One past the last position of the frame for an `N PRECEDING` /
    /// `N FOLLOWING` end bound.
    fn offset_end(&self, p: usize, off: Offset, dir: i8, units: FrameUnits) -> usize {
        match (units, off) {
            (FrameUnits::Rows, Offset::Rows(k)) => {
                let k = k.min(usize::MAX as u64 / 2) as usize;
                if dir < 0 {
                    (p + 1).saturating_sub(k)
                } else {
                    p.saturating_add(k).saturating_add(1)
                }
            }
            (_, Offset::Range(o)) => match self.keys[p] {
                None => self.peer_end[p],
                Some(k) => {
                    let target = num_shift(k, o, dir * self.direction());
                    let asc = self.ascending;
                    // Last position at or before `target` in the ordering, + 1.
                    let at_or_before = |q: usize| match self.keys[q] {
                        Some(kq) => {
                            let c = num_cmp(kq, target);
                            if asc {
                                c != Ordering::Greater
                            } else {
                                c != Ordering::Less
                            }
                        }
                        None => true,
                    };
                    self.nn_lo
                        + partition_point(self.nn_hi - self.nn_lo, |i| at_or_before(self.nn_lo + i))
                }
            },
            _ => p + 1,
        }
    }

    /// +1 for an ascending order, -1 for a descending one: `N PRECEDING` means
    /// smaller values when ascending and larger ones when descending.
    fn direction(&self) -> i8 {
        if self.ascending {
            1
        } else {
            -1
        }
    }
}

/// The number of leading elements `0..len` for which `pred` holds (`pred` must
/// hold for a prefix and fail for the rest).
fn partition_point(len: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

// =======================================================================
// Per-function inputs
// =======================================================================

/// What one window function reads per input row, evaluated once up front.
struct FnInput {
    /// The function's value argument (`SUM(x)` / `LAG(x)` / `FIRST_VALUE(x)`).
    value: Vec<Val>,
    /// `LAG` / `LEAD`'s default, per current row.
    default: Vec<Val>,
    /// An aggregate's `FILTER`: `false` keeps the row out of every frame.
    pass: Vec<bool>,
}

impl<'a> WindowOp<'a> {
    fn materialize(&mut self, ec: &ExecCtx<'a>) -> Result<Vec<Tuple>> {
        let limits = ec.limits;

        // ---- buffer the input
        let mut tuples: Vec<Tuple> = Vec::new();
        let mut bytes = 0usize;
        while let Some(t) = self.input.next(ec)? {
            ec.check()?;
            if tuples.len() >= limits.max_materialized_rows {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "a window function's input exceeds max_materialized_rows ({})",
                        limits.max_materialized_rows
                    ),
                });
            }
            bytes = bytes.saturating_add(t.ctx.estimated_bytes());
            if bytes > limits.max_materialized_bytes {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "a window function's input exceeds max_materialized_bytes ({})",
                        limits.max_materialized_bytes
                    ),
                });
            }
            tuples.push(t);
        }
        let n_rows = tuples.len();
        if n_rows == 0 {
            return Ok(tuples);
        }

        // ---- the frame, resolved once (offsets are constants)
        let frame = ResolvedFrame {
            units: self.spec.frame.units,
            start: resolve_bound(&self.spec.frame.start, self.spec.frame.units, ec)?,
            end: resolve_bound(&self.spec.frame.end, self.spec.frame.units, ec)?,
        };
        let range_offsets = frame.units == FrameUnits::Range
            && matches!(
                (frame.start, frame.end),
                (RBound::Preceding(_) | RBound::Following(_), _)
                    | (_, RBound::Preceding(_) | RBound::Following(_))
            );

        // ---- per-row partition keys, order keys and function inputs
        let mut part_keys: Vec<SetKey> = Vec::with_capacity(n_rows);
        let mut order_keys: Vec<Vec<Val>> = Vec::with_capacity(n_rows);
        let mut inputs: Vec<FnInput> = self
            .functions
            .iter()
            .map(|_| FnInput {
                value: Vec::with_capacity(n_rows),
                default: Vec::new(),
                pass: Vec::with_capacity(n_rows),
            })
            .collect();
        let mut filter_skipped = 0u64;
        for t in &tuples {
            ec.check()?;
            let pk = self
                .spec
                .partition_by
                .iter()
                .map(|e| eval(e, &t.ctx, ec))
                .collect::<Result<Vec<_>>>()?;
            bytes = bytes.saturating_add(row_bytes(&pk));
            part_keys.push(SetKey(pk));
            let ok = self
                .spec
                .order_by
                .iter()
                .map(|o| eval(&o.expr, &t.ctx, ec))
                .collect::<Result<Vec<_>>>()?;
            bytes = bytes.saturating_add(row_bytes(&ok));
            order_keys.push(ok);
            for (slot, input) in self.functions.iter().zip(inputs.iter_mut()) {
                let (value, default, pass) = function_inputs(&slot.func, &t.ctx, ec)?;
                if !pass {
                    filter_skipped += 1;
                }
                input.value.push(value);
                input.default.push(default);
                input.pass.push(pass);
            }
            if bytes > limits.max_materialized_bytes {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "a window function's buffers exceed max_materialized_bytes ({})",
                        limits.max_materialized_bytes
                    ),
                });
            }
        }
        if filter_skipped > 0 {
            ec.metrics
                .record_aggregate_filter_rows_skipped(filter_skipped);
        }

        // ---- partitions, in first-seen order
        let mut index_of: HashMap<SetKey, usize> = HashMap::new();
        let mut partitions: Vec<Vec<usize>> = Vec::new();
        for (row, key) in part_keys.into_iter().enumerate() {
            ec.check()?;
            let pi = match index_of.get(&key) {
                Some(&i) => i,
                None => {
                    partitions.push(Vec::new());
                    index_of.insert(key, partitions.len() - 1);
                    partitions.len() - 1
                }
            };
            if partitions[pi].len() as u64 >= limits.max_window_partition_rows {
                return Err(SqlError::ResourceLimit {
                    detail: format!(
                        "a window partition holds more than max_window_partition_rows ({}) rows",
                        limits.max_window_partition_rows
                    ),
                });
            }
            partitions[pi].push(row);
        }
        ec.metrics
            .record_window_partitions_created(partitions.len() as u64);

        // ---- constants of the functions (NTILE, LAG offset, NTH_VALUE position)
        let constants: Vec<FnConst> = self
            .functions
            .iter()
            .map(|f| function_constants(&f.func, ec))
            .collect::<Result<_>>()?;

        // ---- compute
        let mut results: Vec<Vec<Val>> = vec![vec![None; n_rows]; self.functions.len()];
        for rows in partitions {
            ec.check()?;
            let part = self.order_partition(
                rows,
                &order_keys,
                range_offsets,
                frame.units == FrameUnits::Groups,
            )?;
            for (fi, slot) in self.functions.iter().enumerate() {
                compute_function(
                    &slot.func,
                    &constants[fi],
                    &inputs[fi],
                    &part,
                    &frame,
                    &mut results[fi],
                    ec,
                )?;
            }
        }
        ec.metrics
            .record_window_functions_evaluated((n_rows * self.functions.len()) as u64);

        // ---- attach the values, keeping the input order
        for (fi, slot) in self.functions.iter().enumerate() {
            for (row, tuple) in tuples.iter_mut().enumerate() {
                tuple
                    .ctx
                    .set_window_value(slot.slot, std::mem::take(&mut results[fi][row]));
            }
        }
        Ok(tuples)
    }

    /// Orders one partition by the window's `ORDER BY` (stable: ties stay in
    /// input order) and finds its peer groups.
    fn order_partition(
        &self,
        mut rows: Vec<usize>,
        order_keys: &[Vec<Val>],
        range_offsets: bool,
        groups: bool,
    ) -> Result<Partition> {
        let items = &self.spec.order_by;
        if !items.is_empty() {
            rows.sort_by(|&a, &b| compare_sort_keys(items, &order_keys[a], &order_keys[b]));
        }
        let n = rows.len();
        // Peer groups.
        let mut peer_start = vec![0usize; n];
        let mut peer_end = vec![n; n];
        if items.is_empty() {
            // Every row is a peer of every other.
        } else {
            let mut start = 0usize;
            for p in 1..=n {
                let boundary =
                    p == n || !rows_not_distinct(&order_keys[rows[p - 1]], &order_keys[rows[p]]);
                if boundary {
                    for q in start..p {
                        peer_start[q] = start;
                        peer_end[q] = p;
                    }
                    start = p;
                }
            }
        }
        let (mut keys, mut nn_lo, mut nn_hi) = (Vec::new(), 0, n);
        if range_offsets {
            keys = rows
                .iter()
                .map(|&r| match &order_keys[r][0] {
                    Some(v) => to_num(v).map(Some),
                    None => Ok(None),
                })
                .collect::<Result<Vec<_>>>()?;
            nn_lo = keys.iter().position(Option::is_some).unwrap_or(n);
            nn_hi = keys
                .iter()
                .rposition(Option::is_some)
                .map_or(nn_lo, |i| i + 1);
        }
        // Peer groups, numbered in order, for `GROUPS` frames.
        let (mut group_of, mut group_start) = (Vec::new(), Vec::new());
        if groups {
            group_of = vec![0usize; n];
            for p in 0..n {
                if p == 0 || peer_start[p] == p {
                    group_start.push(p);
                }
                group_of[p] = group_start.len() - 1;
            }
        }
        Ok(Partition {
            rows,
            peer_start,
            peer_end,
            keys,
            nn_lo,
            nn_hi,
            ascending: items.first().is_none_or(|i| !i.descending),
            group_of,
            group_start,
        })
    }
}

/// `(value, default, passes FILTER)` of one function for one input row.
fn function_inputs(
    f: &BoundWindowFunc,
    ctx: &RowContext,
    ec: &ExecCtx,
) -> Result<(Val, Val, bool)> {
    Ok(match f {
        BoundWindowFunc::Aggregate { arg, filter, .. } => {
            let pass = match filter {
                Some(flt) => eval_predicate(flt, ctx, ec)?.is_true(),
                None => true,
            };
            let value = if pass {
                crate::exec::expr_eval::eval_aggregate_arg(arg, ctx, ec)?
            } else {
                None
            };
            (value, None, pass)
        }
        BoundWindowFunc::Lag { expr, default, .. }
        | BoundWindowFunc::Lead { expr, default, .. } => {
            let d = match default {
                Some(d) => eval(d, ctx, ec)?,
                None => None,
            };
            (eval(expr, ctx, ec)?, d, true)
        }
        BoundWindowFunc::FirstValue { expr: e, .. }
        | BoundWindowFunc::LastValue { expr: e, .. } => (eval(e, ctx, ec)?, None, true),
        BoundWindowFunc::NthValue { expr, .. } => (eval(expr, ctx, ec)?, None, true),
        _ => (None, None, true),
    })
}

/// The constant argument of a function, evaluated once.
#[derive(Clone, Copy, Default)]
struct FnConst {
    /// NTILE bucket count / LAG, LEAD offset / NTH_VALUE position.
    n: i64,
}

fn function_constants(f: &BoundWindowFunc, ec: &ExecCtx) -> Result<FnConst> {
    Ok(FnConst {
        n: match f {
            BoundWindowFunc::Ntile(n) => {
                let v = const_i64(n, "the bucket count of NTILE", ec)?;
                if v <= 0 {
                    return Err(runtime("the bucket count of NTILE must be positive"));
                }
                v
            }
            BoundWindowFunc::Lag { offset, .. } | BoundWindowFunc::Lead { offset, .. } => {
                match offset {
                    Some(o) => {
                        let v = const_i64(o, "the offset of LAG/LEAD", ec)?;
                        if v < 0 {
                            return Err(runtime("the offset of LAG/LEAD must not be negative"));
                        }
                        v
                    }
                    None => 1,
                }
            }
            BoundWindowFunc::NthValue { n, .. } => {
                let v = const_i64(n, "the position of NTH_VALUE", ec)?;
                if v <= 0 {
                    return Err(runtime("the position of NTH_VALUE must be positive"));
                }
                v
            }
            _ => 0,
        },
    })
}

// =======================================================================
// The functions
// =======================================================================

/// The positions of a partition whose function argument is not `NULL`, for
/// `IGNORE NULLS`: `positions` lists them in order, and `before[p]` is how many
/// of them lie at positions `< p` (so `before[p + 1]` counts those `<= p`), which
/// turns "the k-th non-NULL row before / after / from here" into an index.
struct NonNulls {
    positions: Vec<usize>,
    before: Vec<usize>,
}

impl NonNulls {
    fn of(part: &Partition, input: &FnInput) -> NonNulls {
        let n = part.len();
        let mut positions = Vec::new();
        let mut before = Vec::with_capacity(n + 1);
        for p in 0..n {
            before.push(positions.len());
            if input.value[part.rows[p]].is_some() {
                positions.push(p);
            }
        }
        before.push(positions.len());
        NonNulls { positions, before }
    }
}

fn bigint(n: usize) -> Val {
    Some(RelationalValue::Bigint(n as i64))
}

fn compute_function(
    f: &BoundWindowFunc,
    c: &FnConst,
    input: &FnInput,
    part: &Partition,
    frame: &ResolvedFrame,
    out: &mut [Val],
    ec: &ExecCtx,
) -> Result<()> {
    let n = part.len();
    match f {
        BoundWindowFunc::RowNumber => {
            for p in 0..n {
                out[part.rows[p]] = bigint(p + 1);
            }
        }
        BoundWindowFunc::Rank => {
            for p in 0..n {
                out[part.rows[p]] = bigint(part.peer_start[p] + 1);
            }
        }
        BoundWindowFunc::DenseRank => {
            let mut dense = 0usize;
            for p in 0..n {
                if p == 0 || part.peer_start[p] == p {
                    dense += 1;
                }
                out[part.rows[p]] = bigint(dense);
            }
        }
        BoundWindowFunc::PercentRank => {
            for p in 0..n {
                let v = if n <= 1 {
                    0.0
                } else {
                    part.peer_start[p] as f64 / (n - 1) as f64
                };
                out[part.rows[p]] = Some(RelationalValue::Double(v));
            }
        }
        BoundWindowFunc::CumeDist => {
            for p in 0..n {
                out[part.rows[p]] =
                    Some(RelationalValue::Double(part.peer_end[p] as f64 / n as f64));
            }
        }
        BoundWindowFunc::Ntile(_) => {
            let k = c.n as u128;
            let (base, rem) = (n as u128 / k, n as u128 % k);
            for p in 0..n {
                let p128 = p as u128;
                let bucket = if p128 < rem * (base + 1) {
                    p128 / (base + 1) + 1
                } else {
                    rem + (p128 - rem * (base + 1)) / base + 1
                };
                out[part.rows[p]] = Some(RelationalValue::Bigint(bucket as i64));
            }
        }
        BoundWindowFunc::Lag { ignore_nulls, .. } | BoundWindowFunc::Lead { ignore_nulls, .. } => {
            let backwards = matches!(f, BoundWindowFunc::Lag { .. });
            let off = usize::try_from(c.n).unwrap_or(usize::MAX);
            let nn = ignore_nulls.then(|| NonNulls::of(part, input));
            for p in 0..n {
                let target = match (&nn, off) {
                    // An offset of 0 is the current row, whatever its value.
                    (_, 0) => Some(p),
                    (None, _) => {
                        if backwards {
                            p.checked_sub(off)
                        } else {
                            p.checked_add(off).filter(|&t| t < n)
                        }
                    }
                    // IGNORE NULLS: the `off`-th row before / after whose value is
                    // not NULL.
                    (Some(nn), _) => {
                        if backwards {
                            let before = nn.before[p];
                            before.checked_sub(off).map(|i| nn.positions[i])
                        } else {
                            let through = nn.before[p + 1];
                            through
                                .checked_add(off - 1)
                                .filter(|&i| i < nn.positions.len())
                                .map(|i| nn.positions[i])
                        }
                    }
                };
                let row = part.rows[p];
                out[row] = match target {
                    Some(t) => input.value[part.rows[t]].clone(),
                    None => input.default[row].clone(),
                };
            }
        }
        BoundWindowFunc::FirstValue { ignore_nulls, .. }
        | BoundWindowFunc::LastValue { ignore_nulls, .. }
        | BoundWindowFunc::NthValue { ignore_nulls, .. } => {
            let nn = ignore_nulls.then(|| NonNulls::of(part, input));
            for p in 0..n {
                ec.check()?;
                let (lo, hi) = part.frame(p, frame);
                let k = usize::try_from(c.n).unwrap_or(usize::MAX);
                let pos = if lo >= hi {
                    None
                } else {
                    match (&nn, f) {
                        (None, BoundWindowFunc::FirstValue { .. }) => Some(lo),
                        (None, BoundWindowFunc::LastValue { .. }) => Some(hi - 1),
                        (None, _) => lo.checked_add(k - 1).filter(|&t| t < hi),
                        // IGNORE NULLS: only the frame's rows with a value count.
                        (Some(nn), BoundWindowFunc::FirstValue { .. }) => {
                            nn.positions.get(nn.before[lo]).copied().filter(|&t| t < hi)
                        }
                        (Some(nn), BoundWindowFunc::LastValue { .. }) => nn.before[hi]
                            .checked_sub(1)
                            .map(|i| nn.positions[i])
                            .filter(|&t| t >= lo),
                        (Some(nn), _) => nn.before[lo]
                            .checked_add(k - 1)
                            .and_then(|i| nn.positions.get(i).copied())
                            .filter(|&t| t < hi),
                    }
                };
                out[part.rows[p]] = pos.and_then(|q| input.value[part.rows[q]].clone());
            }
        }
        BoundWindowFunc::Aggregate {
            func,
            arg,
            distinct,
            ..
        } => {
            let wildcard = matches!(arg, AggregateArg::Wildcard);
            let in_ty: Option<RelationalType> = match arg {
                AggregateArg::Wildcard | AggregateArg::Multi(_) => None,
                AggregateArg::Expr(e) => e.ty,
            };
            let distinct = *distinct;
            let state_limit = ec.limits.max_aggregate_state_bytes;
            let fresh = || -> Result<AggregateAccumulator> {
                Ok(AggregateAccumulator::new(
                    AggregateState::initial(*func, wildcard, in_ty)?,
                    distinct,
                ))
            };
            let feed = |state: &mut AggregateAccumulator, q: usize| -> Result<()> {
                let row = part.rows[q];
                if !input.pass[row] {
                    return Ok(());
                }
                state.update(input.value[row].as_ref())?;
                // A DISTINCT frame remembers every distinct value it saw.
                if distinct && state.estimated_bytes() > state_limit {
                    ec.metrics.record_aggregate_resource_limit_hit();
                    return Err(SqlError::ResourceLimit {
                        detail: format!(
                            "window aggregate state exceeded max_aggregate_state_bytes ({state_limit})"
                        ),
                    });
                }
                Ok(())
            };

            let spans_partition = matches!(frame.start, RBound::UnboundedPreceding)
                && matches!(frame.end, RBound::UnboundedFollowing);
            if spans_partition {
                let mut state = fresh()?;
                for q in 0..n {
                    ec.check()?;
                    feed(&mut state, q)?;
                }
                let v = state.finalize();
                for p in 0..n {
                    out[part.rows[p]] = v.clone();
                }
            } else if matches!(frame.start, RBound::UnboundedPreceding) {
                // The frame starts at the partition start and its end never
                // moves back: one running state, fed each row once.
                let mut state = fresh()?;
                let mut fed = 0usize;
                for p in 0..n {
                    ec.check()?;
                    let (_, hi) = part.frame(p, frame);
                    if hi < fed {
                        // Defensive: an end that moved back (cannot happen for
                        // the supported frames) restarts the running state.
                        state = fresh()?;
                        fed = 0;
                    }
                    while fed < hi {
                        feed(&mut state, fed)?;
                        fed += 1;
                    }
                    out[part.rows[p]] = state.current();
                }
            } else {
                for p in 0..n {
                    ec.check()?;
                    let (lo, hi) = part.frame(p, frame);
                    let mut state = fresh()?;
                    for q in lo..hi {
                        feed(&mut state, q)?;
                    }
                    out[part.rows[p]] = state.finalize();
                }
            }
        }
    }
    Ok(())
}
