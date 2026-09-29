//! Production-grade aggregation contracts, state representations, and
//! grouping key canonicalization — Increment 11.
//!
//! Centralizes all aggregate semantics (item 6: "Every aggregate function
//! must define: name, accepted argument types, return type, NULL behavior,
//! empty-input behavior, state representation, merge semantics, resource
//! implications... centralize aggregate semantics").

use rubixdb::relational::{
    validate_decimal, RelationalError, RelationalType, RelationalValue, MAX_DECIMAL_PRECISION,
};

use crate::error::{Result, SqlError};

/// The closed set of approved aggregate functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggregateFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggregateFunc {
    pub fn name(self) -> &'static str {
        match self {
            AggregateFunc::Count => "count",
            AggregateFunc::Sum => "sum",
            AggregateFunc::Avg => "avg",
            AggregateFunc::Min => "min",
            AggregateFunc::Max => "max",
        }
    }

    pub fn lookup(name: &str) -> Option<AggregateFunc> {
        match name.to_ascii_lowercase().as_str() {
            "count" => Some(AggregateFunc::Count),
            "sum" => Some(AggregateFunc::Sum),
            "avg" => Some(AggregateFunc::Avg),
            "min" => Some(AggregateFunc::Min),
            "max" => Some(AggregateFunc::Max),
            _ => None,
        }
    }
}

/// The argument to an aggregate function call in AST/Bound representations.
#[derive(Debug, Clone, PartialEq)]
pub enum AggregateArg<T> {
    /// `COUNT(*)`
    Wildcard,
    /// `COUNT(expr)`, `SUM(expr)`, etc.
    Expr(T),
}

/// One aggregate function's contract and type rules.
pub struct AggregateContract;

impl AggregateContract {
    /// Validates whether `ty` is accepted as an argument to `func`.
    pub fn check_argument_type(
        func: AggregateFunc,
        is_wildcard: bool,
        ty: Option<RelationalType>,
    ) -> Result<()> {
        if is_wildcard {
            if func == AggregateFunc::Count {
                return Ok(());
            }
            return Err(SqlError::TypeMismatch {
                detail: format!(
                    "aggregate function {} does not support wildcard argument (*)",
                    func.name()
                ),
            });
        }

        let Some(ty) = ty else {
            // Untyped NULL literal argument is accepted for COUNT and MIN/MAX,
            // but for numeric aggregates (SUM, AVG) an untyped NULL lacks a concrete numeric type.
            return match func {
                AggregateFunc::Count | AggregateFunc::Min | AggregateFunc::Max => Ok(()),
                AggregateFunc::Sum | AggregateFunc::Avg => Err(SqlError::TypeMismatch {
                    detail: format!(
                        "aggregate function {} requires a numeric operand",
                        func.name()
                    ),
                }),
            };
        };

        match func {
            AggregateFunc::Count => Ok(()),
            AggregateFunc::Sum => {
                if is_numeric(ty) {
                    Ok(())
                } else {
                    Err(SqlError::TypeMismatch {
                        detail: format!("SUM requires a numeric operand, found {ty:?}"),
                    })
                }
            }
            AggregateFunc::Avg => match ty {
                RelationalType::Integer
                | RelationalType::Bigint
                | RelationalType::Real
                | RelationalType::Double => Ok(()),
                RelationalType::Decimal { .. } => Err(SqlError::Unsupported {
                    detail: "AVG(DECIMAL) is unsupported because DECIMAL division requires scale rescaling not present in RelationalValue".to_string(),
                }),
                _ => Err(SqlError::TypeMismatch {
                    detail: format!("AVG requires a numeric operand, found {ty:?}"),
                }),
            },
            AggregateFunc::Min | AggregateFunc::Max => Ok(()),
        }
    }

    /// Derives the return type and nullability for an aggregate function call.
    pub fn derive_return_type(
        func: AggregateFunc,
        is_wildcard: bool,
        input_type: Option<RelationalType>,
    ) -> Result<(RelationalType, bool)> {
        match func {
            AggregateFunc::Count => {
                // COUNT(*) and COUNT(expr) return BIGINT, never NULL.
                Ok((RelationalType::Bigint, false))
            }
            AggregateFunc::Sum => {
                let ty = input_type.ok_or_else(|| SqlError::TypeMismatch {
                    detail: "SUM requires a typed numeric operand".to_string(),
                })?;
                let return_type = match ty {
                    RelationalType::Integer | RelationalType::Bigint => RelationalType::Bigint,
                    RelationalType::Real => RelationalType::Real,
                    RelationalType::Double => RelationalType::Double,
                    RelationalType::Decimal { scale } => RelationalType::Decimal { scale },
                    _ => {
                        return Err(SqlError::TypeMismatch {
                            detail: format!("SUM requires a numeric operand, found {ty:?}"),
                        })
                    }
                };
                // SUM over 0 qualifying rows or all NULL rows returns NULL.
                Ok((return_type, true))
            }
            AggregateFunc::Avg => {
                let ty = input_type.ok_or_else(|| SqlError::TypeMismatch {
                    detail: "AVG requires a typed numeric operand".to_string(),
                })?;
                let return_type = match ty {
                    RelationalType::Integer
                    | RelationalType::Bigint
                    | RelationalType::Double => RelationalType::Double,
                    RelationalType::Real => RelationalType::Real,
                    RelationalType::Decimal { .. } => {
                        return Err(SqlError::Unsupported {
                            detail: "AVG(DECIMAL) is unsupported because DECIMAL division requires scale rescaling not present in RelationalValue".to_string(),
                        })
                    }
                    _ => {
                        return Err(SqlError::TypeMismatch {
                            detail: format!("AVG requires a numeric operand, found {ty:?}"),
                        })
                    }
                };
                // AVG over 0 qualifying rows returns NULL.
                Ok((return_type, true))
            }
            AggregateFunc::Min | AggregateFunc::Max => {
                let ty = input_type.ok_or_else(|| SqlError::TypeMismatch {
                    detail: format!("{} requires a typed operand", func.name()),
                })?;
                if is_wildcard {
                    return Err(SqlError::TypeMismatch {
                        detail: format!("{} does not support wildcard argument (*)", func.name()),
                    });
                }
                // MIN/MAX returns the same type as input, NULL if 0 rows.
                Ok((ty, true))
            }
        }
    }
}

fn is_numeric(ty: RelationalType) -> bool {
    matches!(
        ty,
        RelationalType::Integer
            | RelationalType::Bigint
            | RelationalType::Real
            | RelationalType::Double
            | RelationalType::Decimal { .. }
    )
}

// =======================================================================
// Canonical Grouping Key (Items 13, 14, 15)
// =======================================================================

/// One grouping value in canonical, hashable, and equality-preserving form.
///
/// Preserves exact SQL grouping semantics:
/// - `NULL` compares equal to `NULL` (single group for NULLs).
/// - `-0.0` and `+0.0` map to the identical bit pattern (`0.0f32.to_bits()`).
/// - All `NaN`s map to the identical canonical NaN bits (`f32::NAN.to_bits()`).
/// - Infinite values are preserved and compared correctly.
/// - Composite groups cannot collide (each field is a separate enum variant).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum GroupingValue {
    Null,
    Boolean(bool),
    Integer(i32),
    Bigint(i64),
    Real(u32),
    Double(u64),
    Decimal(i128, u8),
    Text(String),
    Blob(Vec<u8>),
    Date(i32),
    Time(i64),
    Timestamp(i64),
}

impl GroupingValue {
    pub fn from_optional_value(val: Option<&RelationalValue>) -> Self {
        match val {
            None => GroupingValue::Null,
            Some(v) => match v {
                RelationalValue::Boolean(b) => GroupingValue::Boolean(*b),
                RelationalValue::Integer(i) => GroupingValue::Integer(*i),
                RelationalValue::Bigint(b) => GroupingValue::Bigint(*b),
                RelationalValue::Real(f) => {
                    let bits = if f.is_nan() {
                        f32::NAN.to_bits()
                    } else if *f == 0.0 {
                        0.0f32.to_bits()
                    } else {
                        f.to_bits()
                    };
                    GroupingValue::Real(bits)
                }
                RelationalValue::Double(d) => {
                    let bits = if d.is_nan() {
                        f64::NAN.to_bits()
                    } else if *d == 0.0 {
                        0.0f64.to_bits()
                    } else {
                        d.to_bits()
                    };
                    GroupingValue::Double(bits)
                }
                RelationalValue::Decimal(v, s) => GroupingValue::Decimal(*v, *s),
                RelationalValue::Text(s) => GroupingValue::Text(s.clone()),
                RelationalValue::Blob(b) => GroupingValue::Blob(b.clone()),
                RelationalValue::Date(d) => GroupingValue::Date(*d),
                RelationalValue::Time(t) => GroupingValue::Time(*t),
                RelationalValue::Timestamp(ts) => GroupingValue::Timestamp(*ts),
            },
        }
    }

    pub fn to_optional_value(&self) -> Option<RelationalValue> {
        match self {
            GroupingValue::Null => None,
            GroupingValue::Boolean(b) => Some(RelationalValue::Boolean(*b)),
            GroupingValue::Integer(i) => Some(RelationalValue::Integer(*i)),
            GroupingValue::Bigint(b) => Some(RelationalValue::Bigint(*b)),
            GroupingValue::Real(bits) => Some(RelationalValue::Real(f32::from_bits(*bits))),
            GroupingValue::Double(bits) => Some(RelationalValue::Double(f64::from_bits(*bits))),
            GroupingValue::Decimal(v, s) => Some(RelationalValue::Decimal(*v, *s)),
            GroupingValue::Text(s) => Some(RelationalValue::Text(s.clone())),
            GroupingValue::Blob(b) => Some(RelationalValue::Blob(b.clone())),
            GroupingValue::Date(d) => Some(RelationalValue::Date(*d)),
            GroupingValue::Time(t) => Some(RelationalValue::Time(*t)),
            GroupingValue::Timestamp(ts) => Some(RelationalValue::Timestamp(*ts)),
        }
    }

    pub fn estimated_bytes(&self) -> usize {
        match self {
            GroupingValue::Null => 1,
            GroupingValue::Boolean(_) => 2,
            GroupingValue::Integer(_) => 5,
            GroupingValue::Bigint(_) => 9,
            GroupingValue::Real(_) => 5,
            GroupingValue::Double(_) => 9,
            GroupingValue::Decimal(_, _) => 18,
            GroupingValue::Text(s) => 16 + s.len(),
            GroupingValue::Blob(b) => 16 + b.len(),
            GroupingValue::Date(_) => 5,
            GroupingValue::Time(_) => 9,
            GroupingValue::Timestamp(_) => 9,
        }
    }
}

/// A composite grouping key containing canonical values for each GROUP BY expression.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct GroupingKey(pub Vec<GroupingValue>);

impl GroupingKey {
    pub fn from_values(values: &[Option<RelationalValue>]) -> Self {
        GroupingKey(
            values
                .iter()
                .map(|v| GroupingValue::from_optional_value(v.as_ref()))
                .collect(),
        )
    }

    pub fn to_values(&self) -> Vec<Option<RelationalValue>> {
        self.0
            .iter()
            .map(GroupingValue::to_optional_value)
            .collect()
    }

    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .0
                .iter()
                .map(GroupingValue::estimated_bytes)
                .sum::<usize>()
    }
}

// =======================================================================
// Aggregate State Representation (Items 6, 7, 8, 9, 10, 41, 42, 43)
// =======================================================================

#[derive(Clone, Debug, PartialEq)]
pub enum AggregateState {
    CountStar {
        count: u64,
    },
    CountExpr {
        count: u64,
    },
    SumInt {
        sum: i64,
        seen_non_null: bool,
    },
    SumReal {
        sum: f32,
        seen_non_null: bool,
    },
    SumDouble {
        sum: f64,
        seen_non_null: bool,
    },
    SumDecimal {
        sum: i128,
        scale: u8,
        seen_non_null: bool,
    },
    AvgDouble {
        sum: f64,
        count: u64,
    },
    AvgReal {
        sum: f32,
        count: u64,
    },
    Min {
        min: Option<RelationalValue>,
    },
    Max {
        max: Option<RelationalValue>,
    },
}

impl AggregateState {
    pub fn initial(
        func: AggregateFunc,
        is_wildcard: bool,
        input_type: Option<RelationalType>,
    ) -> Result<Self> {
        if is_wildcard {
            if func == AggregateFunc::Count {
                return Ok(AggregateState::CountStar { count: 0 });
            }
            return Err(SqlError::TypeMismatch {
                detail: format!("{} does not support wildcard argument (*)", func.name()),
            });
        }

        match func {
            AggregateFunc::Count => Ok(AggregateState::CountExpr { count: 0 }),
            AggregateFunc::Sum => {
                let ty = input_type.ok_or_else(|| SqlError::TypeMismatch {
                    detail: "SUM requires typed argument".to_string(),
                })?;
                match ty {
                    RelationalType::Integer | RelationalType::Bigint => {
                        Ok(AggregateState::SumInt {
                            sum: 0,
                            seen_non_null: false,
                        })
                    }
                    RelationalType::Real => Ok(AggregateState::SumReal {
                        sum: 0.0,
                        seen_non_null: false,
                    }),
                    RelationalType::Double => Ok(AggregateState::SumDouble {
                        sum: 0.0,
                        seen_non_null: false,
                    }),
                    RelationalType::Decimal { scale } => Ok(AggregateState::SumDecimal {
                        sum: 0,
                        scale,
                        seen_non_null: false,
                    }),
                    _ => Err(SqlError::TypeMismatch {
                        detail: format!("SUM does not support {ty:?}"),
                    }),
                }
            }
            AggregateFunc::Avg => {
                let ty = input_type.ok_or_else(|| SqlError::TypeMismatch {
                    detail: "AVG requires typed argument".to_string(),
                })?;
                match ty {
                    RelationalType::Real => Ok(AggregateState::AvgReal {
                        sum: 0.0,
                        count: 0,
                    }),
                    RelationalType::Integer
                    | RelationalType::Bigint
                    | RelationalType::Double => Ok(AggregateState::AvgDouble {
                        sum: 0.0,
                        count: 0,
                    }),
                    RelationalType::Decimal { .. } => Err(SqlError::Unsupported {
                        detail: "AVG(DECIMAL) is unsupported because DECIMAL division requires scale rescaling not present in RelationalValue".to_string(),
                    }),
                    _ => Err(SqlError::TypeMismatch {
                        detail: format!("AVG does not support {ty:?}"),
                    }),
                }
            }
            AggregateFunc::Min => Ok(AggregateState::Min { min: None }),
            AggregateFunc::Max => Ok(AggregateState::Max { max: None }),
        }
    }

    /// Accumulates one row's value into the aggregate state.
    pub fn update(&mut self, val: Option<&RelationalValue>) -> Result<()> {
        match self {
            AggregateState::CountStar { count } => {
                // COUNT(*) counts every qualifying row, even if row fields are NULL.
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| SqlError::ResourceLimit {
                        detail: "COUNT(*) exceeded u64 capacity".to_string(),
                    })?;
                Ok(())
            }
            AggregateState::CountExpr { count } => {
                // COUNT(expr) ignores NULL values.
                if val.is_some() {
                    *count = count
                        .checked_add(1)
                        .ok_or_else(|| SqlError::ResourceLimit {
                            detail: "COUNT(expr) exceeded u64 capacity".to_string(),
                        })?;
                }
                Ok(())
            }
            AggregateState::SumInt { sum, seen_non_null } => {
                let Some(val) = val else { return Ok(()) };
                *seen_non_null = true;
                let addend = match val {
                    RelationalValue::Integer(i) => *i as i64,
                    RelationalValue::Bigint(b) => *b,
                    _ => {
                        return Err(SqlError::ExecutionParameter {
                            detail: "SUM(integer/bigint) received non-integer value".to_string(),
                        })
                    }
                };
                *sum = sum
                    .checked_add(addend)
                    .ok_or_else(|| SqlError::ExecutionParameter {
                        detail: "arithmetic overflow in SUM".to_string(),
                    })?;
                Ok(())
            }
            AggregateState::SumReal { sum, seen_non_null } => {
                let Some(val) = val else { return Ok(()) };
                *seen_non_null = true;
                let addend = match val {
                    RelationalValue::Real(r) => *r,
                    _ => {
                        return Err(SqlError::ExecutionParameter {
                            detail: "SUM(real) received non-real value".to_string(),
                        })
                    }
                };
                *sum += addend;
                Ok(())
            }
            AggregateState::SumDouble { sum, seen_non_null } => {
                let Some(val) = val else { return Ok(()) };
                *seen_non_null = true;
                let addend = match val {
                    RelationalValue::Double(d) => *d,
                    _ => {
                        return Err(SqlError::ExecutionParameter {
                            detail: "SUM(double) received non-double value".to_string(),
                        })
                    }
                };
                *sum += addend;
                Ok(())
            }
            AggregateState::SumDecimal {
                sum,
                scale,
                seen_non_null,
            } => {
                let Some(val) = val else { return Ok(()) };
                *seen_non_null = true;
                let addend = match val {
                    RelationalValue::Decimal(d, s) if *s == *scale => *d,
                    _ => {
                        return Err(SqlError::ExecutionParameter {
                            detail: "SUM(decimal) received mismatched decimal scale".to_string(),
                        })
                    }
                };
                let new_sum =
                    sum.checked_add(addend)
                        .ok_or_else(|| SqlError::ExecutionParameter {
                            detail: "arithmetic overflow in SUM(DECIMAL)".to_string(),
                        })?;
                validate_decimal(new_sum, MAX_DECIMAL_PRECISION, *scale).map_err(|e| match e {
                    RelationalError::InvalidInput { detail } => {
                        SqlError::ExecutionParameter { detail }
                    }
                    other => SqlError::Storage(other.to_string()),
                })?;
                *sum = new_sum;
                Ok(())
            }
            AggregateState::AvgDouble { sum, count } => {
                let Some(val) = val else { return Ok(()) };
                let addend = match val {
                    RelationalValue::Integer(i) => *i as f64,
                    RelationalValue::Bigint(b) => *b as f64,
                    RelationalValue::Double(d) => *d,
                    _ => {
                        return Err(SqlError::ExecutionParameter {
                            detail: "AVG(double) received non-numeric value".to_string(),
                        })
                    }
                };
                *sum += addend;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| SqlError::ResourceLimit {
                        detail: "AVG count exceeded u64 capacity".to_string(),
                    })?;
                Ok(())
            }
            AggregateState::AvgReal { sum, count } => {
                let Some(val) = val else { return Ok(()) };
                let addend = match val {
                    RelationalValue::Real(r) => *r,
                    _ => {
                        return Err(SqlError::ExecutionParameter {
                            detail: "AVG(real) received non-real value".to_string(),
                        })
                    }
                };
                *sum += addend;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| SqlError::ResourceLimit {
                        detail: "AVG count exceeded u64 capacity".to_string(),
                    })?;
                Ok(())
            }
            AggregateState::Min { min } => {
                let Some(val) = val else { return Ok(()) };
                match min {
                    None => *min = Some(val.clone()),
                    Some(current) => {
                        if order_value(val, current)? == std::cmp::Ordering::Less {
                            *min = Some(val.clone());
                        }
                    }
                }
                Ok(())
            }
            AggregateState::Max { max } => {
                let Some(val) = val else { return Ok(()) };
                match max {
                    None => *max = Some(val.clone()),
                    Some(current) => {
                        if order_value(val, current)? == std::cmp::Ordering::Greater {
                            *max = Some(val.clone());
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// Finalizes the aggregate state into its client-visible `RelationalValue`.
    pub fn finalize(self) -> Option<RelationalValue> {
        match self {
            AggregateState::CountStar { count } => Some(RelationalValue::Bigint(count as i64)),
            AggregateState::CountExpr { count } => Some(RelationalValue::Bigint(count as i64)),
            AggregateState::SumInt { sum, seen_non_null } => {
                if seen_non_null {
                    Some(RelationalValue::Bigint(sum))
                } else {
                    None
                }
            }
            AggregateState::SumReal { sum, seen_non_null } => {
                if seen_non_null {
                    Some(RelationalValue::Real(sum))
                } else {
                    None
                }
            }
            AggregateState::SumDouble { sum, seen_non_null } => {
                if seen_non_null {
                    Some(RelationalValue::Double(sum))
                } else {
                    None
                }
            }
            AggregateState::SumDecimal {
                sum,
                scale,
                seen_non_null,
            } => {
                if seen_non_null {
                    Some(RelationalValue::Decimal(sum, scale))
                } else {
                    None
                }
            }
            AggregateState::AvgDouble { sum, count } => {
                if count == 0 {
                    None
                } else {
                    Some(RelationalValue::Double(sum / count as f64))
                }
            }
            AggregateState::AvgReal { sum, count } => {
                if count == 0 {
                    None
                } else {
                    Some(RelationalValue::Real(sum / count as f32))
                }
            }
            AggregateState::Min { min } => min,
            AggregateState::Max { max } => max,
        }
    }

    /// Merges another aggregate state into this one (for parallel or partitioned aggregation).
    pub fn merge(&mut self, other: Self) -> Result<()> {
        match (self, other) {
            (AggregateState::CountStar { count: c1 }, AggregateState::CountStar { count: c2 }) => {
                *c1 = c1.checked_add(c2).ok_or_else(|| SqlError::ResourceLimit {
                    detail: "COUNT(*) merge overflow".to_string(),
                })?;
                Ok(())
            }
            (AggregateState::CountExpr { count: c1 }, AggregateState::CountExpr { count: c2 }) => {
                *c1 = c1.checked_add(c2).ok_or_else(|| SqlError::ResourceLimit {
                    detail: "COUNT(expr) merge overflow".to_string(),
                })?;
                Ok(())
            }
            (
                AggregateState::SumInt {
                    sum: s1,
                    seen_non_null: n1,
                },
                AggregateState::SumInt {
                    sum: s2,
                    seen_non_null: n2,
                },
            ) => {
                if n2 {
                    *s1 = s1
                        .checked_add(s2)
                        .ok_or_else(|| SqlError::ExecutionParameter {
                            detail: "arithmetic overflow in SUM merge".to_string(),
                        })?;
                    *n1 = true;
                }
                Ok(())
            }
            (
                AggregateState::SumReal {
                    sum: s1,
                    seen_non_null: n1,
                },
                AggregateState::SumReal {
                    sum: s2,
                    seen_non_null: n2,
                },
            ) => {
                if n2 {
                    *s1 += s2;
                    *n1 = true;
                }
                Ok(())
            }
            (
                AggregateState::SumDouble {
                    sum: s1,
                    seen_non_null: n1,
                },
                AggregateState::SumDouble {
                    sum: s2,
                    seen_non_null: n2,
                },
            ) => {
                if n2 {
                    *s1 += s2;
                    *n1 = true;
                }
                Ok(())
            }
            (
                AggregateState::SumDecimal {
                    sum: s1,
                    scale: sc1,
                    seen_non_null: n1,
                },
                AggregateState::SumDecimal {
                    sum: s2,
                    scale: sc2,
                    seen_non_null: n2,
                },
            ) => {
                if *sc1 != sc2 {
                    return Err(SqlError::ExecutionParameter {
                        detail: "mismatched decimal scale in SUM merge".to_string(),
                    });
                }
                if n2 {
                    let new_sum =
                        s1.checked_add(s2)
                            .ok_or_else(|| SqlError::ExecutionParameter {
                                detail: "arithmetic overflow in SUM(DECIMAL) merge".to_string(),
                            })?;
                    validate_decimal(new_sum, MAX_DECIMAL_PRECISION, *sc1).map_err(
                        |e| match e {
                            RelationalError::InvalidInput { detail } => {
                                SqlError::ExecutionParameter { detail }
                            }
                            other => SqlError::Storage(other.to_string()),
                        },
                    )?;
                    *s1 = new_sum;
                    *n1 = true;
                }
                Ok(())
            }
            (
                AggregateState::AvgDouble { sum: s1, count: c1 },
                AggregateState::AvgDouble { sum: s2, count: c2 },
            ) => {
                *s1 += s2;
                *c1 = c1.checked_add(c2).ok_or_else(|| SqlError::ResourceLimit {
                    detail: "AVG merge count overflow".to_string(),
                })?;
                Ok(())
            }
            (
                AggregateState::AvgReal { sum: s1, count: c1 },
                AggregateState::AvgReal { sum: s2, count: c2 },
            ) => {
                *s1 += s2;
                *c1 = c1.checked_add(c2).ok_or_else(|| SqlError::ResourceLimit {
                    detail: "AVG merge count overflow".to_string(),
                })?;
                Ok(())
            }
            (AggregateState::Min { min: m1 }, AggregateState::Min { min: m2 }) => {
                if let Some(v2) = m2 {
                    match m1 {
                        None => *m1 = Some(v2),
                        Some(v1) => {
                            if order_value(&v2, v1)? == std::cmp::Ordering::Less {
                                *m1 = Some(v2);
                            }
                        }
                    }
                }
                Ok(())
            }
            (AggregateState::Max { max: m1 }, AggregateState::Max { max: m2 }) => {
                if let Some(v2) = m2 {
                    match m1 {
                        None => *m1 = Some(v2),
                        Some(v1) => {
                            if order_value(&v2, v1)? == std::cmp::Ordering::Greater {
                                *m1 = Some(v2);
                            }
                        }
                    }
                }
                Ok(())
            }
            _ => Err(SqlError::ExecutionParameter {
                detail: "cannot merge mismatched aggregate states".to_string(),
            }),
        }
    }

    pub fn estimated_bytes(&self) -> usize {
        match self {
            AggregateState::CountStar { .. } | AggregateState::CountExpr { .. } => 8,
            AggregateState::SumInt { .. } => 9,
            AggregateState::SumReal { .. } => 5,
            AggregateState::SumDouble { .. } => 9,
            AggregateState::SumDecimal { .. } => 18,
            AggregateState::AvgDouble { .. } => 16,
            AggregateState::AvgReal { .. } => 12,
            AggregateState::Min { min } | AggregateState::Max { max: min } => match min {
                None => 8,
                Some(RelationalValue::Text(s)) => 24 + s.len(),
                Some(RelationalValue::Blob(b)) => 24 + b.len(),
                Some(_) => 24,
            },
        }
    }
}

fn order_value(a: &RelationalValue, b: &RelationalValue) -> Result<std::cmp::Ordering> {
    use RelationalValue::*;
    match (a, b) {
        (Boolean(x), Boolean(y)) => Ok(x.cmp(y)),
        (Integer(x), Integer(y)) => Ok(x.cmp(y)),
        (Bigint(x), Bigint(y)) => Ok(x.cmp(y)),
        (Real(x), Real(y)) => x
            .partial_cmp(y)
            .ok_or_else(|| SqlError::ExecutionParameter {
                detail: "NaN is not orderable in MIN/MAX".to_string(),
            }),
        (Double(x), Double(y)) => x
            .partial_cmp(y)
            .ok_or_else(|| SqlError::ExecutionParameter {
                detail: "NaN is not orderable in MIN/MAX".to_string(),
            }),
        (Decimal(x, sx), Decimal(y, sy)) if sx == sy => Ok(x.cmp(y)),
        (Text(x), Text(y)) => Ok(x.cmp(y)),
        (Blob(x), Blob(y)) => Ok(x.cmp(y)),
        (Date(x), Date(y)) => Ok(x.cmp(y)),
        (Time(x), Time(y)) => Ok(x.cmp(y)),
        (Timestamp(x), Timestamp(y)) => Ok(x.cmp(y)),
        _ => Err(SqlError::ExecutionParameter {
            detail: "type mismatch in MIN/MAX comparison".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_semantics_empty_zero_and_null() {
        let mut state_star = AggregateState::initial(AggregateFunc::Count, true, None).unwrap();
        let mut state_expr =
            AggregateState::initial(AggregateFunc::Count, false, Some(RelationalType::Integer))
                .unwrap();

        assert_eq!(
            state_star.clone().finalize(),
            Some(RelationalValue::Bigint(0))
        );
        assert_eq!(
            state_expr.clone().finalize(),
            Some(RelationalValue::Bigint(0))
        );

        state_star.update(None).unwrap();
        state_expr.update(None).unwrap();

        assert_eq!(
            state_star.clone().finalize(),
            Some(RelationalValue::Bigint(1))
        );
        assert_eq!(
            state_expr.clone().finalize(),
            Some(RelationalValue::Bigint(0))
        );

        state_star
            .update(Some(&RelationalValue::Integer(42)))
            .unwrap();
        state_expr
            .update(Some(&RelationalValue::Integer(42)))
            .unwrap();

        assert_eq!(state_star.finalize(), Some(RelationalValue::Bigint(2)));
        assert_eq!(state_expr.finalize(), Some(RelationalValue::Bigint(1)));
    }

    #[test]
    fn sum_semantics_empty_null_overflow() {
        let mut sum_state =
            AggregateState::initial(AggregateFunc::Sum, false, Some(RelationalType::Integer))
                .unwrap();
        assert_eq!(sum_state.clone().finalize(), None);

        sum_state.update(None).unwrap();
        assert_eq!(sum_state.clone().finalize(), None);

        sum_state
            .update(Some(&RelationalValue::Integer(10)))
            .unwrap();
        sum_state
            .update(Some(&RelationalValue::Integer(20)))
            .unwrap();
        assert_eq!(
            sum_state.clone().finalize(),
            Some(RelationalValue::Bigint(30))
        );

        let mut overflow_state =
            AggregateState::initial(AggregateFunc::Sum, false, Some(RelationalType::Bigint))
                .unwrap();
        overflow_state
            .update(Some(&RelationalValue::Bigint(i64::MAX)))
            .unwrap();
        assert!(overflow_state
            .update(Some(&RelationalValue::Bigint(1)))
            .is_err());
    }

    #[test]
    fn avg_semantics_empty_null_and_precision() {
        let mut avg_state =
            AggregateState::initial(AggregateFunc::Avg, false, Some(RelationalType::Integer))
                .unwrap();
        assert_eq!(avg_state.clone().finalize(), None);

        avg_state.update(None).unwrap();
        assert_eq!(avg_state.clone().finalize(), None);

        avg_state
            .update(Some(&RelationalValue::Integer(10)))
            .unwrap();
        avg_state
            .update(Some(&RelationalValue::Integer(25)))
            .unwrap();
        assert_eq!(avg_state.finalize(), Some(RelationalValue::Double(17.5)));
    }

    #[test]
    fn min_max_semantics() {
        let mut min_state =
            AggregateState::initial(AggregateFunc::Min, false, Some(RelationalType::Text)).unwrap();
        let mut max_state =
            AggregateState::initial(AggregateFunc::Max, false, Some(RelationalType::Text)).unwrap();

        assert_eq!(min_state.clone().finalize(), None);
        assert_eq!(max_state.clone().finalize(), None);

        min_state.update(None).unwrap();
        max_state.update(None).unwrap();

        min_state
            .update(Some(&RelationalValue::Text("banana".to_string())))
            .unwrap();
        min_state
            .update(Some(&RelationalValue::Text("apple".to_string())))
            .unwrap();
        min_state
            .update(Some(&RelationalValue::Text("cherry".to_string())))
            .unwrap();

        max_state
            .update(Some(&RelationalValue::Text("banana".to_string())))
            .unwrap();
        max_state
            .update(Some(&RelationalValue::Text("apple".to_string())))
            .unwrap();
        max_state
            .update(Some(&RelationalValue::Text("cherry".to_string())))
            .unwrap();

        assert_eq!(
            min_state.finalize(),
            Some(RelationalValue::Text("apple".to_string()))
        );
        assert_eq!(
            max_state.finalize(),
            Some(RelationalValue::Text("cherry".to_string()))
        );
    }

    #[test]
    fn float_grouping_canonical_zero_and_nan() {
        let neg_zero = GroupingValue::from_optional_value(Some(&RelationalValue::Real(-0.0)));
        let pos_zero = GroupingValue::from_optional_value(Some(&RelationalValue::Real(0.0)));
        assert_eq!(neg_zero, pos_zero);

        let nan1 = GroupingValue::from_optional_value(Some(&RelationalValue::Double(f64::NAN)));
        let nan2 = GroupingValue::from_optional_value(Some(&RelationalValue::Double(-f64::NAN)));
        assert_eq!(nan1, nan2);
    }

    #[test]
    fn composite_grouping_keys_do_not_collide() {
        let key1 = GroupingKey::from_values(&[
            Some(RelationalValue::Integer(1)),
            Some(RelationalValue::Integer(23)),
        ]);
        let key2 = GroupingKey::from_values(&[
            Some(RelationalValue::Integer(12)),
            Some(RelationalValue::Integer(3)),
        ]);
        assert_ne!(key1, key2);

        let null_key1 = GroupingKey::from_values(&[None, Some(RelationalValue::Integer(1))]);
        let null_key2 = GroupingKey::from_values(&[None, Some(RelationalValue::Integer(1))]);
        assert_eq!(null_key1, null_key2);
    }
}
