//! SQL Phase 1, window functions: `GROUPS` frames, `IGNORE NULLS`, and
//! `DISTINCT` window aggregates (matrix rows E-9, E-10).
//!
//! The differential tests compare the real pipeline with an **oracle written
//! from the definitions** (PostgreSQL 18 "Window Function Calls" for frames and
//! `GROUPS`; Snowflake for `IGNORE NULLS`): for every row, the oracle builds the
//! row's partition, orders it, numbers its peer groups, and collects the frame
//! by testing every row of the partition against the two bound conditions. It
//! shares no code with `crate::exec`, `crate::bind` or `crate::aggregate`.

use crate::mission1_support::*;

use rubixdb::relational::RelationalValue;

use crate::error::SqlError;
use crate::exec::{ExecLimits, ExecMetrics};
use crate::limits::SqlLimits;
use crate::parse::parse_statement;
use crate::setop_tests::bound;
use crate::window_tests::{col, dbl, seed_w, win_db};

fn r(vals: &[V]) -> Row {
    vals.to_vec()
}

/// `w(id, g, o, x)` rows 1..=6 with a single (NULL) partition key, `o` the
/// ordering key and `x = id`.
fn tied(tag: &str, os: &[i32]) -> Db {
    let db = win_db(tag);
    let rows: Vec<_> = os
        .iter()
        .enumerate()
        .map(|(i, o)| (i as i32 + 1, None, Some(*o), Some(i as i32 + 1)))
        .collect();
    seed_w(&db, &rows);
    db
}

fn one_column(db: &Db, sql: &str) -> Vec<V> {
    col(&db.rows(sql), 0)
}

fn bigs_or_null(vals: &[Option<i64>]) -> Vec<V> {
    vals.iter()
        .map(|v| v.map(|n| Some(RelationalValue::Bigint(n))).unwrap_or(None))
        .collect()
}

// =======================================================================
// GROUPS frames
// =======================================================================

mod groups {
    use super::*;

    // o = 1,1,2,3,3,3 -> peer groups {1,2} {3} {4,5,6}; x = id.
    fn db(tag: &str) -> Db {
        tied(tag, &[1, 1, 2, 3, 3, 3])
    }

    fn sums(db: &Db, frame: &str) -> Vec<V> {
        one_column(
            db,
            &format!("SELECT SUM(x) OVER (ORDER BY o GROUPS {frame}) FROM w ORDER BY id"),
        )
    }

    #[test]
    fn offsets_count_peer_groups_not_rows() {
        let db = db("p1_g_basic");
        let n = |v: &[i64]| bigs_or_null(&v.iter().map(|x| Some(*x)).collect::<Vec<_>>());
        assert_eq!(
            sums(&db, "BETWEEN 1 PRECEDING AND CURRENT ROW"),
            n(&[3, 3, 6, 18, 18, 18])
        );
        assert_eq!(
            sums(&db, "BETWEEN CURRENT ROW AND 1 FOLLOWING"),
            n(&[6, 6, 18, 15, 15, 15])
        );
        assert_eq!(
            sums(&db, "BETWEEN 1 PRECEDING AND 1 FOLLOWING"),
            n(&[6, 6, 21, 18, 18, 18])
        );
        // the frame is the peers alone
        assert_eq!(
            sums(&db, "BETWEEN 0 PRECEDING AND 0 FOLLOWING"),
            n(&[3, 3, 3, 15, 15, 15])
        );
        assert_eq!(
            sums(&db, "BETWEEN CURRENT ROW AND CURRENT ROW"),
            n(&[3, 3, 3, 15, 15, 15])
        );
        // beyond the number of groups: the whole partition
        assert_eq!(
            sums(&db, "BETWEEN 10 PRECEDING AND 10 FOLLOWING"),
            n(&[21, 21, 21, 21, 21, 21])
        );
        // unbounded
        assert_eq!(
            sums(&db, "BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"),
            n(&[3, 3, 6, 21, 21, 21])
        );
        assert_eq!(
            sums(&db, "BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING"),
            n(&[21, 21, 18, 15, 15, 15])
        );
        // ROWS, for contrast, counts rows
        assert_eq!(
            one_column(&db, "SELECT SUM(x) OVER (ORDER BY o ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w ORDER BY id"),
            n(&[1, 3, 5, 7, 9, 11])
        );
        db.cleanup();
    }

    #[test]
    fn the_shorthand_is_between_the_bound_and_current_row() {
        let db = db("p1_g_short");
        assert_eq!(
            sums(&db, "1 PRECEDING"),
            sums(&db, "BETWEEN 1 PRECEDING AND CURRENT ROW")
        );
        assert_eq!(
            sums(&db, "UNBOUNDED PRECEDING"),
            sums(&db, "BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW")
        );
        db.cleanup();
    }

    #[test]
    fn frames_that_miss_the_partition_are_empty_and_aggregate_as_empty_input() {
        let db = db("p1_g_empty");
        // 1 PRECEDING AND 1 PRECEDING: the previous group alone
        assert_eq!(
            sums(&db, "BETWEEN 1 PRECEDING AND 1 PRECEDING"),
            bigs_or_null(&[None, None, Some(3), Some(3), Some(3), Some(3)])
        );
        assert_eq!(
            one_column(&db, "SELECT COUNT(x) OVER (ORDER BY o GROUPS BETWEEN 1 PRECEDING AND 1 PRECEDING) FROM w ORDER BY id"),
            bigs_or_null(&[Some(0), Some(0), Some(2), Some(1), Some(1), Some(1)])
        );
        // 1 FOLLOWING AND 2 FOLLOWING
        assert_eq!(
            sums(&db, "BETWEEN 1 FOLLOWING AND 2 FOLLOWING"),
            bigs_or_null(&[Some(18), Some(18), Some(15), None, None, None])
        );
        // 5 FOLLOWING AND 6 FOLLOWING: nothing for anyone
        assert_eq!(
            sums(&db, "BETWEEN 5 FOLLOWING AND 6 FOLLOWING"),
            bigs_or_null(&[None; 6])
        );
        db.cleanup();
    }

    #[test]
    fn descending_order_and_null_keys() {
        let db = db("p1_g_desc");
        // o DESC: groups {4,5,6} {3} {1,2}
        let desc = one_column(
            &db,
            "SELECT SUM(x) OVER (ORDER BY o DESC GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w ORDER BY id",
        );
        assert_eq!(
            desc,
            bigs_or_null(&[Some(6), Some(6), Some(18), Some(15), Some(15), Some(15)])
        );
        // NULL keys are one peer group, last ascending: o = 1, 2, NULL, NULL
        let db2 = win_db("p1_g_null");
        seed_w(
            &db2,
            &[
                (1, None, Some(1), Some(1)),
                (2, None, None, Some(2)),
                (3, None, Some(2), Some(3)),
                (4, None, None, Some(4)),
            ],
        );
        let rows = one_column(
            &db2,
            "SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w ORDER BY id",
        );
        // groups: {1}={id1}, {2}={id3}, {NULL}={id2,id4}
        assert_eq!(
            rows,
            bigs_or_null(&[Some(1), Some(3 + 2 + 4), Some(1 + 3), Some(3 + 2 + 4)])
        );
        // NULLS FIRST moves the group to the front
        let rows = one_column(
            &db2,
            "SELECT SUM(x) OVER (ORDER BY o NULLS FIRST GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w ORDER BY id",
        );
        // groups: {id2,id4} {id1} {id3}
        assert_eq!(
            rows,
            bigs_or_null(&[Some(2 + 4 + 1), Some(6), Some(1 + 3), Some(6)])
        );
        db.cleanup();
        db2.cleanup();
    }

    #[test]
    fn with_partitions_and_value_functions() {
        let db = win_db("p1_g_part");
        seed_w(
            &db,
            &[
                (1, Some(1), Some(1), Some(10)),
                (2, Some(1), Some(1), Some(20)),
                (3, Some(1), Some(2), Some(30)),
                (4, Some(2), Some(5), Some(40)),
                (5, Some(2), Some(6), Some(50)),
            ],
        );
        let rows = db.rows(
            "SELECT id, SUM(x) OVER w, FIRST_VALUE(x) OVER w, LAST_VALUE(x) OVER w, COUNT(*) OVER w \
             FROM w WINDOW w AS (PARTITION BY g ORDER BY o GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) ORDER BY id",
        );
        // partition 1: groups {1,2} {3}; partition 2: groups {4} {5}
        assert_eq!(
            rows,
            vec![
                r(&[int(1), big(30), int(10), int(20), big(2)]),
                r(&[int(2), big(30), int(10), int(20), big(2)]),
                r(&[int(3), big(60), int(10), int(30), big(3)]),
                r(&[int(4), big(40), int(40), int(40), big(1)]),
                r(&[int(5), big(90), int(40), int(50), big(2)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn invalid_frames_are_structured_errors() {
        let db = db("p1_g_err");
        for (sql, what) in [
            // a GROUPS frame needs the peers an ORDER BY defines
            ("SELECT SUM(x) OVER (GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w", "no ORDER BY"),
            ("SELECT SUM(x) OVER (PARTITION BY g GROUPS CURRENT ROW) FROM w", "no ORDER BY"),
            // offsets
            ("SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN -1 PRECEDING AND CURRENT ROW) FROM w", "negative"),
            ("SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN 1.5 PRECEDING AND CURRENT ROW) FROM w", "not an integer"),
            ("SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN x PRECEDING AND CURRENT ROW) FROM w", "not a constant"),
            // the standard's shape rules, as for ROWS
            ("SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW) FROM w", "start at UNBOUNDED FOLLOWING"),
            ("SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN CURRENT ROW AND UNBOUNDED PRECEDING) FROM w", "end at UNBOUNDED PRECEDING"),
            ("SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN CURRENT ROW AND 1 PRECEDING) FROM w", "CURRENT ROW start, N PRECEDING end"),
            ("SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN 1 FOLLOWING AND CURRENT ROW) FROM w", "N FOLLOWING start, CURRENT ROW end"),
            // ranking functions take no frame
            ("SELECT RANK() OVER (ORDER BY o GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w", "frame on RANK"),
        ] {
            let err = match parse_statement(sql, &SqlLimits::default()) {
                Err(e) => e,
                Ok(_) => db.try_bind(sql).unwrap_err(),
            };
            assert!(
                matches!(err, SqlError::TypeMismatch { .. } | SqlError::Unsupported { .. } | SqlError::Parse { .. }),
                "{what}: {sql}: {err:?}"
            );
        }
        // NULL and negative offsets that only become known at run time
        let err = db
            .try_run("SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN NULL PRECEDING AND CURRENT ROW) FROM w")
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        let err = db
            .try_run_with(
                "SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN $1 PRECEDING AND CURRENT ROW) FROM w",
                &ExecLimits::default(),
                &ExecMetrics::default(),
                &[int(-1)],
            )
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        // and a parameter offset that is fine
        let ok = db
            .try_run_with(
                "SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN $1 PRECEDING AND CURRENT ROW) FROM w ORDER BY id",
                &ExecLimits::default(),
                &ExecMetrics::default(),
                &[int(1)],
            )
            .unwrap();
        assert_eq!(
            col(&ok.rows, 0),
            bigs_or_null(&[Some(3), Some(3), Some(6), Some(18), Some(18), Some(18)])
        );
        db.cleanup();
    }

    #[test]
    fn frame_exclusion_stays_refused_because_the_parser_has_no_such_syntax() {
        // sqlparser 0.63 documents "TBD: EXCLUDE" on its WindowFrame; a statement
        // that uses it must be a typed error, never a silently ignored clause.
        let db = db("p1_g_exclude");
        for sql in [
            "SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW) FROM w",
            "SELECT SUM(x) OVER (ORDER BY o ROWS BETWEEN 1 PRECEDING AND CURRENT ROW EXCLUDE TIES) FROM w",
        ] {
            let err = parse_statement(sql, &SqlLimits::default()).unwrap_err();
            assert!(
                matches!(err, SqlError::Unsupported { .. } | SqlError::Parse { .. }),
                "{sql}: {err:?}"
            );
        }
        db.cleanup();
    }

    #[test]
    fn explain_names_the_units() {
        let db = db("p1_g_explain");
        let text = crate::plan::explain(&db.plan(
            "SELECT SUM(x) OVER (ORDER BY o GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM w",
        ));
        assert!(text.contains("frame=GROUPS BETWEEN"), "{text}");
        db.cleanup();
    }
}

// =======================================================================
// IGNORE NULLS
// =======================================================================

mod ignore_nulls {
    use super::*;

    // x = NULL, 5, NULL, NULL, 7, NULL ordered by id (= o)
    fn db(tag: &str) -> Db {
        let db = win_db(tag);
        seed_w(
            &db,
            &[
                (1, None, Some(1), None),
                (2, None, Some(2), Some(5)),
                (3, None, Some(3), None),
                (4, None, Some(4), None),
                (5, None, Some(5), Some(7)),
                (6, None, Some(6), None),
            ],
        );
        db
    }

    fn ints(vals: &[Option<i32>]) -> Vec<V> {
        vals.iter().map(|v| v.map(int).unwrap_or(NULL)).collect()
    }

    fn q(db: &Db, call: &str) -> Vec<V> {
        one_column(db, &format!("SELECT {call} FROM w ORDER BY id"))
    }

    #[test]
    fn lag_and_lead_skip_nulls() {
        let db = db("p1_in_lag");
        assert_eq!(
            q(&db, "LAG(x) IGNORE NULLS OVER (ORDER BY o)"),
            ints(&[None, None, Some(5), Some(5), Some(5), Some(7)])
        );
        // contrast: without it a NULL neighbour is the answer
        assert_eq!(
            q(&db, "LAG(x) OVER (ORDER BY o)"),
            ints(&[None, None, Some(5), None, None, Some(7)])
        );
        assert_eq!(
            q(&db, "LAG(x, 2) IGNORE NULLS OVER (ORDER BY o)"),
            ints(&[None, None, None, None, None, Some(5)])
        );
        assert_eq!(
            q(&db, "LEAD(x) IGNORE NULLS OVER (ORDER BY o)"),
            ints(&[Some(5), Some(7), Some(7), Some(7), None, None])
        );
        assert_eq!(
            q(&db, "LEAD(x, 2) IGNORE NULLS OVER (ORDER BY o)"),
            ints(&[Some(7), None, None, None, None, None])
        );
        // the default replaces "no such row"
        assert_eq!(
            q(&db, "LEAD(x, 1, -1) IGNORE NULLS OVER (ORDER BY o)"),
            ints(&[Some(5), Some(7), Some(7), Some(7), Some(-1), Some(-1)])
        );
        assert_eq!(
            q(&db, "LAG(x, 1, -1) IGNORE NULLS OVER (ORDER BY o)"),
            ints(&[Some(-1), Some(-1), Some(5), Some(5), Some(5), Some(7)])
        );
        // offset 0 is the row itself, NULL or not
        assert_eq!(
            q(&db, "LAG(x, 0) IGNORE NULLS OVER (ORDER BY o)"),
            ints(&[None, Some(5), None, None, Some(7), None])
        );
        // descending order reverses "before" and "after": the previous row is the
        // one with the larger id
        assert_eq!(
            q(&db, "LAG(x) IGNORE NULLS OVER (ORDER BY o DESC)"),
            ints(&[Some(5), Some(7), Some(7), Some(7), None, None])
        );
        db.cleanup();
    }

    #[test]
    fn first_last_and_nth_value_only_see_the_frames_non_null_rows() {
        let db = db("p1_in_first");
        // whole frame
        let whole = "ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
        assert_eq!(
            q(
                &db,
                &format!("FIRST_VALUE(x) IGNORE NULLS OVER (ORDER BY o {whole})")
            ),
            ints(&[Some(5); 6])
        );
        assert_eq!(
            q(
                &db,
                &format!("LAST_VALUE(x) IGNORE NULLS OVER (ORDER BY o {whole})")
            ),
            ints(&[Some(7); 6])
        );
        assert_eq!(
            q(
                &db,
                &format!("NTH_VALUE(x, 2) IGNORE NULLS OVER (ORDER BY o {whole})")
            ),
            ints(&[Some(7); 6])
        );
        assert_eq!(
            q(
                &db,
                &format!("NTH_VALUE(x, 3) IGNORE NULLS OVER (ORDER BY o {whole})")
            ),
            ints(&[None; 6])
        );
        // running frames
        assert_eq!(
            q(&db, "FIRST_VALUE(x) IGNORE NULLS OVER (ORDER BY o)"),
            ints(&[None, Some(5), Some(5), Some(5), Some(5), Some(5)])
        );
        assert_eq!(
            q(&db, "LAST_VALUE(x) IGNORE NULLS OVER (ORDER BY o ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)"),
            ints(&[None, Some(5), Some(5), Some(5), Some(7), Some(7)])
        );
        // a sliding frame
        assert_eq!(
            q(&db, "FIRST_VALUE(x) IGNORE NULLS OVER (ORDER BY o ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)"),
            ints(&[Some(5), Some(5), Some(5), Some(7), Some(7), Some(7)])
        );
        assert_eq!(
            q(
                &db,
                "FIRST_VALUE(x) OVER (ORDER BY o ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)"
            ),
            ints(&[None, None, Some(5), None, None, Some(7)])
        );
        // an empty frame
        assert_eq!(
            q(&db, "FIRST_VALUE(x) IGNORE NULLS OVER (ORDER BY o ROWS BETWEEN 3 FOLLOWING AND 4 FOLLOWING)"),
            ints(&[Some(7), Some(7), None, None, None, None])
        );
        db.cleanup();
    }

    #[test]
    fn respect_nulls_is_the_default_and_an_all_null_partition_gives_null_or_the_default() {
        let db = db("p1_in_respect");
        assert_eq!(
            q(&db, "LAG(x) RESPECT NULLS OVER (ORDER BY o)"),
            q(&db, "LAG(x) OVER (ORDER BY o)")
        );
        let db2 = win_db("p1_in_allnull");
        seed_w(
            &db2,
            &[
                (1, Some(1), Some(1), None),
                (2, Some(1), Some(2), None),
                (3, Some(2), Some(1), Some(9)),
            ],
        );
        let rows = db2.rows(
            "SELECT id, LAG(x, 1, -1) IGNORE NULLS OVER (PARTITION BY g ORDER BY o), \
             FIRST_VALUE(x) IGNORE NULLS OVER (PARTITION BY g ORDER BY o ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) \
             FROM w ORDER BY id",
        );
        assert_eq!(
            rows,
            vec![
                r(&[int(1), int(-1), NULL]),
                r(&[int(2), int(-1), NULL]),
                r(&[int(3), int(-1), int(9)]),
            ]
        );
        db.cleanup();
        db2.cleanup();
    }

    #[test]
    fn only_the_value_functions_accept_it() {
        let db = db("p1_in_err");
        for sql in [
            "SELECT ROW_NUMBER() IGNORE NULLS OVER (ORDER BY o) FROM w",
            "SELECT RANK() IGNORE NULLS OVER (ORDER BY o) FROM w",
            "SELECT NTILE(2) IGNORE NULLS OVER (ORDER BY o) FROM w",
            "SELECT SUM(x) IGNORE NULLS OVER (ORDER BY o) FROM w",
            "SELECT COUNT(x) IGNORE NULLS OVER (ORDER BY o) FROM w",
        ] {
            let err = match parse_statement(sql, &SqlLimits::default()) {
                Err(e) => e,
                Ok(_) => db.try_bind(sql).unwrap_err(),
            };
            assert!(
                matches!(
                    err,
                    SqlError::TypeMismatch { .. } | SqlError::Unsupported { .. }
                ),
                "{sql}: {err:?}"
            );
        }
        // BigQuery's in-parentheses spelling is a different syntax, still refused
        let err = parse_statement(
            "SELECT LAG(x IGNORE NULLS) OVER (ORDER BY o) FROM w",
            &SqlLimits::default(),
        );
        assert!(err.is_err(), "{err:?}");
        // FROM FIRST / FROM LAST is not implemented
        let err = parse_statement(
            "SELECT NTH_VALUE(x, 2) FROM LAST OVER (ORDER BY o) FROM w",
            &SqlLimits::default(),
        );
        assert!(err.is_err(), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn explain_shows_it() {
        let db = db("p1_in_explain");
        let text =
            crate::plan::explain(&db.plan("SELECT LAG(x) IGNORE NULLS OVER (ORDER BY o) FROM w"));
        assert!(text.contains("IGNORE NULLS"), "{text}");
        let b = bound(
            &db,
            "SELECT LAG(x) IGNORE NULLS OVER (ORDER BY o), LAG(x) OVER (ORDER BY o) FROM w",
        );
        // not the same call: both are kept
        assert_eq!(b.windows.len(), 2);
        db.cleanup();
    }
}

// =======================================================================
// DISTINCT window aggregates
// =======================================================================

mod window_distinct {
    use super::*;

    // o = id; x = 5, 5, 7, 5, NULL
    fn db(tag: &str) -> Db {
        let db = win_db(tag);
        seed_w(
            &db,
            &[
                (1, None, Some(1), Some(5)),
                (2, None, Some(2), Some(5)),
                (3, None, Some(3), Some(7)),
                (4, None, Some(4), Some(5)),
                (5, None, Some(5), None),
            ],
        );
        db
    }

    #[test]
    fn running_sliding_and_whole_frames() {
        let db = db("p1_wd_frames");
        let q = |frame: &str, f: &str| {
            col(
                &db.rows(&format!(
                    "SELECT {f} OVER (ORDER BY o {frame}) FROM w ORDER BY id"
                )),
                0,
            )
        };
        // the default frame (RANGE UNBOUNDED PRECEDING .. CURRENT ROW)
        assert_eq!(
            q("", "COUNT(DISTINCT x)"),
            bigs_or_null(&[Some(1), Some(1), Some(2), Some(2), Some(2)])
        );
        assert_eq!(
            q("", "SUM(DISTINCT x)"),
            bigs_or_null(&[Some(5), Some(5), Some(12), Some(12), Some(12)])
        );
        // a two-row window
        let sliding = "ROWS BETWEEN 1 PRECEDING AND CURRENT ROW";
        assert_eq!(
            q(sliding, "COUNT(DISTINCT x)"),
            bigs_or_null(&[Some(1), Some(1), Some(2), Some(2), Some(1)])
        );
        assert_eq!(
            q(sliding, "SUM(DISTINCT x)"),
            bigs_or_null(&[Some(5), Some(5), Some(12), Some(12), Some(5)])
        );
        // the whole partition
        let whole = "ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
        assert_eq!(q(whole, "COUNT(DISTINCT x)"), bigs_or_null(&[Some(2); 5]));
        // a frame ahead of the row
        assert_eq!(
            q(
                "ROWS BETWEEN CURRENT ROW AND 2 FOLLOWING",
                "COUNT(DISTINCT x)"
            ),
            bigs_or_null(&[Some(2), Some(2), Some(2), Some(1), Some(0)])
        );
        // AVG / MIN / MAX of the distinct values
        assert_eq!(q(whole, "AVG(DISTINCT x)"), vec![dbl(6.0); 5]);
        assert_eq!(q(whole, "MIN(DISTINCT x)"), vec![int(5); 5]);
        assert_eq!(q(whole, "MAX(DISTINCT x)"), vec![int(7); 5]);
        // plain COUNT(x) in the same window, for contrast
        assert_eq!(q(whole, "COUNT(x)"), bigs_or_null(&[Some(4); 5]));
        db.cleanup();
    }

    #[test]
    fn partitions_filters_and_peers() {
        let db = win_db("p1_wd_part");
        seed_w(
            &db,
            &[
                (1, Some(1), Some(1), Some(5)),
                (2, Some(1), Some(1), Some(6)),
                (3, Some(1), Some(1), Some(5)),
                (4, Some(2), Some(1), Some(5)),
                (5, Some(2), Some(2), None),
            ],
        );
        // peers share a frame (RANGE default): the first three rows tie on o
        let rows = db.rows(
            "SELECT id, COUNT(DISTINCT x) OVER (PARTITION BY g ORDER BY o), \
             COUNT(DISTINCT x) FILTER (WHERE x > 5) OVER (PARTITION BY g ORDER BY o) FROM w ORDER BY id",
        );
        assert_eq!(
            rows,
            vec![
                r(&[int(1), big(2), big(1)]),
                r(&[int(2), big(2), big(1)]),
                r(&[int(3), big(2), big(1)]),
                r(&[int(4), big(1), big(0)]),
                r(&[int(5), big(1), big(0)]),
            ]
        );
        // the same window used by a non-distinct aggregate shares its operator
        let text = crate::plan::explain(&db.plan(
            "SELECT COUNT(DISTINCT x) OVER (PARTITION BY g ORDER BY o), COUNT(x) OVER (PARTITION BY g ORDER BY o) FROM w",
        ));
        assert!(text.contains("count(DISTINCT"), "{text}");
        db.cleanup();
    }

    #[test]
    fn distinct_and_plain_calls_are_different_slots() {
        let db = db("p1_wd_slots");
        let rows = db
            .rows("SELECT COUNT(DISTINCT x) OVER (), COUNT(x) OVER () FROM w ORDER BY id LIMIT 1");
        assert_eq!(rows, vec![r(&[big(2), big(4)])]);
        db.cleanup();
    }

    #[test]
    fn the_distinct_state_is_bounded() {
        let db = win_db("p1_wd_limit");
        let rows: Vec<_> = (1..=200).map(|i| (i, None, Some(i), Some(i))).collect();
        seed_w(&db, &rows);
        let limits = ExecLimits {
            max_aggregate_state_bytes: 1_000,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with(
                "SELECT COUNT(DISTINCT x) OVER (ORDER BY o) FROM w",
                &limits,
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn the_flag_survives_binding_and_explain() {
        let db = db("p1_wd_flag");
        let b = bound(&db, "SELECT SUM(DISTINCT x) OVER () FROM w");
        assert!(b.windows.iter().any(|c| matches!(
            c.func,
            crate::bound::BoundWindowFunc::Aggregate { distinct: true, .. }
        )));
        let text = crate::plan::explain(&db.plan("SELECT SUM(DISTINCT x) OVER () FROM w"));
        assert!(text.contains("sum(DISTINCT"), "{text}");
        db.cleanup();
    }
}

// =======================================================================
// The oracle
// =======================================================================

mod oracle {
    use super::*;
    use std::cmp::Ordering;

    /// `(id, g, o, x)`.
    type R4 = (i32, Option<i32>, Option<i32>, Option<i32>);

    #[derive(Clone, Copy, Debug)]
    enum B {
        UnbPrec,
        Prec(i64),
        Cur,
        Foll(i64),
        UnbFoll,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Units {
        Rows,
        Groups,
        Range,
    }

    #[derive(Clone, Copy, Debug)]
    struct Spec {
        partition: bool,
        desc: bool,
        /// Explicit `NULLS FIRST` (`Some(true)`) / `NULLS LAST`; `None` = default.
        nulls_first: Option<bool>,
        units: Units,
        start: B,
        end: B,
    }

    impl Spec {
        fn sql(&self) -> String {
            let b = |b: B| match b {
                B::UnbPrec => "UNBOUNDED PRECEDING".to_string(),
                B::Prec(n) => format!("{n} PRECEDING"),
                B::Cur => "CURRENT ROW".to_string(),
                B::Foll(n) => format!("{n} FOLLOWING"),
                B::UnbFoll => "UNBOUNDED FOLLOWING".to_string(),
            };
            format!(
                "{}ORDER BY o{}{} {} BETWEEN {} AND {}",
                if self.partition {
                    "PARTITION BY g "
                } else {
                    ""
                },
                if self.desc { " DESC" } else { "" },
                match self.nulls_first {
                    Some(true) => " NULLS FIRST",
                    Some(false) => " NULLS LAST",
                    None => "",
                },
                match self.units {
                    Units::Rows => "ROWS",
                    Units::Groups => "GROUPS",
                    Units::Range => "RANGE",
                },
                b(self.start),
                b(self.end)
            )
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Func {
        Sum,
        Count,
        CountStar,
        Min,
        Max,
        SumD,
        CountD,
        AvgD,
        First(bool),
        Last(bool),
        Nth(i64, bool),
        /// `(offset, ignore_nulls, default)`
        Lag(i64, bool, Option<i32>),
        Lead(i64, bool, Option<i32>),
    }

    impl Func {
        fn sql(&self) -> String {
            let n = |ig: bool| if ig { " IGNORE NULLS" } else { "" };
            match self {
                Func::Sum => "SUM(x)".into(),
                Func::Count => "COUNT(x)".into(),
                Func::CountStar => "COUNT(*)".into(),
                Func::Min => "MIN(x)".into(),
                Func::Max => "MAX(x)".into(),
                Func::SumD => "SUM(DISTINCT x)".into(),
                Func::CountD => "COUNT(DISTINCT x)".into(),
                Func::AvgD => "AVG(DISTINCT x)".into(),
                Func::First(ig) => format!("FIRST_VALUE(x){}", n(*ig)),
                Func::Last(ig) => format!("LAST_VALUE(x){}", n(*ig)),
                Func::Nth(k, ig) => format!("NTH_VALUE(x, {k}){}", n(*ig)),
                Func::Lag(k, ig, None) => format!("LAG(x, {k}){}", n(*ig)),
                Func::Lag(k, ig, Some(d)) => format!("LAG(x, {k}, {d}){}", n(*ig)),
                Func::Lead(k, ig, None) => format!("LEAD(x, {k}){}", n(*ig)),
                Func::Lead(k, ig, Some(d)) => format!("LEAD(x, {k}, {d}){}", n(*ig)),
            }
        }
    }

    /// What the oracle computes: the engine's value shape.
    #[derive(Clone, Debug, PartialEq)]
    enum Out {
        Null,
        Int(i32),
        Big(i64),
        Dbl(f64),
    }

    fn to_v(o: &Out) -> V {
        match o {
            Out::Null => None,
            Out::Int(n) => Some(RelationalValue::Integer(*n)),
            Out::Big(n) => Some(RelationalValue::Bigint(*n)),
            Out::Dbl(f) => Some(RelationalValue::Double(*f)),
        }
    }

    fn key_cmp(a: Option<i32>, b: Option<i32>, desc: bool, nulls_first: bool) -> Ordering {
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

    fn evaluate(rows: &[R4], spec: &Spec, f: &Func) -> Vec<(i32, Out)> {
        // The statement's own default: NULLS LAST ascending, NULLS FIRST descending.
        let nulls_first = spec.nulls_first.unwrap_or(spec.desc);
        let mut out = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            // partition, in input order
            let part: Vec<usize> = (0..rows.len())
                .filter(|&j| !spec.partition || rows[j].1 == row.1)
                .collect();
            // order it (stable: ties stay in input order)
            let mut sorted = part.clone();
            sorted.sort_by(|&a, &b| key_cmp(rows[a].2, rows[b].2, spec.desc, nulls_first));
            let n = sorted.len();
            // peer groups
            let mut group = vec![0usize; n];
            for p in 1..n {
                let same = rows[sorted[p]].2 == rows[sorted[p - 1]].2;
                group[p] = group[p - 1] + usize::from(!same);
            }
            let p = sorted.iter().position(|&j| j == i).unwrap();
            let gi = group[p] as i64;
            let pi = p as i64;
            // the frame: every position whose coordinate satisfies both bounds
            let coord = |q: usize| -> i64 {
                match spec.units {
                    Units::Rows => q as i64,
                    Units::Groups => group[q] as i64,
                    Units::Range => group[q] as i64,
                }
            };
            let here = match spec.units {
                Units::Rows => pi,
                _ => gi,
            };
            let start_ok = |q: usize| match spec.start {
                B::UnbPrec => true,
                B::Prec(k) => coord(q) >= here - k,
                B::Cur => coord(q) >= here,
                B::Foll(k) => coord(q) >= here + k,
                B::UnbFoll => false,
            };
            let end_ok = |q: usize| match spec.end {
                B::UnbFoll => true,
                B::Prec(k) => coord(q) <= here - k,
                B::Cur => coord(q) <= here,
                B::Foll(k) => coord(q) <= here + k,
                B::UnbPrec => false,
            };
            let frame: Vec<usize> = (0..n).filter(|&q| start_ok(q) && end_ok(q)).collect();
            let xs: Vec<Option<i32>> = frame.iter().map(|&q| rows[sorted[q]].3).collect();
            let non_null: Vec<i32> = xs.iter().flatten().copied().collect();
            let mut distinct = non_null.clone();
            distinct.sort_unstable();
            distinct.dedup();
            let val = match f {
                Func::Sum => {
                    if non_null.is_empty() {
                        Out::Null
                    } else {
                        Out::Big(non_null.iter().map(|&v| i64::from(v)).sum())
                    }
                }
                Func::Count => Out::Big(non_null.len() as i64),
                Func::CountStar => Out::Big(frame.len() as i64),
                Func::Min => non_null.iter().min().map_or(Out::Null, |v| Out::Int(*v)),
                Func::Max => non_null.iter().max().map_or(Out::Null, |v| Out::Int(*v)),
                Func::SumD => {
                    if distinct.is_empty() {
                        Out::Null
                    } else {
                        Out::Big(distinct.iter().map(|&v| i64::from(v)).sum())
                    }
                }
                Func::CountD => Out::Big(distinct.len() as i64),
                Func::AvgD => {
                    if distinct.is_empty() {
                        Out::Null
                    } else {
                        let s: i64 = distinct.iter().map(|&v| i64::from(v)).sum();
                        Out::Dbl(s as f64 / distinct.len() as f64)
                    }
                }
                Func::First(ig) => {
                    let pick = if *ig {
                        non_null.first().copied()
                    } else {
                        xs.first().copied().flatten()
                    };
                    pick.map_or(Out::Null, Out::Int)
                }
                Func::Last(ig) => {
                    let pick = if *ig {
                        non_null.last().copied()
                    } else {
                        xs.last().copied().flatten()
                    };
                    pick.map_or(Out::Null, Out::Int)
                }
                Func::Nth(k, ig) => {
                    let k = (*k - 1) as usize;
                    let pick = if *ig {
                        non_null.get(k).copied()
                    } else {
                        xs.get(k).copied().flatten()
                    };
                    pick.map_or(Out::Null, Out::Int)
                }
                Func::Lag(k, ig, d) | Func::Lead(k, ig, d) => {
                    let back = matches!(f, Func::Lag(..));
                    let default = d.map_or(Out::Null, Out::Int);
                    let value_at = |q: usize| rows[sorted[q]].3;
                    if *k == 0 {
                        value_at(p).map_or(Out::Null, Out::Int)
                    } else if !*ig {
                        let target = if back { pi - k } else { pi + k };
                        if target < 0 || target >= n as i64 {
                            default
                        } else {
                            value_at(target as usize).map_or(Out::Null, Out::Int)
                        }
                    } else {
                        // the k-th row before / after whose value is not NULL
                        let candidates: Vec<usize> = if back {
                            (0..p).rev().collect()
                        } else {
                            (p + 1..n).collect()
                        };
                        let hits: Vec<i32> =
                            candidates.iter().filter_map(|&q| value_at(q)).collect();
                        hits.get((*k - 1) as usize)
                            .map_or(default, |v| Out::Int(*v))
                    }
                }
            };
            out.push((row.0, val));
        }
        out.sort_by_key(|(id, _)| *id);
        out
    }

    // ---- deterministic data

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    fn dataset(seed: u64, n: usize) -> Vec<R4> {
        let mut g = Lcg(seed);
        (0..n)
            .map(|i| {
                let part = match g.next(4) {
                    0 => None,
                    k => Some(k as i32 - 1),
                };
                let o = if g.next(6) == 0 {
                    None
                } else {
                    Some(g.next(5) as i32)
                };
                let x = if g.next(4) == 0 {
                    None
                } else {
                    Some(g.next(7) as i32)
                };
                (i as i32 + 1, part, o, x)
            })
            .collect()
    }

    fn bounds_for(units: Units) -> Vec<(B, B)> {
        let starts = [
            B::UnbPrec,
            B::Prec(0),
            B::Prec(1),
            B::Prec(2),
            B::Cur,
            B::Foll(1),
            B::Foll(2),
        ];
        let ends = [
            B::Prec(0),
            B::Prec(1),
            B::Prec(2),
            B::Cur,
            B::Foll(1),
            B::Foll(2),
            B::UnbFoll,
        ];
        let mut out = Vec::new();
        for s in starts {
            for e in ends {
                // the standard's shape rules
                let illegal = matches!((s, e), (B::Cur, B::Prec(_)))
                    || matches!((s, e), (B::Foll(_), B::Prec(_)) | (B::Foll(_), B::Cur));
                if illegal {
                    continue;
                }
                if units == Units::Range
                    && (matches!(s, B::Prec(_) | B::Foll(_))
                        || matches!(e, B::Prec(_) | B::Foll(_)))
                {
                    continue;
                }
                out.push((s, e));
            }
        }
        out
    }

    const FUNCS: &[Func] = &[
        Func::Sum,
        Func::Count,
        Func::CountStar,
        Func::Min,
        Func::Max,
        Func::SumD,
        Func::CountD,
        Func::AvgD,
        Func::First(false),
        Func::First(true),
        Func::Last(false),
        Func::Last(true),
        Func::Nth(2, false),
        Func::Nth(2, true),
        Func::Nth(1, true),
    ];

    fn check(db: &Db, rows: &[R4], spec: &Spec, funcs: &[Func], label: &str) {
        let calls: Vec<String> = funcs
            .iter()
            .map(|f| format!("{} OVER ({})", f.sql(), spec.sql()))
            .collect();
        let sql = format!("SELECT id, {} FROM w ORDER BY id", calls.join(", "));
        let got = db
            .try_run(&sql)
            .unwrap_or_else(|e| panic!("{label}: {sql}: {e}"))
            .rows;
        assert_eq!(got.len(), rows.len(), "{label}: {sql}");
        for (fi, f) in funcs.iter().enumerate() {
            let want = evaluate(rows, spec, f);
            for (ri, (id, w)) in want.iter().enumerate() {
                let g = &got[ri][fi + 1];
                assert_eq!(
                    g,
                    &to_v(w),
                    "{label}: row id {id}, {:?} with {}\n  data (id,g,o,x): {rows:?}",
                    f,
                    spec.sql()
                );
            }
        }
    }

    fn load(tag: &str, rows: &[R4]) -> Db {
        let db = win_db(tag);
        seed_w(&db, rows);
        db
    }

    #[test]
    fn groups_rows_and_range_frames_match_the_oracle() {
        let mut checked = 0usize;
        for (di, (seed, n)) in [(1u64, 0usize), (2, 1), (3, 2), (4, 7), (5, 15), (6, 30)]
            .into_iter()
            .enumerate()
        {
            let rows = dataset(seed, n);
            let db = load(&format!("p1_or_frames_{di}"), &rows);
            for units in [Units::Groups, Units::Rows, Units::Range] {
                for (start, end) in bounds_for(units) {
                    for partition in [false, true] {
                        for (desc, nulls_first) in [
                            (false, None),
                            (true, None),
                            (false, Some(true)),
                            (true, Some(false)),
                        ] {
                            // fewer combinations of the order variants for ROWS/RANGE, which
                            // `window_reference_model` already covers exhaustively
                            if units != Units::Groups
                                && (nulls_first.is_some() || desc && di % 2 == 0)
                            {
                                continue;
                            }
                            let spec = Spec {
                                partition,
                                desc,
                                nulls_first,
                                units,
                                start,
                                end,
                            };
                            check(&db, &rows, &spec, FUNCS, &format!("dataset {di}"));
                            checked += 1;
                        }
                    }
                }
            }
            db.cleanup();
        }
        assert!(
            checked > 1500,
            "only {checked} frame combinations were checked"
        );
    }

    #[test]
    fn lag_and_lead_ignore_nulls_match_the_oracle() {
        let funcs = [
            Func::Lag(0, true, None),
            Func::Lag(1, true, None),
            Func::Lag(1, false, None),
            Func::Lag(2, true, None),
            Func::Lag(2, true, Some(-1)),
            Func::Lag(3, true, Some(-1)),
            Func::Lead(0, true, None),
            Func::Lead(1, true, None),
            Func::Lead(1, false, None),
            Func::Lead(2, true, None),
            Func::Lead(2, true, Some(-1)),
            Func::Lead(3, true, Some(-1)),
        ];
        let mut checked = 0usize;
        for (di, (seed, n)) in [
            (11u64, 0usize),
            (12, 1),
            (13, 3),
            (14, 9),
            (15, 20),
            (16, 40),
        ]
        .into_iter()
        .enumerate()
        {
            let rows = dataset(seed, n);
            let db = load(&format!("p1_or_lag_{di}"), &rows);
            for partition in [false, true] {
                for (desc, nulls_first) in [(false, None), (true, None), (false, Some(true))] {
                    // LAG / LEAD take no frame; the oracle's frame is not used by them
                    let calls: Vec<String> = funcs
                        .iter()
                        .map(|f| {
                            format!(
                                "{} OVER ({}ORDER BY o{}{})",
                                f.sql(),
                                if partition { "PARTITION BY g " } else { "" },
                                if desc { " DESC" } else { "" },
                                match nulls_first {
                                    Some(true) => " NULLS FIRST",
                                    _ => "",
                                }
                            )
                        })
                        .collect();
                    let sql = format!("SELECT id, {} FROM w ORDER BY id", calls.join(", "));
                    let got = db
                        .try_run(&sql)
                        .unwrap_or_else(|e| panic!("{sql}: {e}"))
                        .rows;
                    let spec = Spec {
                        partition,
                        desc,
                        nulls_first,
                        units: Units::Rows,
                        start: B::UnbPrec,
                        end: B::UnbFoll,
                    };
                    for (fi, f) in funcs.iter().enumerate() {
                        for (ri, (id, w)) in evaluate(&rows, &spec, f).iter().enumerate() {
                            assert_eq!(
                                &got[ri][fi + 1],
                                &to_v(w),
                                "dataset {di}: row id {id}, {f:?}, partition={partition} desc={desc} nulls_first={nulls_first:?}\n  data: {rows:?}"
                            );
                            checked += 1;
                        }
                    }
                }
            }
            db.cleanup();
        }
        assert!(checked > 1000, "only {checked} values were checked");
    }

    #[test]
    fn the_oracle_agrees_with_hand_computed_values() {
        // Guards the oracle itself: the GROUPS example of `groups::offsets_count_...`
        // (o = 1,1,2,3,3,3; x = id; 1 PRECEDING .. CURRENT ROW gives 3,3,6,18,18,18).
        let rows: Vec<R4> = [1, 1, 2, 3, 3, 3]
            .iter()
            .enumerate()
            .map(|(i, o)| (i as i32 + 1, None, Some(*o), Some(i as i32 + 1)))
            .collect();
        let spec = Spec {
            partition: false,
            desc: false,
            nulls_first: None,
            units: Units::Groups,
            start: B::Prec(1),
            end: B::Cur,
        };
        let got: Vec<Out> = evaluate(&rows, &spec, &Func::Sum)
            .into_iter()
            .map(|(_, o)| o)
            .collect();
        assert_eq!(
            got,
            vec![
                Out::Big(3),
                Out::Big(3),
                Out::Big(6),
                Out::Big(18),
                Out::Big(18),
                Out::Big(18)
            ]
        );
        let spec = Spec {
            units: Units::Rows,
            ..spec
        };
        let got: Vec<Out> = evaluate(&rows, &spec, &Func::Sum)
            .into_iter()
            .map(|(_, o)| o)
            .collect();
        assert_eq!(
            got,
            vec![
                Out::Big(1),
                Out::Big(3),
                Out::Big(5),
                Out::Big(7),
                Out::Big(9),
                Out::Big(11)
            ]
        );
    }
}
