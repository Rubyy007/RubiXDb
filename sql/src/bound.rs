//! The bound/resolved internal representation — `crate::bind`'s output.
//! Every identifier here is an authoritative catalog reference (item 14:
//! "the future planner must NOT need to repeat name resolution") and
//! every object reference has already passed authorization (item 15).
//! No variant here is executed by this crate.

use rubixdb::catalog::schema::{FkAction, IndexKind};
use rubixdb::catalog::service::NewForeignKey;
use rubixdb::relational::{RelationalType, RelationalValue};

use crate::ast::{BinaryOp, JoinKind, UnaryOp};

/// A stable index into a statement's own FROM-scope (`BoundSelect::
/// from`/`BoundUpdate::table`/`BoundDelete::table`'s position) — never a
/// name a later pass would need to re-resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableRefId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundTableRef {
    pub id: TableRefId,
    pub database_id: u32,
    pub schema_id: u32,
    pub table_id: u32,
    /// The name a column reference must qualify against within this
    /// statement (the alias if one was given, else the table's own
    /// catalog name — already case-folded, `crate::convert`).
    pub effective_name: String,
    /// `true` for the introduced (right-hand) side of a `LEFT JOIN` —
    /// every column from this table binds as `nullable: true` regardless
    /// of its own declared `NOT NULL` (D18: unmatched left rows produce
    /// `NULL` on this side).
    pub null_extended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnRef {
    pub table_ref: TableRefId,
    pub table_id: u32,
    pub ordinal: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundExpr {
    pub kind: BoundExprKind,
    /// `None` only for an untyped `NULL` literal with no narrowing
    /// context (item 21) — every other expression has a concrete,
    /// resolved `RelationalType`.
    pub ty: Option<RelationalType>,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundExprKind {
    /// `None` = `NULL` (of `BoundExpr::ty`'s type, if narrowed; fully
    /// untyped otherwise).
    Literal(Option<RelationalValue>),
    Parameter {
        index: u32,
    },
    Column(ColumnRef),
    UnaryOp {
        op: UnaryOp,
        expr: Box<BoundExpr>,
    },
    BinaryOp {
        left: Box<BoundExpr>,
        op: BinaryOp,
        right: Box<BoundExpr>,
    },
    IsNull {
        expr: Box<BoundExpr>,
        negated: bool,
    },
    Between {
        expr: Box<BoundExpr>,
        negated: bool,
        low: Box<BoundExpr>,
        high: Box<BoundExpr>,
    },
    InList {
        expr: Box<BoundExpr>,
        list: Vec<BoundExpr>,
        negated: bool,
    },
    Like {
        expr: Box<BoundExpr>,
        pattern: Box<BoundExpr>,
        negated: bool,
        case_insensitive: bool,
    },
    Case {
        operand: Option<Box<BoundExpr>>,
        branches: Vec<(BoundExpr, BoundExpr)>,
        else_result: Option<Box<BoundExpr>>,
    },
    Function {
        name: &'static str,
        args: Vec<BoundExpr>,
    },
    /// An aggregate expression before plan generation.
    Aggregate(BoundAggregateExpr),
    /// A reference to the output of an Aggregate plan operator (0-based index into aggregates).
    AggregateRef(usize),
    /// SQL-mission-2: a window function call before plan generation. The
    /// binder extracts every call out of the SELECT list / `QUALIFY` /
    /// `ORDER BY` into `BoundSelect::windows`, leaving a `WindowRef`.
    Window(Box<BoundWindowCall>),
    /// SQL-mission-2: the value a `Window` plan operator computed for the
    /// current row (0-based index into `BoundSelect::windows`).
    WindowRef(usize),
    /// SQL-mission-2: the value of grouping expression `group_by[i]` for the
    /// current output row of an aggregation that uses `GROUPING SETS` /
    /// `ROLLUP` / `CUBE` -- `NULL` when the row's grouping set leaves it out.
    GroupKeyRef(usize),
    /// SQL-mission-2: `GROUPING(e1, ...)`: each element is the index of a
    /// `group_by` expression; the value is the bit string (first argument in
    /// the most significant position) of "this row's grouping set does not
    /// include it".
    Grouping(Vec<usize>),
    /// SQL-mission-1: a scalar / `EXISTS` / `IN (SELECT ...)` subquery — one
    /// node for all three, so the planner and executor have a single
    /// nested-plan mechanism rather than one per syntax form.
    Subquery(Box<BoundSubqueryExpr>),
}

/// What a subquery expression asks of its inner query's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubqueryKind {
    /// `(SELECT x ...)`: zero rows -> `NULL`, one row -> its value, more than
    /// one row -> a runtime error (SQL standard; not a parse-time error).
    Scalar,
    /// `[NOT] EXISTS (SELECT ...)`: never `NULL`.
    Exists { negated: bool },
    /// `operand [NOT] IN (SELECT x ...)`: SQL three-valued logic.
    In { negated: bool },
    /// `operand <op> ANY|SOME|ALL (SELECT x ...)`: `ANY` is the three-valued
    /// disjunction of `operand <op> x` over the rows, `ALL` the conjunction
    /// (an empty set gives `FALSE` for `ANY`, `TRUE` for `ALL`, whatever the
    /// operand is).
    Quantified { op: crate::ast::BinaryOp, all: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundSubqueryExpr {
    /// Statement-unique id (binder-allocated). Executor-side caches of an
    /// uncorrelated subquery's result are keyed by it.
    pub id: u32,
    pub kind: SubqueryKind,
    /// The left-hand expression of `IN`; `None` for `Scalar`/`Exists`.
    pub operand: Option<BoundExpr>,
    pub query: BoundSelect,
    /// Outer-query columns this subquery (at any nesting depth) reads. Empty
    /// means uncorrelated: its result is the same for every outer row.
    pub outer_refs: Vec<ColumnRef>,
    /// `alias.column` labels for the outer row this subquery is evaluated
    /// for: every `outer_refs` column plus the primary-key columns of the
    /// outer tables they belong to. Used only to say *which* outer row
    /// triggered a runtime error (a scalar subquery returning several rows);
    /// never part of evaluation.
    pub outer_labels: Vec<(String, ColumnRef)>,
    /// The physical plan of `query`, filled in by the planner
    /// (`crate::plan`) -- `None` between bind and plan. The executor refuses
    /// a subquery whose plan is `None`.
    pub plan: Option<crate::plan::physical::PhysicalPlan>,
}

pub type BoundAggregateArg = crate::aggregate::AggregateArg<Box<BoundExpr>>;

#[derive(Debug, Clone, PartialEq)]
pub struct BoundAggregateExpr {
    pub func: crate::aggregate::AggregateFunc,
    pub arg: BoundAggregateArg,
    /// SQL-mission-2: `AGG(DISTINCT x)` -- each distinct non-`NULL` value of
    /// the argument is accumulated once per group.
    pub distinct: bool,
    /// SQL-mission-2: `AGG(x) FILTER (WHERE cond)` -- only rows for which the
    /// condition is `TRUE` reach this aggregate's state.
    pub filter: Option<Box<BoundExpr>>,
}

impl BoundAggregateExpr {
    /// A plain aggregate: no `DISTINCT`, no `FILTER`.
    pub fn plain(func: crate::aggregate::AggregateFunc, arg: BoundAggregateArg) -> Self {
        BoundAggregateExpr {
            func,
            arg,
            distinct: false,
            filter: None,
        }
    }
}

// =======================================================================
// SQL-mission-2: window functions
// =======================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum BoundFrameBound {
    UnboundedPreceding,
    /// `N PRECEDING`: a constant, non-negative offset (checked at execution
    /// when it is a parameter).
    Preceding(Box<BoundExpr>),
    CurrentRow,
    Following(Box<BoundExpr>),
    UnboundedFollowing,
}

/// A window frame, always concrete: when the query wrote none the binder
/// stores the default (`RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`
/// with an `ORDER BY`, `... AND UNBOUNDED FOLLOWING` without).
#[derive(Debug, Clone, PartialEq)]
pub struct BoundFrame {
    pub units: crate::ast::FrameUnits,
    pub start: BoundFrameBound,
    pub end: BoundFrameBound,
    /// The query wrote the frame (display only; two windows that differ only
    /// in this flag are the same window).
    pub explicit: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundWindowSpec {
    pub partition_by: Vec<BoundExpr>,
    pub order_by: Vec<BoundOrderByItem>,
    pub frame: BoundFrame,
}

impl BoundWindowSpec {
    /// `true` iff both specs are provably the same window: the same
    /// `PARTITION BY` expressions, the same `ORDER BY` items and the same
    /// (resolved) frame. Only such windows may share one `Window` operator.
    pub fn same_window(&self, other: &BoundWindowSpec) -> bool {
        self.partition_by == other.partition_by
            && self.order_by == other.order_by
            && self.frame.units == other.frame.units
            && self.frame.start == other.frame.start
            && self.frame.end == other.frame.end
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundWindowFunc {
    /// An aggregate evaluated over the window frame.
    Aggregate {
        func: crate::aggregate::AggregateFunc,
        arg: BoundAggregateArg,
        /// `AGG(DISTINCT x) OVER (...)`: each distinct value of the frame
        /// counts once (the frame's own rows, not the partition's).
        distinct: bool,
        filter: Option<Box<BoundExpr>>,
    },
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile(Box<BoundExpr>),
    /// `ignore_nulls`: `IGNORE NULLS` -- rows whose value is `NULL` are skipped
    /// when counting `offset` rows back (`LAG`) or forward (`LEAD`).
    Lag {
        expr: Box<BoundExpr>,
        offset: Option<Box<BoundExpr>>,
        default: Option<Box<BoundExpr>>,
        ignore_nulls: bool,
    },
    Lead {
        expr: Box<BoundExpr>,
        offset: Option<Box<BoundExpr>>,
        default: Option<Box<BoundExpr>>,
        ignore_nulls: bool,
    },
    /// `FIRST_VALUE(expr)`: `expr` at the first row of the **frame** (`NULL` for
    /// an empty frame). With `ignore_nulls`, the first row of the frame whose
    /// value is not `NULL`.
    FirstValue {
        expr: Box<BoundExpr>,
        ignore_nulls: bool,
    },
    /// `LAST_VALUE(expr)`: `expr` at the last row of the **frame**.
    ///
    /// **Not the partition's last row** unless the frame says so. With the
    /// default frame -- `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`, what
    /// a window with an `ORDER BY` and no frame means -- the frame ends at the
    /// current row's last *peer*, so `LAST_VALUE` is the current row's own value
    /// (the value of the last of its ties when there are ties). To get the
    /// partition's last value, write the frame:
    /// `ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING`. (Without an
    /// `ORDER BY` the default frame is the whole partition.) This is the
    /// standard's behaviour, not a quirk of this engine; tests pin all three
    /// cases (`window_tests::exec_2c::last_value_*`).
    LastValue {
        expr: Box<BoundExpr>,
        ignore_nulls: bool,
    },
    /// `NTH_VALUE(expr, n)`: `expr` at the n-th row (1-based) of the **frame**;
    /// `NULL` when the frame has fewer than `n` rows. With `ignore_nulls` the
    /// n-th row whose value is not `NULL`.
    NthValue {
        expr: Box<BoundExpr>,
        n: Box<BoundExpr>,
        ignore_nulls: bool,
    },
}

impl BoundWindowFunc {
    pub fn name(&self) -> &'static str {
        match self {
            BoundWindowFunc::Aggregate { func, .. } => func.name(),
            BoundWindowFunc::RowNumber => "row_number",
            BoundWindowFunc::Rank => "rank",
            BoundWindowFunc::DenseRank => "dense_rank",
            BoundWindowFunc::PercentRank => "percent_rank",
            BoundWindowFunc::CumeDist => "cume_dist",
            BoundWindowFunc::Ntile(_) => "ntile",
            BoundWindowFunc::Lag { .. } => "lag",
            BoundWindowFunc::Lead { .. } => "lead",
            BoundWindowFunc::FirstValue { .. } => "first_value",
            BoundWindowFunc::LastValue { .. } => "last_value",
            BoundWindowFunc::NthValue { .. } => "nth_value",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundWindowCall {
    pub func: BoundWindowFunc,
    pub spec: BoundWindowSpec,
}

impl BoundWindowCall {
    /// Every expression the call evaluates per row: the function's arguments
    /// and filter, the `PARTITION BY` and `ORDER BY` expressions, the frame
    /// offsets.
    pub fn exprs(&self) -> Vec<&BoundExpr> {
        let mut out: Vec<&BoundExpr> = Vec::new();
        match &self.func {
            BoundWindowFunc::Aggregate { arg, filter, .. } => {
                if let BoundAggregateArg::Expr(e) = arg {
                    out.push(e);
                }
                if let Some(f) = filter {
                    out.push(f);
                }
            }
            BoundWindowFunc::RowNumber
            | BoundWindowFunc::Rank
            | BoundWindowFunc::DenseRank
            | BoundWindowFunc::PercentRank
            | BoundWindowFunc::CumeDist => {}
            BoundWindowFunc::Ntile(n) => out.push(n),
            BoundWindowFunc::Lag {
                expr,
                offset,
                default,
                ..
            }
            | BoundWindowFunc::Lead {
                expr,
                offset,
                default,
                ..
            } => {
                out.push(expr);
                out.extend(offset.iter().map(|b| b.as_ref()));
                out.extend(default.iter().map(|b| b.as_ref()));
            }
            BoundWindowFunc::FirstValue { expr: e, .. }
            | BoundWindowFunc::LastValue { expr: e, .. } => out.push(e),
            BoundWindowFunc::NthValue { expr, n, .. } => {
                out.push(expr);
                out.push(n);
            }
        }
        out.extend(self.spec.partition_by.iter());
        out.extend(self.spec.order_by.iter().map(|o| &o.expr));
        for bound in [&self.spec.frame.start, &self.spec.frame.end] {
            if let BoundFrameBound::Preceding(e) | BoundFrameBound::Following(e) = bound {
                out.push(e);
            }
        }
        out
    }

    /// Rebuilds the call with `f` applied to each of `exprs()`'s expressions.
    pub fn map_exprs(self, f: &mut dyn FnMut(BoundExpr) -> BoundExpr) -> BoundWindowCall {
        let mut m = |e: Box<BoundExpr>| Box::new(f(*e));
        let func = match self.func {
            BoundWindowFunc::Aggregate {
                func,
                arg,
                distinct,
                filter,
            } => BoundWindowFunc::Aggregate {
                func,
                distinct,
                arg: match arg {
                    BoundAggregateArg::Wildcard => BoundAggregateArg::Wildcard,
                    BoundAggregateArg::Expr(e) => BoundAggregateArg::Expr(m(e)),
                    BoundAggregateArg::Multi(v) => {
                        BoundAggregateArg::Multi(v.into_iter().map(&mut m).collect())
                    }
                },
                filter: filter.map(&mut m),
            },
            BoundWindowFunc::Ntile(n) => BoundWindowFunc::Ntile(m(n)),
            BoundWindowFunc::Lag {
                expr,
                offset,
                default,
                ignore_nulls,
            } => BoundWindowFunc::Lag {
                expr: m(expr),
                offset: offset.map(&mut m),
                default: default.map(&mut m),
                ignore_nulls,
            },
            BoundWindowFunc::Lead {
                expr,
                offset,
                default,
                ignore_nulls,
            } => BoundWindowFunc::Lead {
                expr: m(expr),
                offset: offset.map(&mut m),
                default: default.map(&mut m),
                ignore_nulls,
            },
            BoundWindowFunc::FirstValue { expr, ignore_nulls } => BoundWindowFunc::FirstValue {
                expr: m(expr),
                ignore_nulls,
            },
            BoundWindowFunc::LastValue { expr, ignore_nulls } => BoundWindowFunc::LastValue {
                expr: m(expr),
                ignore_nulls,
            },
            BoundWindowFunc::NthValue {
                expr,
                n,
                ignore_nulls,
            } => BoundWindowFunc::NthValue {
                expr: m(expr),
                n: m(n),
                ignore_nulls,
            },
            other => other,
        };
        let partition_by = self.spec.partition_by.into_iter().map(&mut *f).collect();
        let order_by = self
            .spec
            .order_by
            .into_iter()
            .map(|o| BoundOrderByItem {
                expr: f(o.expr),
                descending: o.descending,
                nulls: o.nulls,
            })
            .collect();
        let mut bound = |b: BoundFrameBound| match b {
            BoundFrameBound::Preceding(e) => BoundFrameBound::Preceding(Box::new(f(*e))),
            BoundFrameBound::Following(e) => BoundFrameBound::Following(Box::new(f(*e))),
            other => other,
        };
        let start = bound(self.spec.frame.start);
        let end = bound(self.spec.frame.end);
        BoundWindowCall {
            func,
            spec: BoundWindowSpec {
                partition_by,
                order_by,
                frame: BoundFrame {
                    units: self.spec.frame.units,
                    start,
                    end,
                    explicit: self.spec.frame.explicit,
                },
            },
        }
    }
}

impl BoundExpr {
    /// The direct sub-expressions of this node, for read-only walks. A
    /// subquery contributes only its `IN` operand (its query is a separate
    /// bound tree); a window call contributes everything it evaluates.
    pub fn children(&self) -> Vec<&BoundExpr> {
        match &self.kind {
            BoundExprKind::Literal(_)
            | BoundExprKind::Parameter { .. }
            | BoundExprKind::Column(_)
            | BoundExprKind::AggregateRef(_)
            | BoundExprKind::WindowRef(_)
            | BoundExprKind::GroupKeyRef(_)
            | BoundExprKind::Grouping(_) => Vec::new(),
            BoundExprKind::UnaryOp { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                vec![expr]
            }
            BoundExprKind::BinaryOp { left, right, .. } => vec![left, right],
            BoundExprKind::Between {
                expr, low, high, ..
            } => vec![expr, low, high],
            BoundExprKind::InList { expr, list, .. } => {
                let mut v: Vec<&BoundExpr> = vec![expr];
                v.extend(list.iter());
                v
            }
            BoundExprKind::Like { expr, pattern, .. } => vec![expr, pattern],
            BoundExprKind::Case {
                operand,
                branches,
                else_result,
            } => {
                let mut v: Vec<&BoundExpr> = Vec::new();
                v.extend(operand.iter().map(|b| b.as_ref()));
                for (w, t) in branches {
                    v.push(w);
                    v.push(t);
                }
                v.extend(else_result.iter().map(|b| b.as_ref()));
                v
            }
            BoundExprKind::Function { args, .. } => args.iter().collect(),
            BoundExprKind::Aggregate(agg) => {
                let mut v: Vec<&BoundExpr> = Vec::new();
                for e in agg.arg.exprs() {
                    v.push(e);
                }
                v.extend(agg.filter.iter().map(|b| b.as_ref()));
                v
            }
            BoundExprKind::Window(call) => call.exprs(),
            BoundExprKind::Subquery(sq) => sq.operand.iter().collect(),
        }
    }

    /// Rebuilds this node with `f` applied to each of `children()`'s
    /// expressions (same order, same coverage), keeping the node's own type
    /// and nullability.
    pub fn map_children(self, f: &mut dyn FnMut(BoundExpr) -> BoundExpr) -> BoundExpr {
        let BoundExpr { kind, ty, nullable } = self;
        let mut b = |e: Box<BoundExpr>| Box::new(f(*e));
        let kind = match kind {
            leaf @ (BoundExprKind::Literal(_)
            | BoundExprKind::Parameter { .. }
            | BoundExprKind::Column(_)
            | BoundExprKind::AggregateRef(_)
            | BoundExprKind::WindowRef(_)
            | BoundExprKind::GroupKeyRef(_)
            | BoundExprKind::Grouping(_)) => leaf,
            BoundExprKind::UnaryOp { op, expr } => BoundExprKind::UnaryOp { op, expr: b(expr) },
            BoundExprKind::IsNull { expr, negated } => BoundExprKind::IsNull {
                expr: b(expr),
                negated,
            },
            BoundExprKind::BinaryOp { left, op, right } => BoundExprKind::BinaryOp {
                left: b(left),
                op,
                right: b(right),
            },
            BoundExprKind::Between {
                expr,
                negated,
                low,
                high,
            } => BoundExprKind::Between {
                expr: b(expr),
                negated,
                low: b(low),
                high: b(high),
            },
            BoundExprKind::InList {
                expr,
                list,
                negated,
            } => BoundExprKind::InList {
                expr: b(expr),
                list: list.into_iter().map(|e| *b(Box::new(e))).collect(),
                negated,
            },
            BoundExprKind::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => BoundExprKind::Like {
                expr: b(expr),
                pattern: b(pattern),
                negated,
                case_insensitive,
            },
            BoundExprKind::Case {
                operand,
                branches,
                else_result,
            } => BoundExprKind::Case {
                operand: operand.map(&mut b),
                branches: branches
                    .into_iter()
                    .map(|(w, t)| (*b(Box::new(w)), *b(Box::new(t))))
                    .collect(),
                else_result: else_result.map(&mut b),
            },
            BoundExprKind::Function { name, args } => BoundExprKind::Function {
                name,
                args: args.into_iter().map(|e| *b(Box::new(e))).collect(),
            },
            BoundExprKind::Aggregate(agg) => BoundExprKind::Aggregate(BoundAggregateExpr {
                func: agg.func,
                arg: match agg.arg {
                    BoundAggregateArg::Wildcard => BoundAggregateArg::Wildcard,
                    BoundAggregateArg::Expr(e) => BoundAggregateArg::Expr(b(e)),
                    BoundAggregateArg::Multi(v) => {
                        BoundAggregateArg::Multi(v.into_iter().map(&mut b).collect())
                    }
                },
                distinct: agg.distinct,
                filter: agg.filter.map(&mut b),
            }),
            BoundExprKind::Window(call) => {
                BoundExprKind::Window(Box::new((*call).map_exprs(&mut *f)))
            }
            BoundExprKind::Subquery(mut sq) => {
                sq.operand = sq.operand.take().map(f);
                BoundExprKind::Subquery(sq)
            }
        };
        BoundExpr { kind, ty, nullable }
    }

    /// `true` iff this node or a descendant satisfies `pred`.
    pub fn any(&self, pred: &dyn Fn(&BoundExpr) -> bool) -> bool {
        pred(self) || self.children().into_iter().any(|c| c.any(pred))
    }
}

// =======================================================================
// SQL-mission-2: GROUPING SETS / ROLLUP / CUBE
// =======================================================================

/// One element of a `GROUP BY` clause that uses `ROLLUP` / `CUBE` /
/// `GROUPING SETS`; members are indexes into `BoundSelect::group_by`.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundGroupElement {
    Expr(usize),
    Rollup(Vec<Vec<usize>>),
    Cube(Vec<Vec<usize>>),
    Sets(Vec<Vec<usize>>),
}

impl BoundGroupElement {
    /// How many grouping sets this element stands for (saturating).
    pub fn set_count(&self) -> u128 {
        match self {
            BoundGroupElement::Expr(_) => 1,
            BoundGroupElement::Rollup(m) => m.len() as u128 + 1,
            BoundGroupElement::Cube(m) => {
                if m.len() >= 120 {
                    u128::MAX
                } else {
                    1u128 << m.len()
                }
            }
            BoundGroupElement::Sets(s) => s.len() as u128,
        }
    }

    fn sets(&self) -> Vec<Vec<usize>> {
        match self {
            BoundGroupElement::Expr(i) => vec![vec![*i]],
            BoundGroupElement::Rollup(members) => (0..=members.len())
                .rev()
                .map(|n| members[..n].iter().flatten().copied().collect())
                .collect(),
            BoundGroupElement::Cube(members) => {
                let k = members.len();
                (0..(1usize << k))
                    .rev()
                    .map(|mask| {
                        (0..k)
                            .filter(|i| mask & (1 << (k - 1 - i)) != 0)
                            .flat_map(|i| members[i].iter().copied())
                            .collect()
                    })
                    .collect()
            }
            BoundGroupElement::Sets(sets) => sets.clone(),
        }
    }
}

/// The structure of a `GROUP BY` with grouping extensions.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundGrouping {
    pub elements: Vec<BoundGroupElement>,
}

impl BoundGrouping {
    /// The number of grouping sets the clause expands to (saturating), without
    /// expanding it.
    pub fn set_count(&self) -> u128 {
        self.elements
            .iter()
            .fold(1u128, |acc, e| acc.saturating_mul(e.set_count()))
    }

    /// The grouping sets, each a sorted, duplicate-free list of `group_by`
    /// indexes. The cross product of the elements, first element outermost
    /// (`GROUP BY a, ROLLUP(b, c)` -> `(a,b,c) (a,b) (a)`); duplicate sets are
    /// kept, as the SQL standard's default `GROUP BY ALL` quantifier says.
    /// Fails with `ResourceLimit` instead of expanding past `cap` sets.
    pub fn expand(&self, cap: usize) -> crate::error::Result<Vec<Vec<usize>>> {
        let count = self.set_count();
        if count > cap as u128 {
            return Err(crate::error::SqlError::ResourceLimit {
                detail: format!(
                    "GROUP BY expands to {} grouping sets; max_grouping_sets_per_statement is {cap}",
                    if count == u128::MAX {
                        "more than 2^120".to_string()
                    } else {
                        count.to_string()
                    }
                ),
            });
        }
        let mut acc: Vec<Vec<usize>> = vec![Vec::new()];
        for element in &self.elements {
            let sets = element.sets();
            let mut next = Vec::with_capacity(acc.len() * sets.len());
            for prefix in &acc {
                for set in &sets {
                    let mut merged = prefix.clone();
                    merged.extend(set.iter().copied());
                    merged.sort_unstable();
                    merged.dedup();
                    next.push(merged);
                }
            }
            acc = next;
        }
        Ok(acc)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelectItem {
    pub expr: BoundExpr,
    /// The alias if one was given, else a derived display name (a plain
    /// column reference's own name; any other expression's name is
    /// implementation-defined — a future planner is free to synthesize
    /// one, since no client-visible contract depends on it yet).
    pub output_name: String,
}

/// The `table_id` every derived-table / CTE-reference relation carries in
/// its `BoundTableRef` and in the `ColumnRef`s that read it. It is never a
/// catalog id: nothing that looks a table up by id is ever handed such a
/// relation (`LogicalPlan::Derived` / `Cte` are not `Scan`s).
pub const SYNTHETIC_TABLE_ID: u32 = u32::MAX;

/// Where a `FROM` item's rows come from (SQL-mission-1 Slice C).
#[derive(Debug, Clone, PartialEq)]
pub enum BoundRelation {
    /// A catalog table -- the only kind before Slice C.
    Table,
    /// `(SELECT ...) AS alias`, optionally `LATERAL`.
    Derived(Box<BoundDerived>),
    /// A reference to a CTE of an enclosing (or this) query's `WITH` list.
    Cte(BoundCteRef),
    /// SQL-mission-1 Slice D: the reference to a recursive CTE inside its own
    /// recursive term -- the rows of the previous iteration.
    Working(BoundWorkingRef),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundWorkingRef {
    pub work_id: u32,
    pub name: String,
}

/// What makes a `BoundCte` recursive (SQL-mission-1 Slice D).
#[derive(Debug, Clone, PartialEq)]
pub struct BoundRecursion {
    /// The relation id of the working table the recursive term reads.
    pub work_id: u32,
    /// `UNION ALL` (rows are kept) rather than `UNION` (rows are deduplicated
    /// against everything produced so far).
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundDerived {
    /// Statement-unique relation id (`BindEnv::alloc_relation_id`).
    pub id: u32,
    pub query: BoundSelect,
    /// `LATERAL`: the body reads the `FROM` items before it, so it is
    /// re-run for every row of them (and listed in `outer_refs`).
    pub lateral: bool,
    /// For a `LATERAL` body: the columns it reads from outside itself.
    /// Empty for an ordinary derived table, which can read none.
    pub outer_refs: Vec<ColumnRef>,
    /// The body's physical plan, attached by the planner.
    pub plan: Option<crate::plan::physical::PhysicalPlan>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundCteRef {
    pub cte_id: u32,
    pub name: String,
    /// The shared, once-planned definition (filled in by the planner).
    pub plan: Option<std::sync::Arc<crate::plan::physical::CtePlan>>,
}

/// One `WITH name [(cols)] AS (...)` definition, owned by the query (or
/// `UPDATE`/`DELETE`) that wrote the `WITH`. References point to it by id.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundCte {
    pub id: u32,
    pub name: String,
    /// Output column names after any column-name list.
    pub columns: Vec<String>,
    /// For a recursive CTE, the set-operation shell `seed UNION [ALL]
    /// recursive-term` (left operand = seed, right operand = recursive term).
    pub query: BoundSelect,
    /// `Some` for a CTE that recurses (SQL-mission-1 Slice D).
    pub recursion: Option<BoundRecursion>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundFromItem {
    pub table: BoundTableRef,
    /// `None` for the first (base) item; `Some((kind, on_condition))`
    /// for every joined item.
    pub join: Option<(JoinKind, BoundExpr)>,
    /// Where this item's rows come from (`Table` for a catalog table).
    pub relation: BoundRelation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsOrder {
    First,
    Last,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundOrderByItem {
    pub expr: BoundExpr,
    pub descending: bool,
    /// Always resolved to a concrete order here (D5's default applied
    /// when the statement didn't specify one) — never left ambiguous for
    /// a later pass to have to re-derive.
    pub nulls: NullsOrder,
}

/// `left <op> [ALL] right` -- a bound set operation (SQL-mission-1 Slice E).
///
/// The two operands are whole bound queries; the operation's own output is a
/// relation of its own, `set_ref`, whose columns are the unified columns of the
/// operands (see `BoundSelect::set_op`).
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSetOp {
    pub op: crate::ast::SetOperator,
    pub all: bool,
    pub left: BoundSelect,
    pub right: BoundSelect,
    /// Statement-unique table-reference id the operation's output rows are
    /// exposed under: the `ORDER BY` of the set operation and the projection
    /// of the enclosing shell read their columns as `ColumnRef { table_ref:
    /// set_ref, ordinal }`.
    pub set_ref: u32,
    /// The unified type of each output column (`None` only where both
    /// operands are an untyped `NULL`). Each operand's value is widened to it
    /// (`exec::setop`) before rows are compared.
    pub column_types: Vec<Option<RelationalType>>,
    /// `UNION [ALL] BY NAME`: which column of each operand is result column
    /// `i` (`None`: the operand lacks it, so it is `NULL` there). `None` for a
    /// positional operation.
    pub by_name: Option<NameAlignment>,
    /// The operands' physical plans, attached by the planner.
    pub left_plan: Option<crate::plan::physical::PhysicalPlan>,
    pub right_plan: Option<crate::plan::physical::PhysicalPlan>,
}

/// How the columns of a `UNION [ALL] BY NAME` line up: result column `i` is
/// column `left[i]` of the left operand and column `right[i]` of the right one.
#[derive(Debug, Clone, PartialEq)]
pub struct NameAlignment {
    pub left: Vec<Option<usize>>,
    pub right: Vec<Option<usize>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelect {
    /// The `WITH` list this query wrote (SQL-mission-1 Slice C).
    pub ctes: Vec<BoundCte>,
    /// SQL-mission-1 Slice E: `Some` makes this query a **set operation**.
    /// `projection` then lists the operation's output columns (each a
    /// `Column` of `BoundSetOp::set_ref`, carrying the unified type and the
    /// first operand's name), `from` / `selection` / `group_by` / `aggregates`
    /// / `having` are empty, and `order_by` / `limit` / `offset` apply to the
    /// operation's result, with `order_by` bound against `set_ref`'s columns
    /// only.
    pub set_op: Option<Box<BoundSetOp>>,
    pub distinct: bool,
    pub from: Vec<BoundFromItem>,
    pub projection: Vec<BoundSelectItem>,
    pub selection: Option<BoundExpr>,
    pub group_by: Vec<BoundExpr>,
    /// SQL-mission-2: `Some` when the `GROUP BY` uses `ROLLUP` / `CUBE` /
    /// `GROUPING SETS`. `group_by` then lists every distinct grouping
    /// expression (in order of first appearance) and the elements refer to it
    /// by index.
    pub grouping: Option<BoundGrouping>,
    /// Every distinct aggregate function call collected out of
    /// `projection`/`having`/`order_by` during binding — those trees now
    /// reference this list positionally via `BoundExprKind::AggregateRef`
    /// rather than embedding `BoundExprKind::Aggregate` directly (Increment
    /// 11: one shared index namespace for the future `Aggregate` plan
    /// node's own output columns).
    pub aggregates: Vec<BoundAggregateExpr>,
    pub having: Option<BoundExpr>,
    /// SQL-mission-2: every distinct window function call collected out of
    /// `projection` / `qualify` / `order_by` (those trees reference it
    /// positionally through `WindowRef`).
    pub windows: Vec<BoundWindowCall>,
    /// SQL-mission-2: `QUALIFY`, evaluated after the window functions and
    /// before `DISTINCT` / `ORDER BY` / `LIMIT`.
    pub qualify: Option<BoundExpr>,
    pub order_by: Vec<BoundOrderByItem>,
    pub limit: Option<BoundExpr>,
    pub offset: Option<BoundExpr>,
    /// The highest `$n` parameter index referenced anywhere in this
    /// statement (0 if none) — a caller can validate a supplied
    /// parameter list's length against this without re-walking the tree.
    pub max_parameter: u32,
}

/// `RETURNING <select list>` of an `INSERT` / `UPDATE` / `DELETE` (SQL Phase 2a):
/// the items are evaluated, for every row the statement actually changed, against
/// that row's values as they are after the change (for `DELETE`: as they were).
/// `table_ref` is the relation id the items' columns are bound to.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundReturning {
    pub table_ref: u32,
    pub items: Vec<BoundSelectItem>,
}

/// One unique constraint an `ON CONFLICT` clause arbitrates on: the primary key
/// (`index_id` `None`) or a unique index, with the columns it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundArbiter {
    pub index_id: Option<u32>,
    pub ordinals: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundConflictAction {
    DoNothing,
    /// `DO UPDATE SET ... [WHERE ...]`, bound against two relations: the existing
    /// row (`BoundOnConflict::existing_ref`) and the proposed row (`excluded_ref`).
    DoUpdate {
        assignments: Vec<BoundAssignment>,
        selection: Option<BoundExpr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundOnConflict {
    /// The constraints a proposed row is checked against, in order. `DO UPDATE`
    /// always has exactly one (its conflict target); `DO NOTHING` without a
    /// target has the primary key and every ready unique index.
    pub arbiters: Vec<BoundArbiter>,
    pub existing_ref: u32,
    pub excluded_ref: u32,
    pub action: BoundConflictAction,
}

/// How the rows of an `INSERT ... SELECT` map onto the target table's columns.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundInsertSelect {
    pub query: BoundSelect,
    /// The query's physical plan, attached by the planner.
    pub plan: Option<crate::plan::physical::PhysicalPlan>,
    /// For each output column of the query: the target column it fills and the
    /// type its values are widened to (`None`: already that type).
    pub targets: Vec<InsertSelectTarget>,
    /// Target columns the statement does not name, with their `DEFAULT` (or `NULL`).
    pub defaults: Vec<(u16, Option<rubixdb::relational::RelationalValue>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertSelectTarget {
    pub ordinal: u16,
    /// The column's type when the query's differs: a lossless widening
    /// (`INTEGER` to `BIGINT`, ...) or -- the one narrowing -- `BIGINT` to
    /// `INTEGER`, checked per value (`SUM` / `COUNT` give `BIGINT`).
    pub convert_to: Option<RelationalType>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundInsert {
    /// The statement's `WITH` list (visible to an `INSERT ... SELECT`).
    pub ctes: Vec<BoundCte>,
    pub database_id: u32,
    pub schema_id: u32,
    pub table_id: u32,
    /// `INSERT ... SELECT`: the rows come from here and `rows` is empty.
    pub select: Option<Box<BoundInsertSelect>>,
    pub on_conflict: Option<BoundOnConflict>,
    pub returning: Option<BoundReturning>,
    /// Every `WITH` definition the planner planned for the statement (the
    /// statement's own list and those inside the source query); 0 until planned.
    pub defined_ctes: u32,
    /// One row per `VALUES (...)` tuple; each row has exactly one entry
    /// per catalog column, in ordinal order (a column omitted from an
    /// explicit column list binds as an untyped `NULL` `BoundExpr` here
    /// — `crate::bind` has already checked that is legal, i.e. the
    /// column is nullable or has a default; execution-time default-value
    /// substitution, if any, is a future executor's own concern).
    pub rows: Vec<Vec<BoundExpr>>,
    pub max_parameter: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundAssignment {
    pub ordinal: u16,
    pub value: BoundExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundUpdate {
    pub ctes: Vec<BoundCte>,
    pub table: BoundTableRef,
    pub assignments: Vec<BoundAssignment>,
    pub selection: Option<BoundExpr>,
    pub returning: Option<BoundReturning>,
    pub max_parameter: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundDelete {
    pub ctes: Vec<BoundCte>,
    pub table: BoundTableRef,
    pub selection: Option<BoundExpr>,
    pub returning: Option<BoundReturning>,
    pub max_parameter: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundColumnDef {
    pub name: String,
    /// `rubixdb::relational::value::TYPE_TAG_*` — the exact tag
    /// `CatalogService::create_table`'s `ColumnDef::data_type` expects
    /// (item 25: "do not bypass `CatalogService`").
    pub data_type: u8,
    pub type_params: Option<Vec<u8>>,
    pub nullable: bool,
    /// Already-encoded `RelationalValue` bytes for a literal `DEFAULT`
    /// expression (`crate::bind::ddl` requires `DEFAULT` to bind to a
    /// literal — no function calls or column references, matching
    /// `sqlparser`'s own "restricted-expr" framing for this clause).
    pub default_value: Option<Vec<u8>>,
}

/// A `CHECK` constraint, validated at bind time (the expression binds, is boolean,
/// and uses none of the forbidden forms) and carried as the text the catalog stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundCheckDef {
    pub name: String,
    pub text: String,
    /// The columns the expression reads.
    pub columns: Vec<u16>,
}

/// A foreign key declared in `CREATE TABLE`, resolved: the referencing columns are
/// ordinals of the table being created; the referenced table is an existing one
/// (`parent_table_id: Some`) or the table being created itself (`None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundCreateTableForeignKey {
    pub name: String,
    pub child_ordinals: Vec<u16>,
    pub parent_table_id: Option<u32>,
    pub parent_ordinals: Vec<u16>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundCreateTable {
    pub database_id: u32,
    pub schema_id: u32,
    pub name: String,
    pub columns: Vec<BoundColumnDef>,
    pub pk_ordinals: Vec<u16>,
    pub if_not_exists: bool,
    pub foreign_keys: Vec<BoundCreateTableForeignKey>,
    pub checks: Vec<BoundCheckDef>,
}

/// The change an `ALTER TABLE` makes, with every name resolved to catalog ids and
/// ordinals.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundAlterOp {
    /// `IF EXISTS` and the table (or `IF NOT EXISTS` and the column) is as asked.
    Noop,
    AddColumn(BoundColumnDef),
    RenameColumn {
        ordinal: u16,
        new_name: String,
    },
    SetDefault {
        ordinal: u16,
        /// Encoded literal, as `BoundColumnDef::default_value`.
        default_value: Vec<u8>,
    },
    DropDefault {
        ordinal: u16,
    },
    SetNotNull {
        ordinal: u16,
    },
    DropNotNull {
        ordinal: u16,
    },
    AddForeignKey(NewForeignKey),
    AddCheck(BoundCheckDef),
    DropCheck {
        constraint_id: u32,
    },
    AddUnique {
        name: String,
        column_ordinals: Vec<u16>,
    },
    DropForeignKey {
        fk_id: u32,
    },
    DropUnique {
        index_id: u32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundAlterTable {
    pub table_id: Option<u32>,
    pub operation: BoundAlterOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundDropTable {
    /// `None` only when `IF EXISTS` was given and the table genuinely
    /// does not exist (a real no-op, not an error) — `if_exists` alone
    /// does not imply this; it is `Some` whenever the table *was*
    /// found, `IF EXISTS` or not.
    pub table_id: Option<u32>,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundCreateIndex {
    pub table_id: u32,
    pub name: String,
    pub kind: IndexKind,
    pub column_ordinals: Vec<u16>,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundDropIndex {
    /// See `BoundDropTable::table_id`'s doc comment — the same `IF
    /// EXISTS`-no-op shape.
    pub index_id: Option<u32>,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundCreateSchema {
    pub database_id: u32,
    pub name: String,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundCreateDatabase {
    pub name: String,
    pub if_not_exists: bool,
}

/// The fully bound, authorized, typed representation of one statement.
/// **Execution-status boundary** (item 5): every variant is `BOUND` and
/// stops there — nothing in this crate executes any of them.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundStatement {
    Select(Box<BoundSelect>),
    Insert(BoundInsert),
    Update(BoundUpdate),
    Delete(BoundDelete),
    CreateDatabase(BoundCreateDatabase),
    CreateSchema(BoundCreateSchema),
    CreateTable(BoundCreateTable),
    DropTable(BoundDropTable),
    CreateIndex(BoundCreateIndex),
    DropIndex(BoundDropIndex),
    AlterTable(BoundAlterTable),
    Explain(Box<BoundStatement>),
    Begin,
    Commit,
    Rollback,
}
