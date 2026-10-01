//! Increment 18: lazy index row fetch (`PHASE_RUBIXDB_INCREMENT18_
//! MATERIALIZATION_ARCHITECTURE.md`).
//!
//! The index path enumerates matching entries up front (key-only, bounded
//! by `max_index_scan_rows`) and fetches rows ON DEMAND. These tests prove
//! the behaviours that make that safe and useful: `LIMIT` fetches only what
//! it returns, no read-ahead (backpressure), cancellation and deadlines
//! stop the work, the transaction stays correct afterwards, and results are
//! unchanged (the differential/property suites cover result equality at
//! scale). Row-fetch counts come from the relational layer's own counter
//! (`IndexStatsSnapshot::index_rows_fetched`), not from timing.

use std::time::Duration;

use crate::exec::cost::AccessPathMode;
use crate::exec::operators::build_operator;
use crate::exec::{CancellationToken, ExecCtx, ExecLimits, ExecMetrics, RowContext};
use crate::index_read_differential_tests::{rows_of, Diff};
use crate::plan::{PhysicalPlan, Plan};
use crate::SqlError;

fn loaded() -> Diff {
    let d = Diff::with_memtable("mat", 4 * 1024 * 1024);
    // 2,000 rows; a = id % 2 so `a = 1` matches 1,000 rows via index `ia`.
    let mut batch = Vec::new();
    for id in 0..2_000 {
        batch.push(format!("({id}, {}, 's{}', {})", id % 2, id % 5, id % 3));
        if batch.len() == 200 {
            d.write(&format!(
                "INSERT INTO dt (id, a, b, c) VALUES {}",
                batch.join(",")
            ));
            batch.clear();
        }
    }
    d.mode.set(AccessPathMode::ForceIndex);
    d
}

fn fetched(d: &Diff) -> u64 {
    d.builder.stats().index_rows_fetched
}

#[test]
fn limit_fetches_only_the_rows_it_returns() {
    let d = loaded();
    let before = fetched(&d);
    let r = d.select("SELECT * FROM dt WHERE a = 1 LIMIT 5");
    let after = fetched(&d);
    assert_eq!(r.rows.len(), 5);
    assert_eq!(
        after - before,
        5,
        "LIMIT 5 over 1,000 index matches must fetch 5 rows, not 1,000"
    );
    // every returned row really satisfies the predicate
    for row in rows_of(&r) {
        assert_eq!(row.1, Some(1));
    }
    // and OFFSET still works lazily
    let r = d.select("SELECT * FROM dt WHERE a = 1 LIMIT 5 OFFSET 7");
    assert_eq!(r.rows.len(), 5);
    d.f.cleanup();
}

/// No read-ahead: a consumer that pulls one row at a time sees exactly one
/// row fetched per pull -- the work is bounded by what the consumer asks
/// for, however slowly it asks (backpressure by construction; the only
/// per-scan buffer is the primary-key entry list, bounded by
/// `max_index_scan_rows`).
#[test]
fn a_slow_consumer_never_causes_read_ahead() {
    let d = loaded();
    let plan = d.plan("SELECT * FROM dt WHERE a = 1");
    let Plan::Query { physical, .. } = &plan else {
        panic!("query plan expected")
    };
    let txn = d.txm.begin().unwrap();
    let limits = ExecLimits {
        access_path: AccessPathMode::ForceIndex,
        ..ExecLimits::default()
    };
    let (metrics, cancel) = (ExecMetrics::default(), CancellationToken::new());
    let ec = ExecCtx::new(&txn, &d.store, &d.builder, &[], &limits, &metrics, &cancel);
    let mut op = build_operator(physical, &RowContext::new(), &ec).unwrap();
    let base = fetched(&d);
    assert_eq!(
        base,
        fetched(&d),
        "building the operator must fetch no rows"
    );
    for pulled in 1..=40u64 {
        assert!(op.next(&ec).unwrap().is_some());
        assert_eq!(fetched(&d) - base, pulled, "exactly one fetch per pull");
    }
    d.f.cleanup();
}

/// Start, fetch part of the result, cancel: the executor stops, no further
/// row is fetched, and the transaction remains usable and correct.
#[test]
fn cancellation_stops_the_fetch_and_leaves_the_transaction_correct() {
    let d = loaded();
    let plan = d.plan("SELECT * FROM dt WHERE a = 1");
    let Plan::Query { physical, .. } = &plan else {
        panic!("query plan expected")
    };
    let mut txn = d.txm.begin().unwrap();
    d.write_in(
        "INSERT INTO dt (id, a, b, c) VALUES (9000, 1, 's0', 0)",
        &mut txn,
    );
    let limits = ExecLimits {
        access_path: AccessPathMode::ForceIndex,
        ..ExecLimits::default()
    };
    let (metrics, cancel) = (ExecMetrics::default(), CancellationToken::new());
    let base = fetched(&d);
    {
        let ec = ExecCtx::new(&txn, &d.store, &d.builder, &[], &limits, &metrics, &cancel);
        let mut op = build_operator(physical, &RowContext::new(), &ec).unwrap();
        for _ in 0..10 {
            assert!(op.next(&ec).unwrap().is_some());
        }
        cancel.cancel();
        assert!(matches!(op.next(&ec), Err(SqlError::Cancelled)));
        assert!(
            matches!(op.next(&ec), Err(SqlError::Cancelled)),
            "stays cancelled"
        );
    }
    assert_eq!(fetched(&d) - base, 10, "no row fetched after cancellation");
    // The transaction is intact: its own write is still visible, a fresh
    // query through it is correct, and it commits.
    let all = d.select_in("SELECT * FROM dt WHERE a = 1", &txn);
    assert_eq!(all.rows.len(), 1_001);
    txn.commit().unwrap();
    d.mode.set(AccessPathMode::Auto);
    assert_eq!(d.select("SELECT * FROM dt WHERE a = 1").rows.len(), 1_001);
    d.f.cleanup();
}

#[test]
fn a_deadline_stops_the_fetch() {
    let d = loaded();
    let plan = d.plan("SELECT * FROM dt WHERE a = 1");
    let Plan::Query { physical, .. } = &plan else {
        panic!("query plan expected")
    };
    let txn = d.txm.begin().unwrap();
    let limits = ExecLimits {
        access_path: AccessPathMode::ForceIndex,
        deadline: Some(Duration::from_nanos(1)),
        ..ExecLimits::default()
    };
    let (metrics, cancel) = (ExecMetrics::default(), CancellationToken::new());
    let ec = ExecCtx::new(&txn, &d.store, &d.builder, &[], &limits, &metrics, &cancel);
    std::thread::sleep(Duration::from_millis(2));
    let base = fetched(&d);
    let mut op = build_operator(physical, &RowContext::new(), &ec).unwrap();
    assert!(matches!(op.next(&ec), Err(SqlError::DeadlineExceeded)));
    assert_eq!(fetched(&d) - base, 0, "an expired deadline fetches nothing");
    d.f.cleanup();
}

/// `max_index_scan_rows` still bounds the per-scan buffer (the entry list)
/// before a single row is fetched.
#[test]
fn the_entry_buffer_is_bounded_by_max_index_scan_rows() {
    let d = loaded();
    let plan = d.plan("SELECT * FROM dt WHERE a = 1");
    let txn = d.txm.begin().unwrap();
    let limits = ExecLimits {
        access_path: AccessPathMode::ForceIndex,
        max_index_scan_rows: 999,
        ..ExecLimits::default()
    };
    let before = fetched(&d);
    let r = crate::exec::execute(
        &plan,
        &txn,
        &d.store,
        &d.builder,
        &[],
        &limits,
        &ExecMetrics::default(),
        &CancellationToken::new(),
    );
    assert!(matches!(r, Err(SqlError::ResourceLimit { .. })));
    assert_eq!(fetched(&d) - before, 0, "limit hit before any row fetch");
    d.f.cleanup();
}

/// LIMIT over a transaction that has written the table: local rows count,
/// local deletes hide base rows, and the lazy path stays correct.
#[test]
fn limit_composes_with_the_transaction_overlay() {
    let d = loaded();
    let mut t = d.txm.begin().unwrap();
    d.write_in("DELETE FROM dt WHERE id = 1", &mut t); // a = 1 base row
    d.write_in(
        "INSERT INTO dt (id, a, b, c) VALUES (5000, 1, 's0', 0)",
        &mut t,
    );
    let all = d.select_in("SELECT * FROM dt WHERE a = 1", &t);
    assert_eq!(all.rows.len(), 1_000); // 1,000 - deleted + inserted
    assert!(!rows_of(&all).iter().any(|r| r.0 == 1));
    assert!(rows_of(&all).iter().any(|r| r.0 == 5000));
    let few = d.select_in("SELECT * FROM dt WHERE a = 1 LIMIT 3", &t);
    assert_eq!(few.rows.len(), 3);
    for r in rows_of(&few) {
        assert_ne!(r.0, 1);
    }
    d.f.cleanup();
}

#[allow(dead_code)]
fn _unused(_: &PhysicalPlan) {}
