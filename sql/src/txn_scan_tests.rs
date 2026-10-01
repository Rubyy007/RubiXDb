//! Increment 18: transaction scan semantics (read-your-own-writes on scans).
//!
//! Contract under test (`PHASE_RUBIXDB_INCREMENT18_TRANSACTION_SCAN_
//! SEMANTICS.md`): within a transaction, EVERY read -- PK lookup, PK range,
//! secondary-index equality/range, sequential scan, aggregation, join, and
//! the target-finding pass of UPDATE/DELETE -- observes the transaction's
//! base snapshot PLUS its own uncommitted insertions, updates and
//! deletions, exactly once per row. The independent reference model is a
//! plain `BTreeMap` mutated by the test itself; it shares no code with the
//! planner, executor, index, or write-set.

use std::collections::BTreeMap;

use crate::exec::cost::AccessPathMode;
use crate::index_read_differential_tests::{rows_of, Diff, Mrow};

type Model = BTreeMap<i32, Mrow>;

fn m(id: i32, a: Option<i32>, b: Option<&str>, c: i32) -> Mrow {
    (id, a, b.map(|s| s.to_string()), c)
}

fn ins_t(d: &Diff, t: &mut rubixdb::relational::Transaction, model: &mut Model, row: Mrow) {
    let lit_a = row.1.map_or("NULL".to_string(), |v| v.to_string());
    let lit_b = row
        .2
        .as_ref()
        .map_or("NULL".to_string(), |v| format!("'{v}'"));
    d.write_in(
        &format!(
            "INSERT INTO dt (id, a, b, c) VALUES ({}, {lit_a}, {lit_b}, {})",
            row.0, row.3
        ),
        t,
    );
    model.insert(row.0, row);
}

/// Every access-path shape, under every access-path mode, against the model.
fn check(d: &Diff, t: &rubixdb::relational::Transaction, model: &Model, ctx: &str) {
    type Q = (String, Box<dyn Fn(&Mrow) -> bool>);
    let queries: Vec<Q> = vec![
        ("SELECT * FROM dt WHERE id >= 0".into(), Box::new(|_| true)), // PK range
        (
            "SELECT * FROM dt WHERE c >= 0".into(),
            Box::new(|r| r.3 >= 0),
        ), // seq scan
        (
            "SELECT * FROM dt WHERE c = 1".into(),
            Box::new(|r| r.3 == 1),
        ),
        (
            "SELECT * FROM dt WHERE a = 1".into(),
            Box::new(|r| r.1 == Some(1)),
        ), // index eq
        (
            "SELECT * FROM dt WHERE a = 2".into(),
            Box::new(|r| r.1 == Some(2)),
        ),
        (
            "SELECT * FROM dt WHERE a >= 1 AND a < 4".into(),
            Box::new(|r| r.1.is_some_and(|a| (1..4).contains(&a))),
        ), // index range
        (
            "SELECT * FROM dt WHERE a <= 2".into(),
            Box::new(|r| r.1.is_some_and(|a| a <= 2)),
        ),
        (
            "SELECT * FROM dt WHERE b = 's1'".into(),
            Box::new(|r| r.2.as_deref() == Some("s1")),
        ),
        (
            "SELECT * FROM dt WHERE a = 1 AND b = 's1'".into(),
            Box::new(|r| r.1 == Some(1) && r.2.as_deref() == Some("s1")),
        ), // composite
        (
            "SELECT * FROM dt WHERE id >= 5 AND id < 20 AND a = 1".into(),
            Box::new(|r| r.0 >= 5 && r.0 < 20 && r.1 == Some(1)),
        ), // PK range + index
        ("SELECT * FROM dt WHERE a = 99".into(), Box::new(|_| false)),
    ];
    for mode in [
        AccessPathMode::Auto,
        AccessPathMode::ForceIndex,
        AccessPathMode::ForceSeq,
    ] {
        d.mode.set(mode);
        for (sql, pred) in &queries {
            let want: Vec<Mrow> = model.values().filter(|r| pred(r)).cloned().collect();
            let got = rows_of(&d.select_in(sql, t));
            assert_eq!(got, want, "{ctx} mode={mode:?}: {sql}");
        }
        let n = d.select_in("SELECT COUNT(*) FROM dt WHERE a = 1", t);
        let want = model.values().filter(|r| r.1 == Some(1)).count() as i64;
        assert_eq!(
            n.rows[0][0],
            Some(rubixdb::relational::RelationalValue::Bigint(want)),
            "{ctx} mode={mode:?}: COUNT a=1"
        );
    }
    d.mode.set(AccessPathMode::Auto);
    // PK lookups agree with scans (the path that always overlaid).
    for id in 0..40 {
        let got = rows_of(&d.select_in(&format!("SELECT * FROM dt WHERE id = {id}"), t));
        let want: Vec<Mrow> = model.get(&id).cloned().into_iter().collect();
        assert_eq!(got, want, "{ctx}: PK lookup id={id}");
    }
}

fn base() -> (Diff, Model) {
    let d = Diff::with_memtable("txnscan", 4 * 1024 * 1024);
    let mut model = Model::new();
    for (id, a, b, c) in [
        (1, 1, "s1", 1),
        (2, 2, "s2", 1),
        (3, 1, "s1", 0),
        (4, 3, "s3", 1),
        (10, 2, "s1", 0),
        (30, 1, "s2", 1),
    ] {
        d.write(&format!(
            "INSERT INTO dt (id, a, b, c) VALUES ({id}, {a}, '{b}', {c})"
        ));
        model.insert(id, m(id, Some(a), Some(b), c));
    }
    (d, model)
}

#[test]
fn inserted_row_is_visible_to_every_scan() {
    let (d, mut model) = base();
    let mut t = d.txm.begin().unwrap();
    ins_t(&d, &mut t, &mut model, m(7, Some(1), Some("s1"), 1));
    ins_t(&d, &mut t, &mut model, m(8, None, None, 1));
    check(&d, &t, &model, "after insert");
    d.f.cleanup();
}

#[test]
fn updated_row_is_visible_to_every_scan_under_its_new_value_only() {
    let (d, mut model) = base();
    let mut t = d.txm.begin().unwrap();
    // update indexed + unindexed columns of an existing (base) row
    d.write_in("UPDATE dt SET a = 2, b = 's9', c = 0 WHERE id = 1", &mut t);
    *model.get_mut(&1).unwrap() = m(1, Some(2), Some("s9"), 0);
    check(&d, &t, &model, "after update of base row");
    d.f.cleanup();
}

#[test]
fn deleted_row_disappears_from_every_scan() {
    let (d, mut model) = base();
    let mut t = d.txm.begin().unwrap();
    d.write_in("DELETE FROM dt WHERE id = 3", &mut t);
    model.remove(&3);
    check(&d, &t, &model, "after delete of base row");
    d.f.cleanup();
}

#[test]
fn insert_update_delete_sequences_collapse_correctly() {
    let (d, mut model) = base();
    let mut t = d.txm.begin().unwrap();
    // insert -> update -> delete: never visible
    ins_t(&d, &mut t, &mut model, m(20, Some(1), Some("s1"), 1));
    d.write_in("UPDATE dt SET a = 2 WHERE id = 20", &mut t);
    model.get_mut(&20).unwrap().1 = Some(2);
    check(&d, &t, &model, "insert+update");
    d.write_in("DELETE FROM dt WHERE id = 20", &mut t);
    model.remove(&20);
    check(&d, &t, &model, "insert+update+delete");
    // delete -> re-insert of a base key with new values
    d.write_in("DELETE FROM dt WHERE id = 2", &mut t);
    model.remove(&2);
    ins_t(&d, &mut t, &mut model, m(2, Some(3), Some("s3"), 0));
    check(&d, &t, &model, "delete+reinsert base key");
    d.f.cleanup();
}

/// The multi-statement hazard that makes this a correctness issue and not a
/// cosmetic one: UPDATE/DELETE find their target rows through the scan
/// operator, so rows the same transaction inserted were silently skipped.
#[test]
fn update_and_delete_find_rows_inserted_by_the_same_transaction() {
    let (d, mut model) = base();
    let mut t = d.txm.begin().unwrap();
    ins_t(&d, &mut t, &mut model, m(7, Some(1), Some("s1"), 1));
    ins_t(&d, &mut t, &mut model, m(8, Some(1), Some("s2"), 0));
    // UPDATE through the secondary index: base rows 1,3,30 and own rows 7,8
    let n = d.write_in("UPDATE dt SET c = 5 WHERE a = 1", &mut t);
    let want: Vec<i32> = model
        .values()
        .filter(|r| r.1 == Some(1))
        .map(|r| r.0)
        .collect();
    for id in &want {
        model.get_mut(id).unwrap().3 = 5;
    }
    assert_eq!(n as usize, want.len(), "UPDATE a=1 must hit own inserts");
    check(&d, &t, &model, "after update over own inserts");
    // DELETE through a seq-scan predicate over base + own rows
    let n = d.write_in("DELETE FROM dt WHERE c = 5", &mut t);
    let gone = model.values().filter(|r| r.3 == 5).count();
    model.retain(|_, r| r.3 != 5);
    assert_eq!(n as usize, gone, "DELETE c=5 must hit own rows");
    check(&d, &t, &model, "after delete over own rows");
    t.commit().unwrap();
    // The committed state equals the model exactly.
    let fin = d.txm.begin().unwrap();
    check(&d, &fin, &model, "after commit");
    d.f.cleanup();
}

#[test]
fn autocommit_and_other_snapshots_never_see_uncommitted_writes() {
    let (d, model) = base();
    let mut t = d.txm.begin().unwrap();
    d.write_in(
        "INSERT INTO dt (id, a, b, c) VALUES (50, 1, 's1', 1)",
        &mut t,
    );
    d.write_in("DELETE FROM dt WHERE id = 1", &mut t);
    // a different transaction and the autocommit path see the base only
    let other = d.txm.begin().unwrap();
    check(&d, &other, &model, "other transaction");
    assert_eq!(
        rows_of(&d.select("SELECT * FROM dt WHERE id >= 0")).len(),
        model.len()
    );
    d.f.cleanup();
}

// ---------------------------------------------------------------------
// Randomized property: base snapshot + local writes, under every path
// ---------------------------------------------------------------------

use crate::index_read_differential_tests::Rng;

fn run_txn_property(seed: u64, steps: usize) -> u64 {
    let mut rng = Rng(seed);
    let d = Diff::new(&format!("txnprop_{seed}"));
    let mut model = Model::new();
    for id in 0..40 {
        let a = rng.below(4) as i32;
        let b = format!("s{}", rng.below(3));
        let c = rng.below(2) as i32;
        d.write(&format!(
            "INSERT INTO dt (id, a, b, c) VALUES ({id}, {a}, '{b}', {c})"
        ));
        model.insert(id, m(id, Some(a), Some(&b), c));
    }
    let mut t = d.txm.begin().unwrap(); // base snapshot for the whole run
    let mut shadow = model.clone(); // what the world looks like after outside writes
    for step in 0..steps {
        let ctx = format!("seed={seed} step={step}");
        match rng.below(100) {
            0..=29 => {
                // local insert of a fresh id
                let id = 40 + rng.below(40) as i32;
                if !model.contains_key(&id) {
                    let row = m(
                        id,
                        Some(rng.below(4) as i32),
                        Some(&format!("s{}", rng.below(3))),
                        rng.below(2) as i32,
                    );
                    ins_t(&d, &mut t, &mut model, row);
                }
            }
            30..=44 => {
                // local update by PK (PK-lookup path) -- changes indexed cols
                if let Some((&id, _)) = model
                    .iter()
                    .nth(rng.below(model.len().max(1) as u64) as usize)
                {
                    let (a, b) = (rng.below(4) as i32, format!("s{}", rng.below(3)));
                    d.write_in(
                        &format!("UPDATE dt SET a = {a}, b = '{b}' WHERE id = {id}"),
                        &mut t,
                    );
                    let r = model.get_mut(&id).unwrap();
                    r.1 = Some(a);
                    r.2 = Some(b);
                }
            }
            45..=54 => {
                // local delete by PK
                if let Some((&id, _)) = model
                    .iter()
                    .nth(rng.below(model.len().max(1) as u64) as usize)
                {
                    d.write_in(&format!("DELETE FROM dt WHERE id = {id}"), &mut t);
                    model.remove(&id);
                }
            }
            55..=62 => {
                // DML THROUGH a scan/index predicate: exact target count
                let v = rng.below(4) as i32;
                let nb = format!("s{}", rng.below(3));
                let n = d.write_in(&format!("UPDATE dt SET b = '{nb}' WHERE a = {v}"), &mut t);
                let mut cnt = 0;
                for r in model.values_mut().filter(|r| r.1 == Some(v)) {
                    r.2 = Some(nb.clone());
                    cnt += 1;
                }
                assert_eq!(n, cnt, "{ctx}: UPDATE a={v} target count");
            }
            63..=68 => {
                let c = rng.below(2) as i32;
                let n = d.write_in(&format!("DELETE FROM dt WHERE c = {c} AND id >= 0"), &mut t);
                let before = model.len();
                model.retain(|_, r| r.3 != c);
                assert_eq!(n as usize, before - model.len(), "{ctx}: DELETE c={c}");
            }
            69..=84 => {
                // an outside writer commits: invisible to the open snapshot
                let id = rng.below(80) as i32;
                if let std::collections::btree_map::Entry::Vacant(e) = shadow.entry(id) {
                    d.write(&format!(
                        "INSERT INTO dt (id, a, b, c) VALUES ({id}, 1, 's0', 0)"
                    ));
                    e.insert(m(id, Some(1), Some("s0"), 0));
                } else {
                    d.write(&format!("DELETE FROM dt WHERE id = {id}"));
                    shadow.remove(&id);
                }
            }
            85..=89 if step > 5 => {
                // index rebuilt underneath the open snapshot (F-2 + overlay)
                d.write("DROP INDEX ib ON dt");
                d.write("CREATE INDEX ib ON dt (b)");
            }
            _ => {}
        }
        if step % 3 == 0 {
            check(&d, &t, &model, &ctx);
        }
    }
    check(&d, &t, &model, &format!("seed={seed} final"));
    // The outside world is unaffected by the open, uncommitted transaction.
    let other = d.txm.begin().unwrap();
    check(&d, &other, &shadow, &format!("seed={seed} outside view"));
    let cycles = d.f.engine.compaction_metrics().cycles_completed;
    d.f.cleanup();
    cycles
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig { cases: 16, ..proptest::prelude::ProptestConfig::default() })]
    #[test]
    fn transaction_scans_equal_base_plus_local_writes(seed in proptest::prelude::any::<u64>()) {
        let _ = run_txn_property(seed, 90);
    }
}

#[test]
fn transaction_scan_property_fixed_seeds_long() {
    let mut cycles = 0;
    for seed in 1..=6u64 {
        cycles += run_txn_property(seed, 250);
    }
    println!("automatic Compaction cycles during the transaction property runs: {cycles}");
    assert!(cycles > 0, "the property runs must overlap real Compaction");
}
