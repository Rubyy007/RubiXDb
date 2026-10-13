//! An independent oracle for window functions (SQL-mission-2).
//!
//! `evaluate` computes what a window function returns for every row of a small
//! table **directly from the definitions** in ISO/IEC 9075-2 (7.15 window
//! clause, 6.10 window function): for each row it builds the partition, orders
//! it, collects the rows of the frame by testing every row of the partition
//! against the frame's two bound conditions, and applies the function to them.
//! Nothing here is incremental, sorted-array arithmetic or binary search; it
//! never calls `crate::exec`, `crate::plan`, `crate::bind` or `crate::aggregate`,
//! so a bug in the executor's frame arithmetic cannot also be in the oracle.
//!
//! The tests compare it with the real pipeline (parse, bind, plan, execute) on
//! tables of 0, 1, 2 and 100 rows, with all rows tied, no ties and some ties,
//! with NULL partition keys and NULL ordering keys, for every legal frame
//! boundary combination of both `ROWS` and `RANGE`, and for several windows in
//! one `SELECT`.

use std::cmp::Ordering;

use proptest::prelude::*;
use rubixdb::relational::RelationalValue;

use crate::mission1_support::{Db, V};
use crate::window_tests::{seed_w, win_db};

/// One input row: `(id, g, o, x)`.
pub type RRow = (i32, Option<i32>, Option<i32>, Option<i32>);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bound {
    UnboundedPreceding,
    Preceding(i64),
    CurrentRow,
    Following(i64),
    UnboundedFollowing,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Units {
    Rows,
    Range,
}

#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub units: Units,
    pub start: Bound,
    pub end: Bound,
}

/// The window: `PARTITION BY g`?, `ORDER BY o [DESC] [NULLS FIRST|LAST]`?, frame?
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub partition: bool,
    /// `Some((descending, nulls_first))`; `nulls_first` `None` is the default.
    pub order: Option<(bool, Option<bool>)>,
    pub frame: Option<Frame>,
}

impl Spec {
    pub fn sql(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.partition {
            parts.push("PARTITION BY g".to_string());
        }
        if let Some((desc, nulls)) = self.order {
            parts.push(format!(
                "ORDER BY o{}{}",
                if desc { " DESC" } else { "" },
                match nulls {
                    Some(true) => " NULLS FIRST",
                    Some(false) => " NULLS LAST",
                    None => "",
                }
            ));
        }
        if let Some(f) = self.frame {
            let b = |b: Bound| match b {
                Bound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_string(),
                Bound::Preceding(n) => format!("{n} PRECEDING"),
                Bound::CurrentRow => "CURRENT ROW".to_string(),
                Bound::Following(n) => format!("{n} FOLLOWING"),
                Bound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_string(),
            };
            parts.push(format!(
                "{} BETWEEN {} AND {}",
                if f.units == Units::Rows {
                    "ROWS"
                } else {
                    "RANGE"
                },
                b(f.start),
                b(f.end)
            ));
        }
        format!("OVER ({})", parts.join(" "))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Func {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile(i64),
    /// `LAG(x, offset, default)`.
    Lag(i64, Option<i32>),
    Lead(i64, Option<i32>),
    FirstValue,
    LastValue,
    NthValue(i64),
    Sum,
    Count,
    CountStar,
    Avg,
    Min,
    Max,
}

impl Func {
    pub fn sql(&self) -> String {
        match self {
            Func::RowNumber => "ROW_NUMBER()".into(),
            Func::Rank => "RANK()".into(),
            Func::DenseRank => "DENSE_RANK()".into(),
            Func::PercentRank => "PERCENT_RANK()".into(),
            Func::CumeDist => "CUME_DIST()".into(),
            Func::Ntile(n) => format!("NTILE({n})"),
            Func::Lag(o, None) => format!("LAG(x, {o})"),
            Func::Lag(o, Some(d)) => format!("LAG(x, {o}, {d})"),
            Func::Lead(o, None) => format!("LEAD(x, {o})"),
            Func::Lead(o, Some(d)) => format!("LEAD(x, {o}, {d})"),
            Func::FirstValue => "FIRST_VALUE(x)".into(),
            Func::LastValue => "LAST_VALUE(x)".into(),
            Func::NthValue(n) => format!("NTH_VALUE(x, {n})"),
            Func::Sum => "SUM(x)".into(),
            Func::Count => "COUNT(x)".into(),
            Func::CountStar => "COUNT(*)".into(),
            Func::Avg => "AVG(x)".into(),
            Func::Min => "MIN(x)".into(),
            Func::Max => "MAX(x)".into(),
        }
    }

    pub fn needs_order(&self) -> bool {
        matches!(
            self,
            Func::Rank
                | Func::DenseRank
                | Func::PercentRank
                | Func::CumeDist
                | Func::Lag(..)
                | Func::Lead(..)
        )
    }
}

/// A function result, engine types erased (`Integer` and `BIGINT` are `I`).
#[derive(Clone, Copy, Debug)]
pub enum Out {
    Null,
    I(i64),
    F(f64),
}

impl PartialEq for Out {
    fn eq(&self, other: &Out) -> bool {
        match (self, other) {
            (Out::Null, Out::Null) => true,
            (Out::I(a), Out::I(b)) => a == b,
            (Out::F(a), Out::F(b)) => (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0),
            _ => false,
        }
    }
}

pub fn to_out(v: &V) -> Out {
    match v {
        None => Out::Null,
        Some(RelationalValue::Integer(n)) => Out::I(i64::from(*n)),
        Some(RelationalValue::Bigint(n)) => Out::I(*n),
        Some(RelationalValue::Double(f)) => Out::F(*f),
        other => panic!("unexpected result value {other:?}"),
    }
}

// ----------------------------------------------------------------------
// The oracle
// ----------------------------------------------------------------------

/// `a` sorts before `b` in the window ordering?  `Ordering` of the `o` keys with
/// the NULL placement and direction applied.
fn order_cmp(a: Option<i32>, b: Option<i32>, order: (bool, Option<bool>)) -> Ordering {
    let (desc, nulls_first) = order;
    // D5: NULLS LAST ascending, NULLS FIRST descending, unless stated.
    let nulls_first = nulls_first.unwrap_or(desc);
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(x), Some(y)) => {
            if desc {
                y.cmp(&x)
            } else {
                x.cmp(&y)
            }
        }
    }
}

pub fn evaluate(rows: &[RRow], spec: &Spec, f: &Func) -> Vec<Out> {
    let n = rows.len();
    let mut out = vec![Out::Null; n];
    for r in 0..n {
        // The partition of row r: rows with the same g (NULLs are one group).
        let mut part: Vec<usize> = (0..n)
            .filter(|&j| !spec.partition || rows[j].1 == rows[r].1)
            .collect();
        // Ordered; ties keep input order (ids ascend with input order).
        if let Some(order) = spec.order {
            part.sort_by(|&a, &b| order_cmp(rows[a].2, rows[b].2, order).then(a.cmp(&b)));
        }
        let pos = part.iter().position(|&j| j == r).unwrap();
        let m = part.len();
        let order = spec.order;
        // Peers: same o (NULLs peers); with no ORDER BY all rows are peers.
        let peers = |a: usize, b: usize| match order {
            None => true,
            Some(_) => rows[a].2 == rows[b].2,
        };
        // Index of the peer group of the row at position q.
        let group: Vec<usize> = {
            let mut g = Vec::with_capacity(m);
            let mut cur = 0usize;
            for q in 0..m {
                if q > 0 && !peers(part[q - 1], part[q]) {
                    cur += 1;
                }
                g.push(cur);
            }
            g
        };
        let first_peer = (0..m).find(|&q| group[q] == group[pos]).unwrap();
        let last_peer = (0..m).rev().find(|&q| group[q] == group[pos]).unwrap();
        let rank = first_peer as i64 + 1;

        // The frame of row r as the positions that satisfy both bounds.
        let frame = spec.frame.unwrap_or(Frame {
            units: Units::Range,
            start: Bound::UnboundedPreceding,
            end: if order.is_some() {
                Bound::CurrentRow
            } else {
                Bound::UnboundedFollowing
            },
        });
        let desc = order.map(|o| o.0).unwrap_or(false);
        let key_r = rows[r].2;
        let in_frame = |q: usize| -> bool {
            let key_q = rows[part[q]].2;
            // NULL sort keys are ordered by NULLS FIRST / NULLS LAST like any other
            // key, so a value-offset bound compares *positions in that order*: a
            // NULL that sorts before the data is before every target value (it
            // fails a "starts at or after" test and passes an "ends at or before"
            // one), a NULL that sorts after the data is after every target.
            let nulls_first = order.map(|o| o.1.unwrap_or(o.0)).unwrap_or(false);
            let offset_cond = |off: i64, start: bool, preceding: bool| -> bool {
                // r NULL: its peers (the NULLs) only; r non-NULL: positions in the ordering.
                match (key_r, key_q) {
                    (None, _) => {
                        if start {
                            group[q] >= group[pos]
                        } else {
                            group[q] <= group[pos]
                        }
                    }
                    (Some(_), None) => {
                        if start {
                            !nulls_first // at or after the target iff the NULLs sort last
                        } else {
                            nulls_first // at or before the target iff the NULLs sort first
                        }
                    }
                    (Some(kr), Some(kq)) => {
                        let (kr, kq) = (i64::from(kr), i64::from(kq));
                        // target = kr -/+ off in the ordering's direction.
                        let toward_smaller = preceding != desc; // ascending PRECEDING = smaller values
                        let target = if toward_smaller { kr - off } else { kr + off };
                        // start: q is at or after target in the ordering; end: at or before.
                        match (start, desc) {
                            (true, false) => kq >= target,
                            (true, true) => kq <= target,
                            (false, false) => kq <= target,
                            (false, true) => kq >= target,
                        }
                    }
                }
            };
            let start_ok = match (frame.units, frame.start) {
                (_, Bound::UnboundedPreceding) => true,
                (_, Bound::UnboundedFollowing) => false,
                (Units::Rows, Bound::CurrentRow) => q >= pos,
                (Units::Rows, Bound::Preceding(k)) => (q as i64) >= pos as i64 - k,
                (Units::Rows, Bound::Following(k)) => (q as i64) >= pos as i64 + k,
                (Units::Range, Bound::CurrentRow) => group[q] >= group[pos],
                (Units::Range, Bound::Preceding(k)) => offset_cond(k, true, true),
                (Units::Range, Bound::Following(k)) => offset_cond(k, true, false),
            };
            let end_ok = match (frame.units, frame.end) {
                (_, Bound::UnboundedFollowing) => true,
                (_, Bound::UnboundedPreceding) => false,
                (Units::Rows, Bound::CurrentRow) => q <= pos,
                (Units::Rows, Bound::Preceding(k)) => (q as i64) <= pos as i64 - k,
                (Units::Rows, Bound::Following(k)) => (q as i64) <= pos as i64 + k,
                (Units::Range, Bound::CurrentRow) => group[q] <= group[pos],
                (Units::Range, Bound::Preceding(k)) => offset_cond(k, false, true),
                (Units::Range, Bound::Following(k)) => offset_cond(k, false, false),
            };
            start_ok && end_ok
        };
        let frame_rows: Vec<usize> = (0..m).filter(|&q| in_frame(q)).map(|q| part[q]).collect();
        let xs: Vec<i64> = frame_rows
            .iter()
            .filter_map(|&j| rows[j].3.map(i64::from))
            .collect();
        let x_of = |j: usize| rows[j].3.map(|v| Out::I(i64::from(v))).unwrap_or(Out::Null);

        out[r] = match f {
            Func::RowNumber => Out::I(pos as i64 + 1),
            Func::Rank => Out::I(rank),
            Func::DenseRank => Out::I(group[pos] as i64 + 1),
            Func::PercentRank => Out::F(if m <= 1 {
                0.0
            } else {
                (rank - 1) as f64 / (m - 1) as f64
            }),
            Func::CumeDist => Out::F((last_peer + 1) as f64 / m as f64),
            Func::Ntile(k) => {
                // The first (m mod k) buckets get one extra row.
                let (m, k, p) = (m as i64, *k, pos as i64);
                let (base, rem) = (m / k, m % k);
                Out::I(if p < rem * (base + 1) {
                    p / (base + 1) + 1
                } else {
                    rem + (p - rem * (base + 1)) / base + 1
                })
            }
            Func::Lag(off, default) | Func::Lead(off, default) => {
                let target = if matches!(f, Func::Lag(..)) {
                    pos as i64 - off
                } else {
                    pos as i64 + off
                };
                if target >= 0 && (target as usize) < m {
                    x_of(part[target as usize])
                } else {
                    default.map(|d| Out::I(i64::from(d))).unwrap_or(Out::Null)
                }
            }
            Func::FirstValue => frame_rows.first().map(|&j| x_of(j)).unwrap_or(Out::Null),
            Func::LastValue => frame_rows.last().map(|&j| x_of(j)).unwrap_or(Out::Null),
            Func::NthValue(k) => frame_rows
                .get((*k - 1) as usize)
                .map(|&j| x_of(j))
                .unwrap_or(Out::Null),
            Func::CountStar => Out::I(frame_rows.len() as i64),
            Func::Count => Out::I(xs.len() as i64),
            Func::Sum => {
                if xs.is_empty() {
                    Out::Null
                } else {
                    Out::I(xs.iter().sum())
                }
            }
            Func::Avg => {
                if xs.is_empty() {
                    Out::Null
                } else {
                    Out::F(xs.iter().sum::<i64>() as f64 / xs.len() as f64)
                }
            }
            Func::Min => xs.iter().min().map(|v| Out::I(*v)).unwrap_or(Out::Null),
            Func::Max => xs.iter().max().map(|v| Out::I(*v)).unwrap_or(Out::Null),
        };
    }
    out
}

// ----------------------------------------------------------------------
// Datasets and the comparison with the real pipeline
// ----------------------------------------------------------------------

/// Deterministic pseudo-random numbers (no `rand` dependency): xorshift.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

pub fn datasets() -> Vec<(&'static str, Vec<RRow>)> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let n100 = |f: &dyn Fn(i32) -> RRow| (1..=100).map(f).collect::<Vec<_>>();
    vec![
        ("empty", vec![]),
        ("one_row", vec![(1, Some(1), Some(5), Some(7))]),
        (
            "two_rows",
            vec![(1, Some(1), Some(5), Some(7)), (2, Some(1), Some(5), None)],
        ),
        (
            "hundred_no_ties",
            n100(&|i| (i, Some(i % 3), Some(i), Some((i * 37) % 101 - 50))),
        ),
        (
            "hundred_all_tied",
            n100(&|i| (i, Some(i % 4), Some(1), Some((i * 11) % 17))),
        ),
        (
            "hundred_some_ties",
            n100(&|i| (i, Some(i % 5), Some((i * 3) % 10), Some((i * 13) % 23 - 11))),
        ),
        (
            "null_partition_keys",
            n100(&|i| {
                (
                    i,
                    if i % 4 == 0 { None } else { Some(i % 3) },
                    Some((i * 7) % 12),
                    Some(i % 9),
                )
            }),
        ),
        (
            "null_order_keys",
            n100(&|i| {
                (
                    i,
                    Some(i % 3),
                    if i % 5 == 0 { None } else { Some((i * 7) % 12) },
                    Some((i * 5) % 19),
                )
            }),
        ),
        ("all_null_order", n100(&|i| (i, Some(i % 2), None, Some(i)))),
        (
            "null_values",
            n100(&|i| {
                (
                    i,
                    Some(i % 3),
                    Some(i % 8),
                    if i % 3 == 0 { None } else { Some(i) },
                )
            }),
        ),
        (
            "random_mixed",
            (1..=100)
                .map(|i| {
                    let g = if rng.below(6) == 0 {
                        None
                    } else {
                        Some(rng.below(4) as i32)
                    };
                    let o = if rng.below(6) == 0 {
                        None
                    } else {
                        Some(rng.below(15) as i32 - 3)
                    };
                    let x = if rng.below(5) == 0 {
                        None
                    } else {
                        Some(rng.below(100) as i32 - 40)
                    };
                    (i, g, o, x)
                })
                .collect(),
        ),
    ]
}

/// Every legal frame (for the given units): start in {UP, 1 P, 2 P, CR, 1 F},
/// end in {1 P, CR, 1 F, 2 F, UF}, minus the combinations the standard forbids.
pub fn legal_frames(units: Units) -> Vec<Frame> {
    use Bound::*;
    let starts = [
        UnboundedPreceding,
        Preceding(1),
        Preceding(2),
        CurrentRow,
        Following(1),
    ];
    let ends = [
        Preceding(1),
        CurrentRow,
        Following(1),
        Following(2),
        UnboundedFollowing,
    ];
    let mut v = Vec::new();
    for &start in &starts {
        for &end in &ends {
            let legal = !matches!(
                (start, end),
                (CurrentRow, Preceding(_))
                    | (Following(_), Preceding(_))
                    | (Following(_), CurrentRow)
            );
            if legal {
                v.push(Frame { units, start, end });
            }
        }
    }
    v
}

/// Runs `SELECT id, f1 OVER spec, f2 OVER spec, ... FROM w ORDER BY id` and
/// returns each function's column.
pub fn run_pipeline(db: &Db, spec: &Spec, funcs: &[Func]) -> Vec<Vec<Out>> {
    let cols: Vec<String> = funcs
        .iter()
        .map(|f| format!("{} {}", f.sql(), spec.sql()))
        .collect();
    let sql = format!("SELECT id, {} FROM w ORDER BY id", cols.join(", "));
    let rows = db.rows(&sql);
    (0..funcs.len())
        .map(|c| rows.iter().map(|r| to_out(&r[c + 1])).collect())
        .collect()
}

pub fn check(db: &Db, name: &str, rows: &[RRow], spec: &Spec, funcs: &[Func]) {
    let got = run_pipeline(db, spec, funcs);
    for (c, f) in funcs.iter().enumerate() {
        let want = evaluate(rows, spec, f);
        assert_eq!(
            got[c],
            want,
            "dataset {name}: {} {} differs from the reference model\n rows: {rows:?}",
            f.sql(),
            spec.sql()
        );
    }
}

fn frame_funcs() -> Vec<Func> {
    vec![
        Func::Sum,
        Func::Count,
        Func::CountStar,
        Func::Avg,
        Func::Min,
        Func::Max,
        Func::FirstValue,
        Func::LastValue,
        Func::NthValue(2),
    ]
}

fn other_funcs() -> Vec<Func> {
    vec![
        Func::RowNumber,
        Func::Rank,
        Func::DenseRank,
        Func::PercentRank,
        Func::CumeDist,
        Func::Ntile(1),
        Func::Ntile(3),
        Func::Ntile(1000),
        Func::Lag(1, None),
        Func::Lag(2, Some(-9)),
        Func::Lead(1, None),
        Func::Lead(3, Some(99)),
    ]
}

fn orders() -> Vec<(bool, Option<bool>)> {
    vec![
        (false, None),
        (true, None),
        (false, Some(true)),
        (true, Some(false)),
    ]
}

#[test]
fn every_frame_boundary_combination_matches_the_model_on_every_dataset() {
    let mut cases = 0usize;
    for (name, rows) in datasets() {
        let db = win_db(&format!("wref_frames_{name}"));
        seed_w(&db, &rows);
        for units in [Units::Rows, Units::Range] {
            for frame in legal_frames(units) {
                for partition in [false, true] {
                    for order in orders() {
                        // ROWS frames over an order with ties depend on the tie
                        // break (input order), which the model reproduces.
                        let spec = Spec {
                            partition,
                            order: Some(order),
                            frame: Some(frame),
                        };
                        check(&db, name, &rows, &spec, &frame_funcs());
                        cases += 1;
                    }
                }
            }
        }
        db.cleanup();
    }
    assert!(cases > 1000, "{cases}");
}

#[test]
fn frames_without_an_order_by_match_the_model() {
    // ROWS frames need no ORDER BY (the order is then the input order; every row
    // is a peer, so only RANGE UNBOUNDED/CURRENT ROW forms are legal without one).
    for (name, rows) in datasets() {
        let db = win_db(&format!("wref_noorder_{name}"));
        seed_w(&db, &rows);
        for partition in [false, true] {
            // The default frame (no ORDER BY): the whole partition.
            let spec = Spec {
                partition,
                order: None,
                frame: None,
            };
            check(&db, name, &rows, &spec, &frame_funcs());
            check(&db, name, &rows, &spec, &[Func::RowNumber, Func::Ntile(3)]);
            for units in [Units::Rows, Units::Range] {
                for frame in legal_frames(Units::Rows) {
                    let offsets = matches!(frame.start, Bound::Preceding(_) | Bound::Following(_))
                        || matches!(frame.end, Bound::Preceding(_) | Bound::Following(_));
                    if units == Units::Range && offsets {
                        continue; // RANGE with an offset needs an ORDER BY
                    }
                    let spec = Spec {
                        partition,
                        order: None,
                        frame: Some(Frame { units, ..frame }),
                    };
                    check(&db, name, &rows, &spec, &frame_funcs());
                }
            }
        }
        db.cleanup();
    }
}

#[test]
fn default_frames_and_the_functions_that_ignore_frames_match_the_model() {
    for (name, rows) in datasets() {
        let db = win_db(&format!("wref_default_{name}"));
        seed_w(&db, &rows);
        for partition in [false, true] {
            for order in orders() {
                let spec = Spec {
                    partition,
                    order: Some(order),
                    frame: None,
                };
                check(&db, name, &rows, &spec, &frame_funcs());
                check(&db, name, &rows, &spec, &other_funcs());
            }
            // Without ORDER BY: only the functions that do not require one.
            let spec = Spec {
                partition,
                order: None,
                frame: None,
            };
            let funcs: Vec<Func> = other_funcs()
                .into_iter()
                .filter(|f| !f.needs_order())
                .collect();
            check(&db, name, &rows, &spec, &funcs);
        }
        db.cleanup();
    }
}

#[test]
fn two_functions_with_different_windows_in_one_select_each_match_the_model() {
    for (name, rows) in datasets() {
        let db = win_db(&format!("wref_two_{name}"));
        seed_w(&db, &rows);
        let w1 = Spec {
            partition: true,
            order: Some((false, None)),
            frame: None,
        };
        let w2 = Spec {
            partition: false,
            order: Some((true, None)),
            frame: Some(Frame {
                units: Units::Rows,
                start: Bound::Preceding(2),
                end: Bound::CurrentRow,
            }),
        };
        let w3 = Spec {
            partition: true,
            order: Some((false, None)),
            frame: Some(Frame {
                units: Units::Range,
                start: Bound::Preceding(1),
                end: Bound::Following(1),
            }),
        };
        let sql =
            format!(
            "SELECT id, SUM(x) {}, SUM(x) {}, RANK() {}, COUNT(*) {}, SUM(x) {} FROM w ORDER BY id",
            w1.sql(),
            w2.sql(),
            w1.sql(),
            w3.sql(),
            Spec { partition: false, order: None, frame: None }.sql()
        );
        let got = db.rows(&sql);
        let pairs: [(usize, Spec, Func); 5] = [
            (1, w1, Func::Sum),
            (2, w2, Func::Sum),
            (3, w1, Func::Rank),
            (4, w3, Func::CountStar),
            (
                5,
                Spec {
                    partition: false,
                    order: None,
                    frame: None,
                },
                Func::Sum,
            ),
        ];
        for (c, spec, f) in pairs {
            let want = evaluate(&rows, &spec, &f);
            let have: Vec<Out> = got.iter().map(|r| to_out(&r[c])).collect();
            assert_eq!(
                have,
                want,
                "dataset {name}: column {c} ({} {})",
                f.sql(),
                spec.sql()
            );
        }
        db.cleanup();
    }
}

fn arb_rows() -> impl Strategy<Value = Vec<RRow>> {
    prop::collection::vec(
        (
            prop::option::weighted(0.8, 0i32..4),
            prop::option::weighted(0.8, -3i32..8),
            prop::option::weighted(0.8, -50i32..50),
        ),
        0..40,
    )
    .prop_map(|v| {
        v.into_iter()
            .enumerate()
            .map(|(i, (g, o, x))| (i as i32 + 1, g, o, x))
            .collect()
    })
}

fn arb_frame() -> impl Strategy<Value = Option<Frame>> {
    let all: Vec<Frame> = [Units::Rows, Units::Range]
        .into_iter()
        .flat_map(legal_frames)
        .collect();
    (0..=all.len()).prop_map(move |i| all.get(i).copied())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn random_tables_and_windows_match_the_model(
        rows in arb_rows(),
        frame in arb_frame(),
        partition in any::<bool>(),
        desc in any::<bool>(),
        nulls in prop::option::of(any::<bool>()),
    ) {
        let db = win_db("wref_prop");
        seed_w(&db, &rows);
        let spec = Spec { partition, order: Some((desc, nulls)), frame };
        check(&db, "random", &rows, &spec, &frame_funcs());
        if frame.is_none() {
            check(&db, "random", &rows, &spec, &other_funcs());
        }
        db.cleanup();
    }
}
