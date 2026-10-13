//! Set operations at execution (SQL-mission-1 Slice E): `UNION`, `INTERSECT`,
//! `EXCEPT`, each with and without `ALL`.
//!
//! A set operation is **not** a join. Its operator takes the rows of its two
//! operand plans, widens every value to the column's unified type
//! (`widen_value`), and combines the rows as the standard says. A row's
//! identity is its *whole tuple* under SQL's "not distinct" comparison
//! ([`rows_not_distinct`]): two `NULL`s are the same value there, which is not
//! what the `=` operator says (`NULL = NULL` is `UNKNOWN`) and not something
//! `=` is ever used for in this module.
//!
//! | operator | rows produced | memory held |
//! |---|---|---|
//! | `UNION ALL` | every left row, then every right row | none (streams) |
//! | `UNION` | the first occurrence of each distinct row of left ++ right | the distinct rows seen |
//! | `INTERSECT` | each distinct row of left that is in right | right's distinct rows + emitted left rows |
//! | `INTERSECT ALL` | each row `min(count_left, count_right)` times | right's distinct rows with counts |
//! | `EXCEPT` | each distinct row of left that is not in right | right's distinct rows + emitted left rows |
//! | `EXCEPT ALL` | each row `max(0, count_left - count_right)` times | right's distinct rows with counts |
//!
//! Whatever is held is bounded by `max_materialized_rows` (distinct rows) and
//! `max_materialized_bytes`: exceeding either is `RESOURCE_LIMIT`, never a
//! truncated result. Rows come out in the order their first occurrence appears
//! in the left operand, then the right (`UNION`); a set operation's result
//! order is otherwise unspecified without an `ORDER BY`.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use rubixdb::relational::{RelationalType, RelationalValue};

use crate::ast::SetOperator;
use crate::error::{Result, SqlError};
use crate::exec::operators::{build_operator, Operator};
use crate::exec::{ExecCtx, RowContext, Tuple};
use crate::plan::logical::SetOpNode;
use crate::plan::physical::PhysicalPlan;

pub(crate) type Row = Vec<Option<RelationalValue>>;

// =======================================================================
// The distinctness predicate
// =======================================================================

/// SQL's "not distinct" for two values: `NULL` and `NULL` are the same value,
/// `NULL` and a non-`NULL` are different, and two non-`NULL` values are the
/// same when they are of the same type and equal. For floating point, `+0.0`
/// and `-0.0` are the same value and all `NaN`s are the same value (the
/// grouping rule `GROUP BY` and `DISTINCT` already use). Values of different
/// types are never the same: by the time rows reach this comparison every
/// column has been widened to one type, so a mixed pair is only ever compared
/// by mistake, and is reported as "different" rather than guessed equal.
pub(crate) fn values_not_distinct(a: Option<&RelationalValue>, b: Option<&RelationalValue>) -> bool {
    use RelationalValue::*;
    match (a, b) {
        (None, None) => true,
        (None, Some(_)) | (Some(_), None) => false,
        (Some(x), Some(y)) => match (x, y) {
            (Boolean(p), Boolean(q)) => p == q,
            (Integer(p), Integer(q)) => p == q,
            (Bigint(p), Bigint(q)) => p == q,
            (Real(p), Real(q)) => (p.is_nan() && q.is_nan()) || p == q,
            (Double(p), Double(q)) => (p.is_nan() && q.is_nan()) || p == q,
            // The scale is part of the value's representation (columns are
            // widened to one scale first).
            (Decimal(p, sp), Decimal(q, sq)) => sp == sq && p == q,
            (Text(p), Text(q)) => p == q,
            (Blob(p), Blob(q)) => p == q,
            (Date(p), Date(q)) => p == q,
            (Time(p), Time(q)) => p == q,
            (Timestamp(p), Timestamp(q)) => p == q,
            _ => false,
        },
    }
}

/// Two rows are not distinct when they have the same number of columns and
/// every pair of columns is not distinct. The empty row is not distinct from
/// the empty row.
pub(crate) fn rows_not_distinct(a: &[Option<RelationalValue>], b: &[Option<RelationalValue>]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| values_not_distinct(x.as_ref(), y.as_ref()))
}

/// A row as a hash-map key whose equality *is* [`rows_not_distinct`] and whose
/// hash is consistent with it (equal rows hash alike: floats are hashed in a
/// canonical form).
#[derive(Debug, Clone)]
pub(crate) struct SetKey(pub Row);

impl PartialEq for SetKey {
    fn eq(&self, other: &Self) -> bool {
        rows_not_distinct(&self.0, &other.0)
    }
}
impl Eq for SetKey {}

impl Hash for SetKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        use RelationalValue::*;
        self.0.len().hash(state);
        for v in &self.0 {
            match v {
                None => 0u8.hash(state),
                Some(Boolean(b)) => (1u8, b).hash(state),
                Some(Integer(i)) => (2u8, i).hash(state),
                Some(Bigint(i)) => (3u8, i).hash(state),
                Some(Real(f)) => {
                    let bits = if f.is_nan() { f32::NAN.to_bits() } else if *f == 0.0 { 0 } else { f.to_bits() };
                    (4u8, bits).hash(state)
                }
                Some(Double(f)) => {
                    let bits = if f.is_nan() { f64::NAN.to_bits() } else if *f == 0.0 { 0 } else { f.to_bits() };
                    (5u8, bits).hash(state)
                }
                Some(Decimal(v, s)) => (6u8, v, s).hash(state),
                Some(Text(s)) => (7u8, s).hash(state),
                Some(Blob(b)) => (8u8, b).hash(state),
                Some(Date(d)) => (9u8, d).hash(state),
                Some(Time(t)) => (10u8, t).hash(state),
                Some(Timestamp(t)) => (11u8, t).hash(state),
            }
        }
    }
}

/// Estimated bytes a row occupies in a key set (the same per-value sizes
/// `GroupingValue::estimated_bytes` uses, so the byte bound means the same
/// thing here as in `GROUP BY` and `DISTINCT`).
pub(crate) fn row_bytes(row: &[Option<RelationalValue>]) -> usize {
    use RelationalValue::*;
    std::mem::size_of::<SetKey>()
        + row
            .iter()
            .map(|v| match v {
                None => 1,
                Some(Boolean(_)) => 2,
                Some(Integer(_)) | Some(Real(_)) | Some(Date(_)) => 5,
                Some(Bigint(_)) | Some(Double(_)) | Some(Time(_)) | Some(Timestamp(_)) => 9,
                Some(Decimal(..)) => 18,
                Some(Text(s)) => 16 + s.len(),
                Some(Blob(b)) => 16 + b.len(),
            })
            .sum::<usize>()
}

// =======================================================================
// Widening to the unified column type
// =======================================================================

fn out_of_range(to: RelationalType) -> SqlError {
    SqlError::ExecutionParameter {
        detail: format!("a value is out of range for the {to:?} column of a set operation"),
    }
}

/// `10^exp` as an `i128`, `None` if it does not fit.
fn pow10(exp: u32) -> Option<i128> {
    10i128.checked_pow(exp)
}

/// Converts `value` to the unified column type `to`. Only the conversions the
/// bind-time unification table allows exist: a value already of type `to` is
/// returned unchanged, `INTEGER` widens to `BIGINT` / `DECIMAL` / `DOUBLE`,
/// `BIGINT` to `DECIMAL` / `DOUBLE`, `REAL` to `DOUBLE`, `DECIMAL` to a
/// larger-scale `DECIMAL` or to `DOUBLE`. A `BIGINT` or `DECIMAL` widened to
/// `DOUBLE` is rounded to the nearest representable value (magnitudes above
/// 2^53 are not exact); a widening to `DECIMAL` that overflows the 128-bit
/// representation is an error, never a wrapped value.
pub(crate) fn widen_value(value: RelationalValue, to: RelationalType) -> Result<RelationalValue> {
    use RelationalValue as V;
    Ok(match (value, to) {
        (v @ V::Boolean(_), RelationalType::Boolean)
        | (v @ V::Integer(_), RelationalType::Integer)
        | (v @ V::Bigint(_), RelationalType::Bigint)
        | (v @ V::Real(_), RelationalType::Real)
        | (v @ V::Double(_), RelationalType::Double)
        | (v @ V::Text(_), RelationalType::Text)
        | (v @ V::Blob(_), RelationalType::Blob)
        | (v @ V::Date(_), RelationalType::Date)
        | (v @ V::Time(_), RelationalType::Time)
        | (v @ V::Timestamp(_), RelationalType::Timestamp) => v,
        (V::Integer(i), RelationalType::Bigint) => V::Bigint(i64::from(i)),
        (V::Integer(i), RelationalType::Double) => V::Double(f64::from(i)),
        (V::Bigint(i), RelationalType::Double) => V::Double(i as f64),
        (V::Real(f), RelationalType::Double) => V::Double(f64::from(f)),
        (V::Integer(i), RelationalType::Decimal { scale }) => {
            decimal_from(i128::from(i), 0, scale, to)?
        }
        (V::Bigint(i), RelationalType::Decimal { scale }) => {
            decimal_from(i128::from(i), 0, scale, to)?
        }
        (V::Decimal(v, s), RelationalType::Decimal { scale }) => decimal_from(v, s, scale, to)?,
        (V::Decimal(v, s), RelationalType::Double) => {
            let divisor = pow10(u32::from(s)).map(|d| d as f64).unwrap_or(f64::INFINITY);
            V::Double(v as f64 / divisor)
        }
        // Only a value whose type the binder could not know (a `$n` parameter
        // supplied with another type than the column's) gets here.
        (other, target) => {
            return Err(SqlError::ExecutionParameter {
                detail: format!(
                    "a {} value cannot be converted to the {target:?} column of a set operation",
                    value_kind(&other)
                ),
            })
        }
    })
}

/// `value` at scale `from` as a decimal of scale `to_scale` (`to_scale >=
/// from`).
fn decimal_from(value: i128, from: u8, to_scale: u8, to: RelationalType) -> Result<RelationalValue> {
    let shift = u32::from(to_scale)
        .checked_sub(u32::from(from))
        .ok_or_else(|| out_of_range(to))?;
    let factor = pow10(shift).ok_or_else(|| out_of_range(to))?;
    let scaled = value.checked_mul(factor).ok_or_else(|| out_of_range(to))?;
    Ok(RelationalValue::Decimal(scaled, to_scale))
}

fn value_kind(v: &RelationalValue) -> &'static str {
    use RelationalValue::*;
    match v {
        Boolean(_) => "BOOLEAN",
        Integer(_) => "INTEGER",
        Bigint(_) => "BIGINT",
        Real(_) => "REAL",
        Double(_) => "DOUBLE",
        Decimal(..) => "DECIMAL",
        Text(_) => "TEXT",
        Blob(_) => "BLOB",
        Date(_) => "DATE",
        Time(_) => "TIME",
        Timestamp(_) => "TIMESTAMP",
    }
}

/// Widens every value of `row` to its column's unified type (`NULL` stays
/// `NULL`; a column with no type -- both operands were an untyped `NULL` -- has
/// only `NULL`s).
pub(crate) fn widen_row(row: Row, types: &[Option<RelationalType>]) -> Result<Row> {
    row.into_iter()
        .zip(types)
        .map(|(v, ty)| match (v, ty) {
            (Some(v), Some(ty)) => widen_value(v, *ty).map(Some),
            (v, _) => Ok(v),
        })
        .collect()
}

// =======================================================================
// The operator
// =======================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    UnionAll,
    Union,
    Intersect,
    IntersectAll,
    Except,
    ExceptAll,
}

/// One operand: its plan is opened on the first row asked of it, so a `UNION
/// ALL` that a `LIMIT` stops early never starts its right operand.
struct Operand<'a> {
    plan: PhysicalPlan,
    op: Option<Box<dyn Operator<'a> + 'a>>,
}

impl<'a> Operand<'a> {
    fn next(
        &mut self,
        outer: &RowContext,
        types: &[Option<RelationalType>],
        ec: &ExecCtx<'a>,
    ) -> Result<Option<Row>> {
        if self.op.is_none() {
            self.op = Some(build_operator(&self.plan, outer, ec)?);
        }
        match self.op.as_mut().expect("opened above").next(ec)? {
            None => Ok(None),
            Some(tuple) => widen_row(tuple.projected, types).map(Some),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Left,
    Right,
    Done,
}

/// How many times a row is in the right operand (`remaining` is consumed by
/// the `ALL` forms) and whether the left operand has already produced it (the
/// distinct forms).
struct Slot {
    remaining: u64,
    emitted: bool,
}

struct SetOpOp<'a> {
    kind: Kind,
    left: Operand<'a>,
    right: Operand<'a>,
    types: Vec<Option<RelationalType>>,
    set_ref: u32,
    outer: RowContext,
    phase: Phase,
    /// `UNION`: the distinct rows produced so far.
    seen: HashSet<SetKey>,
    /// `INTERSECT` / `EXCEPT` (both forms): built from the right operand the
    /// first time a left row is asked for.
    slots: Option<HashMap<SetKey, Slot>>,
    bytes: usize,
}

pub(super) fn open_set_op<'a>(
    node: &SetOpNode,
    outer: &RowContext,
    _ec: &ExecCtx<'a>,
) -> Result<Box<dyn Operator<'a> + 'a>> {
    let kind = match (node.op, node.all) {
        (SetOperator::Union, true) => Kind::UnionAll,
        (SetOperator::Union, false) => Kind::Union,
        (SetOperator::Intersect, false) => Kind::Intersect,
        (SetOperator::Intersect, true) => Kind::IntersectAll,
        (SetOperator::Except, false) => Kind::Except,
        (SetOperator::Except, true) => Kind::ExceptAll,
    };
    Ok(Box::new(SetOpOp {
        kind,
        left: Operand {
            plan: (*node.left).clone(),
            op: None,
        },
        right: Operand {
            plan: (*node.right).clone(),
            op: None,
        },
        types: node.column_types.clone(),
        set_ref: node.set_ref,
        outer: outer.clone(),
        phase: Phase::Left,
        seen: HashSet::new(),
        slots: None,
        bytes: 0,
    }))
}

impl<'a> SetOpOp<'a> {
    fn emit(&self, row: Row) -> Tuple {
        Tuple {
            ctx: self.outer.with_row(self.set_ref, row.clone()),
            projected: row,
        }
    }

    /// The next row of `left ++ right`, in that order.
    fn next_concatenated(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Row>> {
        loop {
            match self.phase {
                Phase::Left => match self.left.next(&self.outer, &self.types, ec)? {
                    Some(row) => return Ok(Some(row)),
                    None => self.phase = Phase::Right,
                },
                Phase::Right => match self.right.next(&self.outer, &self.types, ec)? {
                    Some(row) => return Ok(Some(row)),
                    None => self.phase = Phase::Done,
                },
                Phase::Done => return Ok(None),
            }
        }
    }

    /// Fails closed if holding one more row would pass the materialization
    /// bounds.
    fn admit(&mut self, held: usize, row: &[Option<RelationalValue>], what: &str, ec: &ExecCtx<'a>) -> Result<()> {
        if held >= ec.limits.max_materialized_rows {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "{what} holds more than max_materialized_rows ({}) distinct rows",
                    ec.limits.max_materialized_rows
                ),
            });
        }
        self.bytes = self.bytes.saturating_add(row_bytes(row));
        if self.bytes > ec.limits.max_materialized_bytes {
            return Err(SqlError::ResourceLimit {
                detail: format!(
                    "{what} exceeded max_materialized_bytes ({})",
                    ec.limits.max_materialized_bytes
                ),
            });
        }
        Ok(())
    }

    /// Counts the right operand's rows (`INTERSECT` / `EXCEPT`).
    fn build_slots(&mut self, ec: &ExecCtx<'a>) -> Result<()> {
        let what = if matches!(self.kind, Kind::Intersect | Kind::IntersectAll) {
            "INTERSECT"
        } else {
            "EXCEPT"
        };
        let mut slots: HashMap<SetKey, Slot> = HashMap::new();
        while let Some(row) = self.right.next(&self.outer, &self.types, ec)? {
            ec.check()?;
            let key = SetKey(row);
            if let Some(slot) = slots.get_mut(&key) {
                slot.remaining += 1;
                continue;
            }
            self.admit(slots.len(), &key.0, what, ec)?;
            slots.insert(key, Slot { remaining: 1, emitted: false });
        }
        self.slots = Some(slots);
        Ok(())
    }
}

impl<'a> Operator<'a> for SetOpOp<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>> {
        match self.kind {
            Kind::UnionAll => {
                ec.check()?;
                Ok(self.next_concatenated(ec)?.map(|row| self.emit(row)))
            }
            Kind::Union => loop {
                ec.check()?;
                let Some(row) = self.next_concatenated(ec)? else {
                    return Ok(None);
                };
                let key = SetKey(row);
                if self.seen.contains(&key) {
                    ec.metrics.record_set_op_rows_deduplicated(1);
                    continue;
                }
                self.admit(self.seen.len(), &key.0, "UNION", ec)?;
                let tuple = self.emit(key.0.clone());
                self.seen.insert(key);
                return Ok(Some(tuple));
            },
            Kind::Intersect | Kind::IntersectAll | Kind::Except | Kind::ExceptAll => {
                if self.slots.is_none() {
                    self.build_slots(ec)?;
                }
                loop {
                    ec.check()?;
                    let Some(row) = self.left.next(&self.outer, &self.types, ec)? else {
                        return Ok(None);
                    };
                    let key = SetKey(row);
                    let mut slots = self.slots.take().expect("built above");
                    let verdict = self.judge(&mut slots, key, ec);
                    self.slots = Some(slots);
                    if let Some(row) = verdict? {
                        return Ok(Some(self.emit(row)));
                    }
                }
            }
        }
    }
}

impl<'a> SetOpOp<'a> {
    /// What one left row does for `INTERSECT` / `EXCEPT`: `Some(row)` to emit
    /// it, `None` to drop it.
    fn judge(
        &mut self,
        slots: &mut HashMap<SetKey, Slot>,
        key: SetKey,
        ec: &ExecCtx<'a>,
    ) -> Result<Option<Row>> {
        match self.kind {
            // Each distinct left row that is also on the right, once.
            Kind::Intersect => match slots.get_mut(&key) {
                Some(slot) if !slot.emitted => {
                    slot.emitted = true;
                    Ok(Some(key.0))
                }
                Some(_) => {
                    ec.metrics.record_set_op_rows_deduplicated(1);
                    Ok(None)
                }
                None => Ok(None),
            },
            // min(left, right) copies: each right copy cancels one left copy.
            Kind::IntersectAll => match slots.get_mut(&key) {
                Some(slot) if slot.remaining > 0 => {
                    slot.remaining -= 1;
                    Ok(Some(key.0))
                }
                _ => Ok(None),
            },
            // Each distinct left row that is not on the right, once.
            Kind::Except => match slots.get_mut(&key) {
                Some(slot) if slot.remaining > 0 => Ok(None),
                Some(slot) if slot.emitted => {
                    ec.metrics.record_set_op_rows_deduplicated(1);
                    Ok(None)
                }
                Some(slot) => {
                    slot.emitted = true;
                    Ok(Some(key.0))
                }
                None => {
                    self.admit(slots.len(), &key.0, "EXCEPT", ec)?;
                    let row = key.0.clone();
                    slots.insert(key, Slot { remaining: 0, emitted: true });
                    Ok(Some(row))
                }
            },
            // max(0, left - right) copies: each right copy cancels one left copy.
            Kind::ExceptAll => match slots.get_mut(&key) {
                Some(slot) if slot.remaining > 0 => {
                    slot.remaining -= 1;
                    Ok(None)
                }
                _ => Ok(Some(key.0)),
            },
            Kind::UnionAll | Kind::Union => unreachable!("handled by the caller"),
        }
    }
}
