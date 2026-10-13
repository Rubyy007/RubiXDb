//! Relational oracle (ADR section 12.6, A5 route (a)): class B kills of the relational workload (rows + secondary-index
//! entries written through `TableStore`, one `write_batch` per commit unit) at every engine fault point, judged by the decoded
//! row model and by a raw-keyspace differential against a never-crashed reference. Plus the oracle's own failure tests.
//!
//! Class B only. Single writer only. SQL parsing/planning is above this layer and is not exercised here.

use std::sync::Arc;

use crate::kill_b;
use crate::oracle;
use crate::rel_oracle;
use crate::rel_workload::{self, apply_op, attach, gen_rel_op};
use crate::results;
use crate::rig::{self, Stop};
use crate::scen::{self, Case, Fixture, Scen};
use crate::workload::{self, WriterLedger};
use rubixdb::faults::Point;

#[test]
fn relational_control_clean_run_is_accepted() {
    let c = Case {
        point: Point::WalFsync,
        scen: Scen::Rel,
        trigger: "once".into(),
        stop: Stop::RunToEnd,
        params: scen::REL,
        action: "log".into(),
    };
    let prep = scen::prepare(&c, "control-relational");
    let mut args = prep.args.clone();
    for a in args.iter_mut() {
        if a == "end=exit" {
            *a = "end=clean".into();
        }
    }
    let run = rig::run_child(&prep.work, &args, None, Stop::RunToEnd, None, false);
    assert!(
        run.status.is_some_and(|s| s.success()),
        "relational control did not finish: {} {}",
        run.exit_text(),
        run.stderr_tail
    );
    let (dist, text) = scen::distinguishable_from_clean_stop(Scen::Rel, &prep, &run);
    assert!(!dist, "a completed run was classified as stopped ({text})");
    let j = scen::judge(Scen::Rel, &prep);
    assert!(j.accepted, "relational control rejected: {}", j.detail);
    let l = workload::read_ledger(&prep.ledger, 0).unwrap();
    assert_eq!((l.acked, l.submitted), (scen::REL.ops, scen::REL.ops));
}

#[test]
fn kill9_class_b_relational_every_engine_point() {
    let counts = scen::trace(Scen::Rel, scen::REL);
    let mut cases = Vec::new();
    let mut not_reached = Vec::new();
    for (point, sc) in kill_b::reachable_points() {
        if sc != Scen::Engine {
            continue;
        }
        let total = scen::total(counts, point.name());
        if total == 0 {
            not_reached.push(point.name().to_string());
            continue;
        }
        let trigger = if total >= 4 {
            format!("nth={}", (total / 3).max(2))
        } else {
            "once".to_string()
        };
        cases.push((
            Case {
                point,
                scen: Scen::Rel,
                trigger,
                stop: Stop::SelfAbort,
                params: scen::REL,
                action: "abort".into(),
            },
            "B",
        ));
    }
    std::fs::write(
        results::out_dir().join("relational_not_reached.txt"),
        not_reached.join("\n"),
    )
    .unwrap();
    assert!(
        cases.len() >= 20,
        "the relational workload reaches too few engine points ({}); not reached: {not_reached:?}",
        cases.len()
    );
    let rows = kill_b::run_all(cases, 4);
    results::write("class_b_relational.tsv", &rows);
    let failed: Vec<_> = rows.iter().filter(|r| r.verdict != "PASS").collect();
    let msg: Vec<String> = failed
        .iter()
        .map(|r| format!("{} ({}): {}", r.point, r.trigger, r.detail))
        .collect();
    assert!(
        failed.is_empty(),
        "{} of {} relational class B cases did not pass:\n  {}",
        failed.len(),
        rows.len(),
        msg.join("\n  ")
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// the oracle must be able to fail
// ---------------------------------------------------------------------------------------------------------------------

struct Fixture1 {
    work: std::path::PathBuf,
    template: std::path::PathBuf,
    stack: rel_workload::RelStack,
    ledger: WriterLedger,
    params: workload::Params,
}

/// A real database: the setup template plus the first `k` operations, all acknowledged.
fn built(k: usize, tag: &str) -> Fixture1 {
    let template = scen::template(Fixture::RelSetup).root.join("data");
    let work = rig::fresh_case(&format!("relself-{tag}"));
    let data = work.join("data");
    rig::copy_dir(&template, &data);
    let engine = Arc::new(oracle::product_open(&data).map_err(|e| e.1).expect("open"));
    let stack = attach(Arc::clone(&engine)).expect("attach");
    let params = scen::REL;
    for i in 0..k {
        apply_op(&stack, &gen_rel_op(&params, i)).expect("op");
    }
    Fixture1 {
        work,
        template,
        stack,
        ledger: WriterLedger {
            submitted: k,
            acked: k,
            ack_seqs: (1..=k as u64).collect(),
            errored: 0,
        },
        params,
    }
}

fn invariants(v: &oracle::Verdict) -> Vec<&'static str> {
    v.violations.iter().map(|x| x.invariant).collect()
}

#[test]
fn relational_oracle_accepts_a_correct_database() {
    let f = built(60, "ok");
    let raw = oracle::dump(&f.stack.engine).unwrap();
    let v =
        rel_oracle::check_relational(&f.template, &f.work, &f.params, &f.ledger, &f.stack, &raw);
    assert!(v.ok(), "{}", v.summary());
}

#[test]
fn relational_oracle_rejects_a_row_without_its_index_entry_that_decoded_rows_cannot_see() {
    let f = built(40, "noidx");
    let clean = oracle::dump(&f.stack.engine).unwrap();
    // The index entries of t_name_idx are exactly the raw keys inside `index_entry_range`.
    let (lo, hi) =
        rubixdb::relational::index_key::index_entry_range(f.stack.table_id, f.stack.index_id);
    let in_range = |k: &Vec<u8>| {
        let ge = match &lo {
            std::ops::Bound::Included(l) => k >= l,
            std::ops::Bound::Excluded(l) => k > l,
            std::ops::Bound::Unbounded => true,
        };
        let lt = match &hi {
            std::ops::Bound::Included(h) => k <= h,
            std::ops::Bound::Excluded(h) => k < h,
            std::ops::Bound::Unbounded => true,
        };
        ge && lt
    };
    let index_keys: Vec<Vec<u8>> = clean.keys().filter(|k| in_range(k)).cloned().collect();
    assert!(
        !index_keys.is_empty(),
        "the workload produced no index entries"
    );
    let victim = index_keys[index_keys.len() / 2].clone();
    let mut tampered = clean.clone();
    tampered.remove(&victim);

    // Decoded rows come from the real, healthy stack and are untouched; only the raw keyspace lost one index entry.
    let v = rel_oracle::check_relational(
        &f.template,
        &f.work,
        &f.params,
        &f.ledger,
        &f.stack,
        &tampered,
    );
    assert!(!v.ok(), "a database missing one index entry was accepted");
    assert!(
        invariants(&v).contains(&"R2/R7"),
        "expected the raw differential (R2/R7) to flag it, got {:?}",
        invariants(&v)
    );
    assert!(
        !invariants(&v).contains(&"R1") && !invariants(&v).contains(&"R2/R3/R4"),
        "the decoded-row check should NOT see this (that is the point of the differential): {:?}",
        invariants(&v)
    );

    // And the reverse: a dangling index entry whose row does not exist.
    let mut dangling = clean.clone();
    let mut k = victim.clone();
    k.extend_from_slice(b"-dangling");
    dangling.insert(k, Vec::new());
    let v = rel_oracle::check_relational(
        &f.template,
        &f.work,
        &f.params,
        &f.ledger,
        &f.stack,
        &dangling,
    );
    assert!(
        !v.ok() && invariants(&v).contains(&"R2/R7"),
        "a dangling index entry was accepted: {:?}",
        invariants(&v)
    );
}

#[test]
fn relational_oracle_rejects_altered_extra_and_missing_raw_data() {
    let f = built(50, "raw");
    let clean = oracle::dump(&f.stack.engine).unwrap();
    let ns = rubixdb::relational::key::RELATIONAL_NAMESPACE;
    let some_key = clean
        .keys()
        .find(|k| k.first() == Some(&ns))
        .cloned()
        .unwrap();
    // altered value
    let mut t = clean.clone();
    t.get_mut(&some_key).unwrap().push(1);
    assert!(
        !rel_oracle::check_relational(&f.template, &f.work, &f.params, &f.ledger, &f.stack, &t)
            .ok(),
        "altered raw value accepted"
    );
    // extra key
    let mut t = clean.clone();
    let mut extra = some_key.clone();
    extra.push(0xFF);
    t.insert(extra, b"dangling".to_vec());
    assert!(
        !rel_oracle::check_relational(&f.template, &f.work, &f.params, &f.ledger, &f.stack, &t)
            .ok(),
        "dangling raw key accepted"
    );
    // missing key
    let mut t = clean.clone();
    t.remove(&some_key);
    assert!(
        !rel_oracle::check_relational(&f.template, &f.work, &f.params, &f.ledger, &f.stack, &t)
            .ok(),
        "missing raw key accepted"
    );
}

#[test]
fn relational_oracle_rejects_a_lost_acknowledged_row() {
    // The database really lacks the effect of operation `k`, but the ledger says it was acknowledged. Pick a `k` whose
    // effect is visible in the state (a no-op operation could not be told apart from a lost one).
    let p = scen::REL;
    let k = (40..120)
        .find(|k| rel_workload::rel_model(&p, *k) != rel_workload::rel_model(&p, *k + 1))
        .expect("an operation that changes the state exists in this seed");
    let f = built(k, "lost");
    let mut l = f.ledger.clone();
    l.submitted = k + 1;
    l.acked = k + 1;
    l.ack_seqs.push(k as u64 + 1);
    let raw = oracle::dump(&f.stack.engine).unwrap();
    let v = rel_oracle::check_relational(&f.template, &f.work, &f.params, &l, &f.stack, &raw);
    assert!(!v.ok(), "a lost acknowledged operation ({k}) was accepted");
    assert!(
        invariants(&v)
            .iter()
            .any(|i| *i == "R1" || *i == "R2/R3/R4"),
        "{:?}",
        invariants(&v)
    );
}

#[test]
fn relational_oracle_rejects_an_out_of_order_commit() {
    // Build a database that applied every operation of 0..60 except one in the middle.
    let template = scen::template(Fixture::RelSetup).root.join("data");
    let p = scen::REL;
    let mut skip = None;
    for s in 0..58 {
        let mut without = std::collections::BTreeMap::new();
        for i in 0..60 {
            if i == s {
                continue;
            }
            match gen_rel_op(&p, i) {
                rel_workload::RelOp::Upsert { id, name, grp } => {
                    without.insert(id, (name, grp));
                }
                rel_workload::RelOp::Delete { id } => {
                    without.remove(&id);
                }
            }
        }
        if rel_workload::rel_model(&p, 60) != without {
            skip = Some(s);
            break;
        }
    }
    let skip = skip.expect("a skippable operation");
    let work = rig::fresh_case("relself-ooo");
    let data = work.join("data");
    rig::copy_dir(&template, &data);
    let engine = Arc::new(oracle::product_open(&data).map_err(|e| e.1).unwrap());
    let stack = attach(Arc::clone(&engine)).unwrap();
    for i in (0..60).filter(|i| *i != skip) {
        apply_op(&stack, &gen_rel_op(&p, i)).unwrap();
    }
    let l = WriterLedger {
        submitted: 60,
        acked: 60,
        ack_seqs: (1..=60).collect(),
        errored: 0,
    };
    let raw = oracle::dump(&engine).unwrap();
    let v = rel_oracle::check_relational(&template, &work, &p, &l, &stack, &raw);
    assert!(
        !v.ok(),
        "skipping operation {skip} while applying later ones was accepted"
    );
}
