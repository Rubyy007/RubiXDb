//! SQL-mission-2 statement corpus: the canonical examples of the mission
//! prompt (and the cases around them) with the exact rows each must return on
//! one shared seed, run through the real parse -> bind -> plan -> execute
//! pipeline.
//!
//! Like the Mission 1 corpus it is a cross-sub-slice regression suite, and it
//! uses only APIs that exist on the Mission-1-complete tree
//! (`mission1_support`), so it compiles there unchanged: every row below is
//! `Unsupported` (window functions, `QUALIFY`, grouping extensions, `FILTER`,
//! `DISTINCT` aggregates) on that tree and must FAIL there and PASS here. The
//! recorded evidence is in `sql/mission2_evidence/`.

use crate::mission1_support::*;
use rubixdb::relational::RelationalValue;

fn render(v: &V) -> String {
    match v {
        None => "NULL".to_string(),
        Some(RelationalValue::Integer(n)) => n.to_string(),
        Some(RelationalValue::Bigint(n)) => n.to_string(),
        Some(RelationalValue::Double(f)) => format!("{f}"),
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

/// `ct(id INTEGER PK, g INTEGER, a INTEGER, b INTEGER, x DOUBLE, y INTEGER)`:
///
/// | id | g | a | b | x | y |
/// |---|---|---|---|---|---|
/// | 1 | 1 | 1 | 1 | 10 | 1 |
/// | 2 | 1 | 1 | 2 | 20 | -1 |
/// | 3 | 1 | 2 | 1 | 70 | 2 |
/// | 4 | 2 | 2 | 1 | 40 | 0 |
/// | 5 | 2 | NULL | 2 | 40 | 3 |
/// | 6 | 2 | NULL | NULL | 20 | -2 |
fn seeded(tag: &str) -> Db {
    let db = Db::new(tag);
    db.write("CREATE TABLE ct (id INTEGER PRIMARY KEY, g INTEGER, a INTEGER, b INTEGER, x DOUBLE, y INTEGER)");
    db.write(
        "INSERT INTO ct VALUES (1, 1, 1, 1, 10.0, 1), (2, 1, 1, 2, 20.0, -1), (3, 1, 2, 1, 70.0, 2), \
         (4, 2, 2, 1, 40.0, 0), (5, 2, NULL, 2, 40.0, 3), (6, 2, NULL, NULL, 20.0, -2)",
    );
    db
}

/// `(sub-slice, sql, expected rows rendered as "a|b|c")`, compared in order:
/// every statement either has an `ORDER BY` or has a result order this
/// implementation defines (a window operator keeps its input order; grouping
/// sets come out set by set, the groups of a set in first-seen order).
const CORPUS: &[(&str, &str, &[&str])] = &[
    // ---- canonical: window functions ----
    (
        "2A",
        "SELECT id, SUM(x) OVER (ORDER BY id) FROM ct",
        &["1|10", "2|30", "3|100", "4|140", "5|180", "6|200"],
    ),
    (
        "2B",
        "SELECT id, AVG(x) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM ct",
        &[
            "1|10",
            "2|15",
            "3|33.333333333333336",
            "4|43.333333333333336",
            "5|50",
            "6|33.333333333333336",
        ],
    ),
    (
        "2A",
        "SELECT g, x, RANK() OVER (PARTITION BY g ORDER BY x DESC) FROM ct",
        &["1|10|3", "1|20|2", "1|70|1", "2|40|1", "2|40|1", "2|20|3"],
    ),
    (
        "2C",
        "SELECT id, x, LAG(x) OVER (ORDER BY id) FROM ct",
        &["1|10|NULL", "2|20|10", "3|70|20", "4|40|70", "5|40|40", "6|20|40"],
    ),
    (
        "2C",
        "SELECT g, x, x / SUM(x) OVER (PARTITION BY g) * 100 FROM ct",
        &["1|10|10", "1|20|20", "1|70|70", "2|40|40", "2|40|40", "2|20|20"],
    ),
    // ---- window functions: the rest of the family ----
    (
        "2A",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY g), RANK() OVER (ORDER BY g), DENSE_RANK() OVER (ORDER BY g) FROM ct",
        &["1|1|1|1", "2|2|1|1", "3|3|1|1", "4|4|4|2", "5|5|4|2", "6|6|4|2"],
    ),
    (
        "2A",
        "SELECT id, SUM(x) OVER (ORDER BY g) FROM ct",
        &["1|100", "2|100", "3|100", "4|200", "5|200", "6|200"],
    ),
    (
        "2A",
        "SELECT id, COUNT(*) OVER (PARTITION BY a) FROM ct",
        &["1|2", "2|2", "3|2", "4|2", "5|2", "6|2"],
    ),
    (
        "2B",
        "SELECT id, SUM(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM ct",
        &["1|30", "2|100", "3|130", "4|150", "5|100", "6|60"],
    ),
    (
        "2B",
        "SELECT id, SUM(x) OVER (ORDER BY x RANGE BETWEEN 10 PRECEDING AND 10 FOLLOWING) FROM ct",
        &["1|50", "2|50", "3|70", "4|80", "5|80", "6|50"],
    ),
    (
        "2C",
        "SELECT id, LAST_VALUE(x) OVER (ORDER BY id), LAST_VALUE(x) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM ct",
        &["1|10|20", "2|20|20", "3|70|20", "4|40|20", "5|40|20", "6|20|20"],
    ),
    (
        "2C",
        "SELECT id, NTILE(4) OVER (ORDER BY id), PERCENT_RANK() OVER (ORDER BY g), CUME_DIST() OVER (ORDER BY g) FROM ct",
        &["1|1|0|0.5", "2|1|0|0.5", "3|2|0|0.5", "4|2|0.6|1", "5|3|0.6|1", "6|4|0.6|1"],
    ),
    (
        "2D",
        "SELECT id FROM ct QUALIFY ROW_NUMBER() OVER (PARTITION BY g ORDER BY x DESC, id) = 1",
        &["3", "4"],
    ),
    (
        "2D",
        "SELECT id, RANK() OVER (PARTITION BY g ORDER BY x DESC) AS rk FROM ct QUALIFY rk = 1",
        &["3|1", "4|1", "5|1"],
    ),
    // ---- canonical: advanced aggregation ----
    (
        "2E",
        "SELECT a, b, SUM(x) FROM ct GROUP BY GROUPING SETS ((a, b), (a), ())",
        &[
            "1|1|10", "1|2|20", "2|1|110", "NULL|2|40", "NULL|NULL|20", "1|NULL|30", "2|NULL|110",
            "NULL|NULL|60", "NULL|NULL|200",
        ],
    ),
    (
        "2E",
        "SELECT a, b, SUM(x) FROM ct GROUP BY ROLLUP (a, b)",
        &[
            "1|1|10", "1|2|20", "2|1|110", "NULL|2|40", "NULL|NULL|20", "1|NULL|30", "2|NULL|110",
            "NULL|NULL|60", "NULL|NULL|200",
        ],
    ),
    (
        "2E",
        "SELECT a, b, SUM(x) FROM ct GROUP BY CUBE (a, b)",
        &[
            "1|1|10", "1|2|20", "2|1|110", "NULL|2|40", "NULL|NULL|20", "1|NULL|30", "2|NULL|110",
            "NULL|NULL|60", "NULL|1|120", "NULL|2|60", "NULL|NULL|20", "NULL|NULL|200",
        ],
    ),
    (
        "2E",
        "SELECT a, GROUPING(a) AS g, SUM(x) FROM ct GROUP BY ROLLUP (a)",
        &["1|0|30", "2|0|110", "NULL|0|60", "NULL|1|200"],
    ),
    (
        "2E",
        "SELECT a, b, GROUPING(a, b), SUM(x) FROM ct GROUP BY CUBE (a, b) HAVING GROUPING(a, b) >= 2 ORDER BY GROUPING(a, b), b",
        &["NULL|1|2|120", "NULL|2|2|60", "NULL|NULL|2|20", "NULL|NULL|3|200"],
    ),
    // ---- canonical: FILTER and DISTINCT ----
    (
        "2F",
        "SELECT SUM(x) FILTER (WHERE y > 0), COUNT(*) FROM ct",
        &["120|6"],
    ),
    (
        "2F",
        "SELECT g, COUNT(*) FILTER (WHERE y > 0), COUNT(*) FILTER (WHERE y < 0), COUNT(*) FROM ct GROUP BY g ORDER BY g",
        &["1|2|1|3", "2|1|1|3"],
    ),
    (
        "2F",
        "SELECT g, COUNT(DISTINCT x) FROM ct GROUP BY g ORDER BY g",
        &["1|3", "2|2"],
    ),
    (
        "2F",
        "SELECT g, SUM(DISTINCT x), SUM(x), AVG(DISTINCT x) FROM ct GROUP BY g ORDER BY g",
        &["1|100|100|33.333333333333336", "2|60|100|30"],
    ),
    // ---- the families together ----
    (
        "2E+2A",
        "SELECT a, SUM(x), RANK() OVER (ORDER BY SUM(x) DESC) FROM ct GROUP BY ROLLUP (a) ORDER BY a NULLS FIRST, RANK() OVER (ORDER BY SUM(x) DESC)",
        &["NULL|200|1", "NULL|60|3", "1|30|4", "2|110|2"],
    ),
];

fn run_corpus(tag: &str, only: Option<&str>) {
    let db = seeded(tag);
    let mut failures = Vec::new();
    let mut ran = 0;
    for (slice, sql, expected) in CORPUS {
        if only.is_some_and(|o| !slice.starts_with(o)) || expected.is_empty() {
            continue;
        }
        ran += 1;
        match db.try_run(sql) {
            Ok(result) => {
                let got = render_rows(&result.rows);
                let want: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
                if got != want {
                    failures.push(format!(
                        "[{slice}] {sql}\n   got  {got:?}\n   want {want:?}"
                    ));
                }
            }
            Err(e) => failures.push(format!("[{slice}] {sql}\n   error: {e}")),
        }
    }
    db.cleanup();
    assert!(
        failures.is_empty(),
        "{} of {ran} corpus statements failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn the_mission_2_corpus_returns_the_expected_rows() {
    run_corpus("m2_corpus", None);
}

#[test]
fn every_sub_slice_has_corpus_statements() {
    for slice in ["2A", "2B", "2C", "2D", "2E", "2F"] {
        assert!(
            CORPUS
                .iter()
                .any(|(s, _, e)| s.starts_with(slice) && !e.is_empty()),
            "no corpus statement for {slice}"
        );
    }
}
