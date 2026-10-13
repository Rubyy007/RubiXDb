//! An independent reference aggregation engine — item 51's own "never
//! the production algorithm as its own oracle," applied to `GROUP BY`/
//! `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`. `reference_aggregate` is a from-
//! scratch reimplementation over plain `Vec`/`HashMap` — it never calls
//! `crate::plan`, `crate::exec`, `TableStore`, or `IndexBuilder` for any
//! of its own semantic decisions. Compared against the real end-to-end
//! pipeline (parse → bind → plan → execute) for a fixed scenario matrix
//! (item 53) and for `proptest`-generated random tables with a fixed
//! seed (item 52).

use std::collections::HashMap;
use std::sync::Arc;

use proptest::prelude::*;

use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::value::{TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use rubixdb::relational::{RelationalValue, TransactionManager};

use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::exec::{execute_autocommit, CancellationToken, ExecLimits, ExecMetrics};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::plan::{build_plan, PlannerLimits, PlannerMetrics};
use crate::test_support::Fixture;

/// One reference-model input row: a nullable grouping key and a nullable
/// numeric value.
type Row = (Option<i32>, Option<i32>);

/// One reference-model output group: `(group_key, count_star, count_val,
/// sum_val, min_val, max_val, avg_val)`.
type GroupResult = (
    Option<i32>,
    i64,
    i64,
    Option<i64>,
    Option<i32>,
    Option<i32>,
    Option<f64>,
);

/// Groups `rows` by their (nullable) first field — `HashMap<Option<i32>,
/// _>`'s own derived `Eq`/`Hash` already treats `None == None` (SQL's
/// `GROUP BY` NULL-grouping rule, item 15 — deliberately the *opposite*
/// of `WHERE`'s `NULL = NULL -> UNKNOWN`, and this reference model never
/// imports or calls anything from `crate::exec::expr_eval` to make sure
/// it cannot accidentally reuse that other rule) — and independently
/// computes `COUNT(*)`, `COUNT(val)`, `SUM(val)`, `MIN(val)`, `MAX(val)`,
/// `AVG(val)` per group, returned sorted by group key ascending with
/// `NULL` first (matching `ORDER BY grp NULLS FIRST`, so a caller never
/// has to separately reason about emission order).
/// `(count_star, count_val, sum_val, min_val, max_val)` per-group
/// accumulator, before finalization into a `GroupResult`.
type GroupAccumulator = (i64, i64, i64, Option<i32>, Option<i32>);

fn reference_aggregate(rows: &[Row]) -> Vec<GroupResult> {
    let mut groups: HashMap<Option<i32>, GroupAccumulator> = HashMap::new();
    for &(grp, val) in rows {
        let entry = groups.entry(grp).or_insert((0, 0, 0, None, None));
        entry.0 += 1; // count_star
        if let Some(v) = val {
            entry.1 += 1; // count_val
            entry.2 += v as i64; // sum_val (accumulated regardless; only used if count_val > 0)
            entry.3 = Some(entry.3.map_or(v, |m| m.min(v)));
            entry.4 = Some(entry.4.map_or(v, |m| m.max(v)));
        }
    }
    let mut out: Vec<GroupResult> = groups
        .into_iter()
        .map(
            |(grp, (count_star, count_val, sum_val, min_val, max_val))| {
                let sum = if count_val > 0 { Some(sum_val) } else { None };
                let avg = if count_val > 0 {
                    Some(sum_val as f64 / count_val as f64)
                } else {
                    None
                };
                (grp, count_star, count_val, sum, min_val, max_val, avg)
            },
        )
        .collect();
    out.sort_by(|a, b| match (a.0, b.0) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(&y),
    });
    out
}

struct AggFixture {
    f: Fixture,
    catalog: Arc<CatalogService>,
    store: Arc<TableStore>,
    builder: Arc<IndexBuilder>,
    txm: TransactionManager,
}

impl AggFixture {
    fn new(tag: &str) -> Self {
        let f = Fixture::new(tag);
        f.catalog
            .create_table(
                f.ctx.default_schema_id,
                "agg_t",
                &[
                    ColumnDef {
                        name: "id".to_string(),
                        data_type: TYPE_TAG_INTEGER,
                        nullable: false,
                        default_value: None,
                        type_params: None,
                    },
                    ColumnDef {
                        name: "grp".to_string(),
                        data_type: TYPE_TAG_INTEGER,
                        nullable: true,
                        default_value: None,
                        type_params: None,
                    },
                    ColumnDef {
                        name: "val".to_string(),
                        data_type: TYPE_TAG_INTEGER,
                        nullable: true,
                        default_value: None,
                        type_params: None,
                    },
                    ColumnDef {
                        name: "label".to_string(),
                        data_type: TYPE_TAG_TEXT,
                        nullable: true,
                        default_value: None,
                        type_params: None,
                    },
                ],
                &[0],
            )
            .unwrap();

        let catalog = Arc::new(CatalogService::new(Arc::clone(&f.engine)));
        let store = Arc::new(TableStore::new(Arc::clone(&f.engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&f.engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        let txm = TransactionManager::new(Arc::clone(&f.engine), Arc::clone(&store));

        AggFixture {
            f,
            catalog,
            store,
            builder,
            txm,
        }
    }

    fn table_id(&self) -> u32 {
        self.catalog
            .get_table_by_name(self.f.ctx.default_schema_id, "agg_t")
            .unwrap()
            .unwrap()
            .table_id
    }

    fn insert(&self, rows: &[Row]) {
        let table_id = self.table_id();
        for (i, &(grp, val)) in rows.iter().enumerate() {
            self.store
                .put_row(
                    table_id,
                    &[
                        Some(RelationalValue::Integer(i as i32)),
                        grp.map(RelationalValue::Integer),
                        val.map(RelationalValue::Integer),
                        None,
                    ],
                )
                .unwrap();
        }
    }

    fn run_grouped_aggregate(&self) -> Vec<GroupResult> {
        let sql = "SELECT grp, COUNT(*), COUNT(val), SUM(val), MIN(val), MAX(val), AVG(val) \
                    FROM agg_t GROUP BY grp ORDER BY grp NULLS FIRST";
        let limits = SqlLimits::default();
        let stmt = parse_statement(sql, &limits).unwrap();
        let metrics = SqlMetrics::default();
        let auth = AuthContext::admin("reference-model-test");
        let bound =
            bind_statement(&self.catalog, &self.f.ctx, &auth, &metrics, &limits, &stmt).unwrap();
        let plan = build_plan(
            &bound,
            &self.catalog,
            &PlannerLimits::default(),
            &PlannerMetrics::default(),
        )
        .unwrap();

        let result = execute_autocommit(
            &plan,
            &self.txm,
            &self.store,
            &self.builder,
            &[],
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap();

        result
            .rows
            .into_iter()
            .map(|r| {
                let grp = as_opt_i32(&r[0]);
                let count_star = as_i64(&r[1]);
                let count_val = as_i64(&r[2]);
                let sum_val = r[3].as_ref().map(|v| match v {
                    RelationalValue::Bigint(n) => *n,
                    other => panic!("unexpected SUM type {other:?}"),
                });
                let min_val = as_opt_i32(&r[4]);
                let max_val = as_opt_i32(&r[5]);
                let avg_val = r[6].as_ref().map(|v| match v {
                    RelationalValue::Double(d) => *d,
                    other => panic!("unexpected AVG type {other:?}"),
                });
                (
                    grp, count_star, count_val, sum_val, min_val, max_val, avg_val,
                )
            })
            .collect()
    }

    fn cleanup(self) {
        self.f.cleanup();
    }
}

fn as_opt_i32(v: &Option<RelationalValue>) -> Option<i32> {
    match v {
        None => None,
        Some(RelationalValue::Integer(n)) => Some(*n),
        other => panic!("unexpected type {other:?}"),
    }
}

fn as_i64(v: &Option<RelationalValue>) -> i64 {
    match v {
        Some(RelationalValue::Bigint(n)) => *n,
        other => panic!("unexpected COUNT type {other:?}"),
    }
}

/// Asserts actual/expected agree, treating `AVG`'s `f64` with a small
/// epsilon (floating-point accumulation order can differ trivially
/// between this reference model and the real executor's own row-by-row
/// `AggregateState::update`, item 43 — both are legitimate IEEE-754
/// results of the same mathematical sum/count, never a correctness gap).
fn assert_groups_match(actual: &[GroupResult], expected: &[GroupResult], context: &str) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "{context}: group count mismatch\nactual={actual:?}\nexpected={expected:?}"
    );
    for (a, e) in actual.iter().zip(expected.iter()) {
        assert_eq!(a.0, e.0, "{context}: group key mismatch");
        assert_eq!(a.1, e.1, "{context}: COUNT(*) mismatch for group {:?}", a.0);
        assert_eq!(
            a.2, e.2,
            "{context}: COUNT(val) mismatch for group {:?}",
            a.0
        );
        assert_eq!(a.3, e.3, "{context}: SUM(val) mismatch for group {:?}", a.0);
        assert_eq!(a.4, e.4, "{context}: MIN(val) mismatch for group {:?}", a.0);
        assert_eq!(a.5, e.5, "{context}: MAX(val) mismatch for group {:?}", a.0);
        match (a.6, e.6) {
            (None, None) => {}
            (Some(x), Some(y)) => assert!(
                (x - y).abs() < 1e-9,
                "{context}: AVG(val) mismatch for group {:?}: {x} vs {y}",
                a.0
            ),
            _ => panic!("{context}: AVG(val) presence mismatch for group {:?}", a.0),
        }
    }
}

// =======================================================================
// Fixed scenario matrix (item 53) — NULLs, duplicates, empty input,
// composite/high-cardinality groups, all deterministic (no random seed).
// =======================================================================

#[test]
fn matches_reference_model_across_a_fixed_scenario_matrix() {
    let scenarios: Vec<Vec<Row>> = vec![
        // Empty input.
        vec![],
        // Single row.
        vec![(Some(1), Some(10))],
        // Duplicates within one group.
        vec![
            (Some(1), Some(10)),
            (Some(1), Some(20)),
            (Some(1), Some(30)),
        ],
        // Multiple groups, no NULLs.
        vec![
            (Some(1), Some(10)),
            (Some(2), Some(20)),
            (Some(1), Some(30)),
            (Some(2), Some(40)),
        ],
        // NULL grouping key -- every NULL-group row belongs to one group.
        vec![
            (None, Some(1)),
            (None, Some(2)),
            (Some(1), Some(3)),
            (None, None),
        ],
        // NULL values within a real group (some-NULL / all-NULL mixes).
        vec![
            (Some(1), None),
            (Some(1), None),
            (Some(2), Some(5)),
            (Some(2), None),
        ],
        // Negative and zero values.
        vec![(Some(1), Some(-5)), (Some(1), Some(0)), (Some(1), Some(5))],
    ];

    for (i, scenario) in scenarios.into_iter().enumerate() {
        let fx = AggFixture::new(&format!("agg_ref_matrix_{i}"));
        fx.insert(&scenario);
        let actual = fx.run_grouped_aggregate();
        let expected = reference_aggregate(&scenario);
        assert_groups_match(&actual, &expected, &format!("scenario {i}"));
        fx.cleanup();
    }
}

#[test]
fn matches_reference_model_for_high_cardinality_groups() {
    // item 31: high-cardinality group-state stress, not just a low-
    // cardinality boolean-shaped column.
    let mut rows: Vec<Row> = Vec::new();
    for g in 0..2_000i32 {
        rows.push((Some(g), Some(g * 2)));
        rows.push((Some(g), Some(g * 2 + 1)));
    }
    let fx = AggFixture::new("agg_ref_high_cardinality");
    fx.insert(&rows);
    let actual = fx.run_grouped_aggregate();
    let expected = reference_aggregate(&rows);
    assert_eq!(actual.len(), 2_000);
    assert_groups_match(&actual, &expected, "high cardinality");
    fx.cleanup();
}

// =======================================================================
// Property testing (item 52) — bounded random tables, deterministic seed
// (the default `proptest` seed/config this crate's other property tests,
// `reference_model.rs`/`plan_reference_model.rs`, already rely on).
// =======================================================================

fn arb_row() -> impl Strategy<Value = Row> {
    (prop::option::of(-4i32..=4), prop::option::of(-100i32..=100))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn matches_reference_model_for_random_tables(rows in prop::collection::vec(arb_row(), 0..80)) {
        let fx = AggFixture::new("agg_ref_prop");
        fx.insert(&rows);
        let actual = fx.run_grouped_aggregate();
        let expected = reference_aggregate(&rows);
        assert_groups_match(&actual, &expected, "property test");
        fx.cleanup();
    }
}

// =======================================================================
// SQL-mission-2: GROUPING SETS / ROLLUP / CUBE / GROUPING(), aggregate
// FILTER and DISTINCT aggregates -- the same independent-oracle discipline.
//
// `reference` below groups with plain `Vec`s and linear searches (no hashing,
// so no shared notion of key equality with the executor), builds each grouping
// set's groups in first-seen order and emits set after set -- the order this
// implementation documents -- and computes every aggregate from the group's
// member rows by definition. It never calls `crate::exec`, `crate::plan`,
// `crate::aggregate` or `crate::bound`.
// =======================================================================

mod mission2 {
    use super::*;
    use crate::aggregation_tests::{ag_db, seed_ag};
    use crate::mission1_support::Db;

    /// One input row: `(a, b, x, y)`.
    type In = (Option<i32>, Option<i32>, Option<i32>, Option<i32>);

    #[derive(Debug, Clone, PartialEq)]
    struct Out {
        a: Option<i32>,
        b: Option<i32>,
        grouping_a: i32,
        grouping_b: i32,
        sum: Option<i64>,
        count_star: i64,
        count_distinct: i64,
        sum_distinct: Option<i64>,
        sum_filter: Option<i64>,
        count_filter: i64,
    }

    /// A group's key `(a, b)` and the indexes of its member rows.
    type GroupMembers = ((Option<i32>, Option<i32>), Vec<usize>);

    /// `sets`: grouping sets over the columns {0 = a, 1 = b}, in output order.
    fn reference(rows: &[In], sets: &[Vec<usize>]) -> Vec<Out> {
        let mut out = Vec::new();
        for set in sets {
            let (has_a, has_b) = (set.contains(&0), set.contains(&1));
            // Groups in first-seen order: (key, member row indexes).
            let mut groups: Vec<GroupMembers> = Vec::new();
            for (i, r) in rows.iter().enumerate() {
                let key = (if has_a { r.0 } else { None }, if has_b { r.1 } else { None });
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, members)) => members.push(i),
                    None => groups.push((key, vec![i])),
                }
            }
            // The empty set is a global aggregate: one group even over no rows.
            if set.is_empty() && groups.is_empty() {
                groups.push(((None, None), vec![]));
            }
            for (key, members) in groups {
                let xs: Vec<i64> = members.iter().filter_map(|&i| rows[i].2.map(i64::from)).collect();
                let mut distinct = xs.clone();
                distinct.sort_unstable();
                distinct.dedup();
                let passing: Vec<usize> = members
                    .iter()
                    .copied()
                    .filter(|&i| rows[i].3.is_some_and(|y| y > 0))
                    .collect();
                let fx: Vec<i64> = passing.iter().filter_map(|&i| rows[i].2.map(i64::from)).collect();
                out.push(Out {
                    a: key.0,
                    b: key.1,
                    grouping_a: i32::from(!has_a),
                    grouping_b: i32::from(!has_b),
                    sum: (!xs.is_empty()).then(|| xs.iter().sum()),
                    count_star: members.len() as i64,
                    count_distinct: distinct.len() as i64,
                    sum_distinct: (!distinct.is_empty()).then(|| distinct.iter().sum()),
                    sum_filter: (!fx.is_empty()).then(|| fx.iter().sum()),
                    count_filter: passing.len() as i64,
                });
            }
        }
        out
    }

    fn insert(db: &Db, rows: &[In]) {
        let seed: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, (a, b, x, y))| (i as i32 + 1, *a, *b, *x, *y))
            .collect();
        seed_ag(db, &seed);
    }

    fn opt32(v: &Option<RelationalValue>) -> Option<i32> {
        match v {
            None => None,
            Some(RelationalValue::Integer(n)) => Some(*n),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn opt64(v: &Option<RelationalValue>) -> Option<i64> {
        match v {
            None => None,
            Some(RelationalValue::Bigint(n)) => Some(*n),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn run(db: &Db, clause: &str) -> Vec<Out> {
        let sql = format!(
            "SELECT a, b, GROUPING(a), GROUPING(b), SUM(x), COUNT(*), COUNT(DISTINCT x), SUM(DISTINCT x), \
             SUM(x) FILTER (WHERE y > 0), COUNT(*) FILTER (WHERE y > 0) FROM ag GROUP BY {clause}"
        );
        db.rows(&sql)
            .iter()
            .map(|r| Out {
                a: opt32(&r[0]),
                b: opt32(&r[1]),
                grouping_a: opt32(&r[2]).unwrap(),
                grouping_b: opt32(&r[3]).unwrap(),
                sum: opt64(&r[4]),
                count_star: opt64(&r[5]).unwrap(),
                count_distinct: opt64(&r[6]).unwrap(),
                sum_distinct: opt64(&r[7]),
                sum_filter: opt64(&r[8]),
                count_filter: opt64(&r[9]).unwrap(),
            })
            .collect()
    }

    /// Each clause and the grouping sets it means (columns 0 = a, 1 = b).
    fn clauses() -> Vec<(&'static str, Vec<Vec<usize>>)> {
        vec![
            ("a, b", vec![vec![0, 1]]),
            ("ROLLUP (a, b)", vec![vec![0, 1], vec![0], vec![]]),
            ("ROLLUP (b, a)", vec![vec![0, 1], vec![1], vec![]]),
            ("CUBE (a, b)", vec![vec![0, 1], vec![0], vec![1], vec![]]),
            ("GROUPING SETS ((a), (b))", vec![vec![0], vec![1]]),
            ("GROUPING SETS ((a), (b), ())", vec![vec![0], vec![1], vec![]]),
            ("GROUPING SETS ((a, b), (a), (b), ())", vec![vec![0, 1], vec![0], vec![1], vec![]]),
            ("GROUPING SETS ((b, a), ())", vec![vec![0, 1], vec![]]),
            ("GROUPING SETS ((a, b), (a, b))", vec![vec![0, 1], vec![0, 1]]),
            ("a, ROLLUP (b)", vec![vec![0, 1], vec![0]]),
            ("ROLLUP (a), b", vec![vec![0, 1], vec![1]]),
            ("ROLLUP (a), ROLLUP (b)", vec![vec![0, 1], vec![0], vec![1], vec![]]),
            ("CUBE (a), ROLLUP (b)", vec![vec![0, 1], vec![0], vec![1], vec![]]),
        ]
    }

    fn scenarios() -> Vec<Vec<In>> {
        let mut v: Vec<Vec<In>> = vec![
            vec![],
            vec![(Some(1), Some(1), Some(5), Some(1))],
            vec![(None, None, None, None)],
            vec![(None, None, Some(1), Some(1)), (None, None, Some(1), Some(-1)), (None, Some(1), Some(2), None)],
            vec![
                (Some(1), Some(1), Some(1), Some(1)),
                (Some(1), Some(2), Some(2), Some(-1)),
                (Some(2), Some(1), Some(4), Some(3)),
                (Some(2), Some(1), Some(4), None),
                (None, Some(1), Some(16), Some(5)),
            ],
        ];
        // A bigger table with repeated values (DISTINCT matters) and NULLs everywhere.
        v.push(
            (0..60)
                .map(|i| {
                    (
                        if i % 7 == 0 { None } else { Some(i % 3) },
                        if i % 5 == 0 { None } else { Some(i % 4) },
                        if i % 6 == 0 { None } else { Some((i * 7) % 9) },
                        if i % 4 == 0 { None } else { Some((i % 5) - 2) },
                    )
                })
                .collect(),
        );
        v
    }

    #[test]
    fn grouping_forms_match_the_reference_model_on_a_fixed_scenario_matrix() {
        for (si, rows) in scenarios().into_iter().enumerate() {
            let db = ag_db(&format!("aggref2_matrix_{si}"));
            insert(&db, &rows);
            for (clause, sets) in clauses() {
                assert_eq!(
                    run(&db, clause),
                    reference(&rows, &sets),
                    "scenario {si}, GROUP BY {clause}\nrows: {rows:?}"
                );
            }
            db.cleanup();
        }
    }

    fn arb_row() -> impl Strategy<Value = In> {
        (
            prop::option::weighted(0.8, 0i32..3),
            prop::option::weighted(0.8, 0i32..3),
            prop::option::weighted(0.8, -3i32..6),
            prop::option::weighted(0.8, -2i32..3),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn grouping_forms_match_the_reference_model_for_random_tables(
            rows in prop::collection::vec(arb_row(), 0..40),
        ) {
            let db = ag_db("aggref2_prop");
            insert(&db, &rows);
            for (clause, sets) in clauses() {
                prop_assert_eq!(
                    run(&db, clause),
                    reference(&rows, &sets),
                    "GROUP BY {} rows {:?}", clause, rows
                );
            }
            db.cleanup();
        }
    }
}
