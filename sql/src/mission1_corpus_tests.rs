//! SQL-mission-1 statement corpus: user-visible statements and the exact
//! rows they must return on the shared seed data, run through the real
//! parse -> bind -> plan -> execute pipeline.
//!
//! Two purposes:
//!
//! 1. A cross-slice regression suite -- every slice appends its statements,
//!    so a later slice cannot silently break an earlier one.
//! 2. **Non-vacuity on the pre-mission tree.** This file (with
//!    `mission1_support.rs`) uses only APIs that exist on the pre-mission
//!    commit, so it compiles there unchanged; the new-syntax rows must FAIL
//!    on the pre-mission tree (they were `Unsupported` there) and PASS here.
//!    The recorded evidence is in `sql/mission1_evidence/`.

use crate::mission1_support::*;
use rubixdb::relational::RelationalValue;

fn render(v: &V) -> String {
    match v {
        None => "NULL".to_string(),
        Some(RelationalValue::Integer(n)) => n.to_string(),
        Some(RelationalValue::Bigint(n)) => n.to_string(),
        Some(RelationalValue::Boolean(b)) => b.to_string(),
        Some(RelationalValue::Text(s)) => format!("'{s}'"),
        Some(other) => format!("{other:?}"),
    }
}

fn render_rows(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect()
}

/// `t`: (1 alice true) (2 bob false) (3 carol NULL) (4 NULL true) (5 erin true)
/// `orders`: (10 c1 100 ->1) (11 c2 50 ->1) (12 c3 75 ->2) (13 c4 NULL ->3)
///           (14 c5 20 ->NULL)
fn seeded(tag: &str) -> Db {
    let db = Db::new(tag);
    db.seed_t(&[
        (1, Some("alice"), Some(true)),
        (2, Some("bob"), Some(false)),
        (3, Some("carol"), None),
        (4, None, Some(true)),
        (5, Some("erin"), Some(true)),
    ]);
    db.seed_orders(&[
        (10, "c1", Some(100), Some(1)),
        (11, "c2", Some(50), Some(1)),
        (12, "c3", Some(75), Some(2)),
        (13, "c4", None, Some(3)),
        (14, "c5", Some(20), None),
    ]);
    db
}

/// `(slice, sql, expected rows rendered as "a|b|c")`. Statements with no
/// `ORDER BY` are compared as sorted lists.
const CORPUS: &[(&str, &str, &[&str])] = &[
    // ---- Slice A: scalar subqueries ----
    ("A", "SELECT (SELECT MAX(amount) FROM orders)", &["100"]),
    ("A", "SELECT (SELECT amount FROM orders WHERE id = 999)", &["NULL"]),
    ("A", "SELECT (SELECT MAX(amount) FROM orders) + 1", &["101"]),
    (
        "A",
        "SELECT id FROM t WHERE id = (SELECT MIN(t_id) FROM orders)",
        &["1"],
    ),
    (
        "A",
        "SELECT id FROM t WHERE id > (SELECT MIN(id) FROM t) ORDER BY id",
        &["2", "3", "4", "5"],
    ),
    (
        "A",
        "SELECT active, COUNT(*) FROM t GROUP BY active HAVING COUNT(*) > (SELECT COUNT(*) FROM orders WHERE amount IS NULL)",
        &["true|3"],
    ),
    // ---- Slice A: IN / NOT IN ----
    (
        "A",
        "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders) ORDER BY id",
        &["1", "2", "3"],
    ),
    (
        "A",
        "SELECT id FROM t WHERE id NOT IN (SELECT t_id FROM orders)",
        &[],
    ),
    (
        "A",
        "SELECT id FROM t WHERE id NOT IN (SELECT t_id FROM orders WHERE t_id IS NOT NULL) ORDER BY id",
        &["4", "5"],
    ),
    ("A", "SELECT 9 IN (SELECT t_id FROM orders)", &["NULL"]),
    ("A", "SELECT 9 NOT IN (SELECT t_id FROM orders)", &["NULL"]),
    ("A", "SELECT NULL NOT IN (SELECT id FROM t WHERE id > 100)", &["true"]),
    ("A", "SELECT 1 IN (SELECT t_id FROM orders)", &["true"]),
    // ---- Slice A: EXISTS ----
    (
        "A",
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE amount > 90) ORDER BY id",
        &["1", "2", "3", "4", "5"],
    ),
    (
        "A",
        "SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM orders WHERE amount > 90)",
        &[],
    ),
    ("A", "SELECT EXISTS (SELECT t_id FROM orders WHERE id = 14)", &["true"]),
    // ---- Slice A: nesting ----
    (
        "A",
        "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders WHERE amount > (SELECT MIN(amount) FROM orders WHERE t_id IN (SELECT id FROM t WHERE active))) ORDER BY id",
        &["1", "2"],
    ),
    // ---- Slice B: correlated subqueries (expected rows derived by hand from
    // the seed data above, not by running the engine) ----
    (
        "B",
        "SELECT id, (SELECT MAX(amount) FROM orders WHERE orders.t_id = t.id) FROM t ORDER BY id",
        &["1|100", "2|75", "3|NULL", "4|NULL", "5|NULL"],
    ),
    (
        "B",
        "SELECT id, (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) FROM t ORDER BY id",
        &["1|2", "2|1", "3|1", "4|0", "5|0"],
    ),
    (
        "B",
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id) ORDER BY id",
        &["1", "2", "3"],
    ),
    (
        "B",
        "SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id) ORDER BY id",
        &["4", "5"],
    ),
    (
        "B",
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id AND orders.amount > 60) ORDER BY id",
        &["1", "2"],
    ),
    (
        "B",
        "SELECT id FROM t WHERE id IN (SELECT o.t_id FROM orders o WHERE o.amount > t.id * 20) ORDER BY id",
        &["1", "2"],
    ),
    // The NULL trap: every inner set contains order 14's NULL t_id.
    (
        "B",
        "SELECT id FROM t WHERE id NOT IN (SELECT o.t_id FROM orders o WHERE o.amount < t.id * 30 OR o.amount IS NULL) ORDER BY id",
        &[],
    ),
    (
        "B",
        "SELECT id FROM t WHERE id NOT IN (SELECT o.t_id FROM orders o WHERE (o.amount < t.id * 30 OR o.amount IS NULL) AND o.t_id IS NOT NULL) ORDER BY id",
        &["1", "2", "4", "5"],
    ),
    (
        "B",
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id AND EXISTS (SELECT 1 FROM t t2 WHERE t2.id = o.t_id AND t2.id < 3)) ORDER BY id",
        &["1", "2"],
    ),
    (
        "B",
        "SELECT id, (SELECT t.id * 10) FROM t ORDER BY id",
        &["1|10", "2|20", "3|30", "4|40", "5|50"],
    ),
    (
        "B",
        "SELECT id, (SELECT CASE WHEN COUNT(*) = 0 THEN t.id ELSE 0 END FROM orders WHERE orders.t_id = t.id) FROM t ORDER BY id",
        &["1|0", "2|0", "3|0", "4|4", "5|5"],
    ),
    // ---- Slice C: derived tables, LATERAL, non-recursive CTEs (expected rows
    // derived by hand from the seed data above, not by running the engine) ----
    (
        "C",
        "SELECT x FROM (SELECT id AS x FROM t WHERE id > 2) AS d ORDER BY x",
        &["3", "4", "5"],
    ),
    (
        "C",
        "SELECT g.t_id, g.n FROM (SELECT t_id, COUNT(*) AS n FROM orders WHERE t_id IS NOT NULL GROUP BY t_id) AS g ORDER BY g.t_id",
        &["1|2", "2|1", "3|1"],
    ),
    (
        "C",
        "SELECT t.id, g.n FROM t LEFT JOIN (SELECT t_id, COUNT(*) AS n FROM orders GROUP BY t_id) AS g ON g.t_id = t.id ORDER BY t.id",
        &["1|2", "2|1", "3|1", "4|NULL", "5|NULL"],
    ),
    (
        "C",
        "SELECT b FROM (SELECT id, id * 2 FROM t WHERE id <= 2) AS d(a, b) ORDER BY b",
        &["2", "4"],
    ),
    (
        "C",
        "WITH x AS (SELECT id FROM t WHERE id <= 2) SELECT id FROM x ORDER BY id",
        &["1", "2"],
    ),
    (
        "C",
        "WITH a AS (SELECT id FROM t WHERE id >= 2), b AS (SELECT id FROM a WHERE id <= 4) SELECT id FROM b ORDER BY id",
        &["2", "3", "4"],
    ),
    (
        "C",
        "WITH x AS (SELECT t_id FROM orders WHERE t_id IS NOT NULL) SELECT COUNT(*) FROM x AS a JOIN x AS b ON a.t_id = b.t_id",
        &["6"],
    ),
    ("C", "WITH orders AS (SELECT 7 AS q) SELECT q FROM orders", &["7"]),
    (
        "C",
        "WITH x AS (SELECT id FROM t WHERE id > 100) SELECT COUNT(*) FROM x",
        &["0"],
    ),
    (
        "C",
        "SELECT t.id, l.amt FROM t LEFT JOIN LATERAL (SELECT orders.amount AS amt FROM orders WHERE orders.t_id = t.id ORDER BY orders.amount DESC LIMIT 1) AS l ON true ORDER BY t.id",
        &["1|100", "2|75", "3|NULL", "4|NULL", "5|NULL"],
    ),
    (
        "C",
        "SELECT id, (WITH m AS (SELECT MAX(amount) AS mx FROM orders) SELECT mx FROM m) FROM t WHERE id <= 2 ORDER BY id",
        &["1|100", "2|100"],
    ),
];

#[test]
fn corpus_statements_return_exactly_the_expected_rows() {
    let db = seeded("corpus");
    let mut failures = Vec::new();
    for (slice, sql, expected) in CORPUS {
        let ordered = sql.contains("ORDER BY");
        let got = match db.try_run(sql) {
            Ok(r) => r.rows,
            Err(e) => {
                failures.push(format!("[{slice}] {sql}\n    error: {e}"));
                continue;
            }
        };
        let mut got = render_rows(&got);
        let mut want: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        if !ordered {
            got.sort();
            want.sort();
        }
        if got != want {
            failures.push(format!("[{slice}] {sql}\n    got:  {got:?}\n    want: {want:?}"));
        }
    }
    db.cleanup();
    assert!(
        failures.is_empty(),
        "{} corpus statement(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
