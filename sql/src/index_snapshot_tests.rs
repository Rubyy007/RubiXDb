//! Increment 17 / F-2: snapshot correctness of secondary-index reads
//! across index creation, rebuild, drop/recreate.
//!
//! Safety property under test: for every valid snapshot, a query that
//! the planner answers through a secondary index returns **exactly** the
//! rows an independent reference model (a plain in-memory copy of the
//! table as it stood at that snapshot) says the snapshot can see -- no
//! missing rows, no extra rows, no duplicates. The reference model is
//! captured from the test's own bookkeeping at the moment each
//! transaction begins; it shares no code with the planner, the index, or
//! the executor.
//!
//! All interleavings are deterministic (no sleeps, no timing): the
//! "BEGIN during CREATE INDEX" cases build the index through its real
//! lifecycle pieces -- `CatalogService::create_index` (state `Building`)
//! followed by `IndexBuilder::recover_incomplete_builds` (the real
//! backfill + `Building -> Ready` promotion) -- so a transaction can be
//! begun at an exact point of the build timeline.

use rubixdb::catalog::schema::IndexKind;

use crate::index_read_differential_tests::{rows_of, Diff, Mrow};

fn dt_table_id(d: &Diff) -> u32 {
    d.catalog
        .get_table_by_name(d.f.ctx.default_schema_id, "dt")
        .unwrap()
        .unwrap()
        .table_id
}

/// A `Diff` whose `dt` table has the three standard indexes dropped, so
/// each test controls index creation precisely.
fn bare() -> Diff {
    let d = Diff::new("idxsnap");
    d.write("DROP INDEX ia ON dt");
    d.write("DROP INDEX ib ON dt");
    d.write("DROP INDEX iab ON dt");
    d
}

fn ins(d: &Diff, id: i32, a: i32, b: &str) {
    d.write(&format!(
        "INSERT INTO dt (id, a, b, c) VALUES ({id}, {a}, '{b}', 0)"
    ));
}

/// Queries answered through `ib` (equality) and `ia` (range) when those
/// indexes exist; compared with the model through the snapshot `txn`.
fn assert_snapshot(d: &Diff, txn: &rubixdb::relational::Transaction, model: &[Mrow], ctx: &str) {
    for b in ["s0", "s1", "s2", "s3"] {
        let got = rows_of(&d.select_in(&format!("SELECT * FROM dt WHERE b = '{b}'"), txn));
        let mut expect: Vec<Mrow> = model
            .iter()
            .filter(|r| r.2.as_deref() == Some(b))
            .cloned()
            .collect();
        expect.sort();
        assert_eq!(got, expect, "{ctx}: b = {b}");
    }
    for (lo, hi) in [(0, 3), (2, 6), (0, 100)] {
        let got = rows_of(&d.select_in(
            &format!("SELECT * FROM dt WHERE a >= {lo} AND a < {hi}"),
            txn,
        ));
        let mut expect: Vec<Mrow> = model
            .iter()
            .filter(|r| r.1.is_some_and(|a| a >= lo && a < hi))
            .cloned()
            .collect();
        expect.sort();
        assert_eq!(got, expect, "{ctx}: a in [{lo},{hi})");
    }
}

fn model_now(d: &Diff) -> Vec<Mrow> {
    let r = d.select("SELECT * FROM dt WHERE id >= 0");
    rows_of(&r)
}

/// CREATE INDEX *after* the transaction began (the exact F-2 scenario).
#[test]
fn create_index_with_old_snapshot() {
    let d = bare();
    ins(&d, 1, 1, "s1");
    ins(&d, 2, 2, "s2");
    ins(&d, 3, 3, "s2");
    let t1 = d.txm.begin().unwrap();
    let m1 = model_now(&d);
    // later changes T1 must not see
    ins(&d, 4, 4, "s2");
    d.write("DELETE FROM dt WHERE id = 1");
    d.write("CREATE INDEX ib ON dt (b)");
    d.write("CREATE INDEX ia ON dt (a)");
    assert_snapshot(&d, &t1, &m1, "T1 (before CREATE INDEX)");
    let t3 = d.txm.begin().unwrap();
    let m3 = model_now(&d);
    assert_snapshot(&d, &t3, &m3, "T3 (after CREATE INDEX)");
    d.f.cleanup();
}

/// DROP INDEX then CREATE INDEX with the same name, old snapshot began
/// while the first incarnation was Ready.
#[test]
fn drop_then_create_index_with_old_snapshot() {
    let d = bare();
    d.write("CREATE INDEX ib ON dt (b)");
    d.write("CREATE INDEX ia ON dt (a)");
    ins(&d, 1, 1, "s1");
    ins(&d, 2, 2, "s2");
    let t1 = d.txm.begin().unwrap();
    let m1 = model_now(&d);
    d.write("UPDATE dt SET b = 's3' WHERE id = 2"); // indexed value changes after T1
    d.write("DROP INDEX ib ON dt");
    d.write("DROP INDEX ia ON dt");
    d.write("CREATE INDEX ib ON dt (b)");
    d.write("CREATE INDEX ia ON dt (a)");
    assert_snapshot(&d, &t1, &m1, "T1 (before DROP/CREATE)");
    let t3 = d.txm.begin().unwrap();
    assert_snapshot(&d, &t3, &model_now(&d), "T3 (after DROP/CREATE)");
    d.f.cleanup();
}

/// Indexed value UPDATEd between the snapshot and the rebuild: the
/// rebuilt index only holds the NEW value; the old snapshot must neither
/// miss the row under its old value nor receive it under the new one.
#[test]
fn update_between_snapshot_and_rebuild_neither_missing_nor_extra() {
    let d = bare();
    ins(&d, 1, 1, "s1");
    let t1 = d.txm.begin().unwrap();
    let m1 = model_now(&d);
    d.write("UPDATE dt SET b = 's2', a = 5 WHERE id = 1");
    d.write("CREATE INDEX ib ON dt (b)");
    d.write("CREATE INDEX ia ON dt (a)");
    assert_snapshot(&d, &t1, &m1, "T1");
    let got_new = rows_of(&d.select_in("SELECT * FROM dt WHERE b = 's2'", &t1));
    assert!(
        got_new.is_empty(),
        "T1 must not see the post-snapshot value"
    );
    d.f.cleanup();
}

/// BEGIN *during* CREATE INDEX: the index is `Building` when the
/// transaction begins; writes land before and after; the build then
/// completes. T2's snapshot predates `Ready`.
#[test]
fn begin_during_create_index_build() {
    let d = bare();
    let tid = dt_table_id(&d);
    ins(&d, 1, 1, "s1");
    ins(&d, 2, 2, "s2");
    // T0: the catalog `Building` row exists; writers now maintain it.
    d.catalog
        .create_index(tid, "ib", IndexKind::NonUnique, &[2])
        .unwrap();
    ins(&d, 3, 3, "s2"); // maintained while Building
    let t2 = d.txm.begin().unwrap(); // snapshot during the build
    let m2 = model_now(&d);
    ins(&d, 4, 4, "s2");
    d.write("DELETE FROM dt WHERE id = 2");
    // backfill + Building -> Ready (the real lifecycle path)
    let recovered = d.builder.recover_incomplete_builds().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_snapshot(&d, &t2, &m2, "T2 (began during build)");
    let t3 = d.txm.begin().unwrap();
    assert_snapshot(&d, &t3, &model_now(&d), "T3 (after Ready)");
    d.f.cleanup();
}

/// INSERT / UPDATE / DELETE all interleaved across the build with three
/// snapshots: T1 before the build, T2 during, T3 after.
#[test]
fn insert_update_delete_during_build_three_snapshots() {
    let d = bare();
    let tid = dt_table_id(&d);
    ins(&d, 1, 1, "s1");
    ins(&d, 2, 2, "s2");
    ins(&d, 3, 3, "s3");
    let t1 = d.txm.begin().unwrap();
    let m1 = model_now(&d);
    d.write("UPDATE dt SET b = 's0' WHERE id = 1");
    d.catalog
        .create_index(tid, "ib", IndexKind::NonUnique, &[2])
        .unwrap();
    ins(&d, 4, 4, "s2");
    let t2 = d.txm.begin().unwrap();
    let m2 = model_now(&d);
    d.write("DELETE FROM dt WHERE id = 3");
    d.write("UPDATE dt SET b = 's1' WHERE id = 4");
    d.builder.recover_incomplete_builds().unwrap();
    ins(&d, 5, 5, "s2");
    let t3 = d.txm.begin().unwrap();
    let m3 = model_now(&d);
    assert_snapshot(&d, &t1, &m1, "T1 (before build)");
    assert_snapshot(&d, &t2, &m2, "T2 (during build)");
    assert_snapshot(&d, &t3, &m3, "T3 (after build)");
    d.f.cleanup();
}

// ---------------------------------------------------------------------
// Which access path ran, and the shapes that share the access operator.
// ---------------------------------------------------------------------

/// The fallback counter proves the old-snapshot read really ran as a table
/// scan (not that the index happened to give the right answer), and that a
/// snapshot taken after the rebuild still uses the index.
#[test]
fn fallback_is_taken_exactly_when_the_snapshot_predates_the_index() {
    let d = bare();
    ins(&d, 1, 1, "s1");
    ins(&d, 2, 2, "s2");
    let t1 = d.txm.begin().unwrap();
    d.write("CREATE INDEX ib ON dt (b)");
    let (r, m) = d.select_in_metrics("SELECT * FROM dt WHERE b = 's2'", &t1);
    assert_eq!(rows_of(&r).len(), 1);
    assert_eq!(m.index_snapshot_fallbacks, 1, "old snapshot must fall back");
    assert_eq!(m.seq_scans, 1);
    let t3 = d.txm.begin().unwrap();
    let (r, m) = d.select_in_metrics("SELECT * FROM dt WHERE b = 's2'", &t3);
    assert_eq!(rows_of(&r).len(), 1);
    assert_eq!(
        m.index_snapshot_fallbacks, 0,
        "new snapshot must use the index"
    );
    assert_eq!(m.seq_scans, 0);
    assert_eq!(m.index_scans, 1);
    d.f.cleanup();
}

/// `ORDER BY` satisfied by the index (the Sort node is eliminated): the
/// fallback must re-establish the order exactly -- ascending, NULLS FIRST,
/// ties in primary-key order.
#[test]
fn ordered_index_scan_fallback_preserves_order() {
    let d = bare();
    for (id, a, b) in [
        (5, 1, "s2"),
        (1, 2, "s0"),
        (3, 3, "s2"),
        (2, 4, "s1"),
        (4, 5, "s0"),
    ] {
        ins(&d, id, a, b);
    }
    d.write("INSERT INTO dt (id, a, b, c) VALUES (6, 6, NULL, 0)");
    let t1 = d.txm.begin().unwrap();
    d.write("CREATE INDEX ib ON dt (b)");
    // ORDER BY-only index scan (sort elided) and predicate + order. The
    // plan must really have dropped the Sort node, or this test would be
    // vacuous.
    for sql in [
        "SELECT id, b FROM dt ORDER BY b NULLS FIRST",
        "SELECT id FROM dt WHERE b = 's2' ORDER BY b NULLS FIRST",
    ] {
        let dbg = format!("{:?}", d.plan(sql));
        assert!(!dbg.contains("Sort {"), "Sort must be eliminated: {sql}");
        assert!(dbg.contains("IndexScan"), "must plan an IndexScan: {sql}");
    }
    let got = d.select_in("SELECT id, b FROM dt ORDER BY b NULLS FIRST", &t1);
    let ids: Vec<i32> = got
        .rows
        .iter()
        .map(|r| match &r[0] {
            Some(rubixdb::relational::RelationalValue::Integer(i)) => *i,
            o => panic!("{o:?}"),
        })
        .collect();
    // NULL first, then s0 (ids 1,4), s1 (2), s2 (3,5) -- ties by id.
    assert_eq!(ids, vec![6, 1, 4, 2, 3, 5]);
    let got = d.select_in(
        "SELECT id FROM dt WHERE b = 's2' ORDER BY b NULLS FIRST",
        &t1,
    );
    let ids: Vec<i32> = got
        .rows
        .iter()
        .map(|r| match &r[0] {
            Some(rubixdb::relational::RelationalValue::Integer(i)) => *i,
            o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(ids, vec![3, 5]);
    // The same two queries from a fresh snapshot use the index and agree.
    let t3 = d.txm.begin().unwrap();
    let (got3, m) = d.select_in_metrics("SELECT id, b FROM dt ORDER BY b NULLS FIRST", &t3);
    assert_eq!(m.index_snapshot_fallbacks, 0);
    assert_eq!(got3.rows.len(), 6);
    d.f.cleanup();
}

/// JOIN, aggregation, and DML all share the access operator: each must be
/// correct under an old snapshot.
#[test]
fn join_aggregate_and_dml_under_an_old_snapshot() {
    let d = bare();
    ins(&d, 1, 1, "s1");
    ins(&d, 2, 2, "s2");
    ins(&d, 3, 2, "s2");
    let mut t1 = d.txm.begin().unwrap();
    ins(&d, 4, 2, "s2"); // after T1
    d.write("CREATE INDEX ib ON dt (b)");
    d.write("CREATE INDEX ia ON dt (a)");
    // JOIN through an index predicate
    let j = d.select_in(
        "SELECT dt.id, dj.label FROM dt JOIN dj ON dt.a = dj.a WHERE dt.b = 's2'",
        &t1,
    );
    assert_eq!(j.rows.len(), 2, "T1 sees ids 2 and 3 only");
    // aggregation
    let c = d.select_in("SELECT COUNT(*) FROM dt WHERE a = 2", &t1);
    assert_eq!(
        c.rows[0][0],
        Some(rubixdb::relational::RelationalValue::Bigint(2))
    );
    // DELETE through the index predicate, in the old snapshot: exactly the
    // two rows it can see (row 4 is invisible to it).
    let n = d.write_in("DELETE FROM dt WHERE b = 's2'", &mut t1);
    assert_eq!(n, 2);
    t1.commit().unwrap();
    let left = rows_of(&d.select("SELECT * FROM dt WHERE id >= 0"));
    let ids: Vec<i32> = left.iter().map(|r| r.0).collect();
    assert_eq!(ids, vec![1, 4], "rows 2,3 deleted; 4 (post-snapshot) kept");
    d.f.cleanup();
}
