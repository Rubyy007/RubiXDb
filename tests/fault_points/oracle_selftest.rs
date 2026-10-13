//! The oracle must be able to fail. Each case feeds it a recovered keyspace that deliberately breaks one invariant and
//! requires a violation of the expected kind. If any of these passes silently, every "accepted" in the kill matrix is
//! worthless - so these run in the same binary and must all hold.

use std::collections::BTreeMap;

use crate::oracle::{check_seq, check_state, Dump};
use crate::workload::{apply, gen_op, key, model_prefix, Item, Params, WriterLedger};

const P: Params = Params {
    seed: 3,
    writers: 2,
    ops: 60,
    keys: 8,
};

fn ledgers(acked: usize, submitted: usize) -> Vec<WriterLedger> {
    (0..P.writers)
        .map(|_| WriterLedger {
            submitted,
            acked,
            ack_seqs: (1..=acked as u64).collect(),
            errored: 0,
        })
        .collect()
}

fn full_state(k: usize) -> Dump {
    let mut d = Dump::new();
    for w in 0..P.writers {
        d.extend(model_prefix(&P, w, k));
    }
    d
}

fn invariants(d: &Dump, l: &[WriterLedger]) -> Vec<&'static str> {
    check_state(&P, l, d)
        .violations
        .iter()
        .map(|v| v.invariant)
        .collect()
}

#[test]
fn oracle_accepts_the_exact_acknowledged_state() {
    let v = check_state(&P, &ledgers(60, 60), &full_state(60));
    assert!(v.ok(), "{}", v.summary());
}

#[test]
fn oracle_accepts_unacknowledged_tail_present_or_absent() {
    // 58 acknowledged, 60 submitted: any K in 58..=60 is legal (C-ACK-3), never anything else.
    for k in 58..=60 {
        let v = check_state(&P, &ledgers(58, 60), &full_state(k));
        assert!(v.ok(), "K={k}: {}", v.summary());
    }
}

#[test]
fn oracle_rejects_a_removed_acknowledged_key() {
    let mut d = full_state(60);
    let victim = d.keys().next().cloned().expect("a key");
    d.remove(&victim);
    let inv = invariants(&d, &ledgers(60, 60));
    assert!(inv.contains(&"R1"), "expected an R1 violation, got {inv:?}");
}

#[test]
fn oracle_rejects_an_altered_value() {
    let mut d = full_state(60);
    let victim = d.keys().next().cloned().expect("a key");
    d.get_mut(&victim).unwrap().push(b'!');
    let inv = invariants(&d, &ledgers(60, 60));
    assert!(
        inv.contains(&"R4"),
        "expected an R4 (malformed value) violation, got {inv:?}"
    );
}

#[test]
fn oracle_rejects_an_extra_key() {
    let mut d = full_state(60);
    d.insert(key(0, 999), b"never submitted".to_vec());
    let inv = invariants(&d, &ledgers(60, 60));
    assert!(
        inv.contains(&"R4"),
        "expected an R4 (phantom key) violation, got {inv:?}"
    );
    let mut d2 = full_state(60);
    d2.insert(b"stray-key".to_vec(), b"x".to_vec());
    assert!(invariants(&d2, &ledgers(60, 60)).contains(&"R4"));
}

#[test]
fn oracle_rejects_an_out_of_order_commit() {
    // Apply every operation of writer 0 EXCEPT one in the middle whose absence changes the state: a later commit is
    // present while an earlier one is not. That is not a prefix of the commit sequence.
    let mut chosen = None;
    for skip in 0..59 {
        let mut with = BTreeMap::new();
        let mut without = BTreeMap::new();
        for i in 0..60 {
            let op = gen_op(&P, 0, i);
            apply(&mut with, &op);
            if i != skip {
                apply(&mut without, &op);
            }
        }
        if with != without {
            chosen = Some((skip, without));
            break;
        }
    }
    let (skip, without) = chosen.expect("a skippable operation exists for this seed");
    let mut d = full_state(60);
    d.retain(|k, _| !k.starts_with(b"w00-"));
    d.extend(without);
    let inv = invariants(&d, &ledgers(60, 60));
    assert!(
        !inv.is_empty(),
        "skipping commit {skip} while applying later ones was accepted"
    );
    assert!(
        inv.iter().any(|i| *i == "R1" || *i == "R2/R3"),
        "unexpected labels {inv:?}"
    );
}

#[test]
fn oracle_rejects_a_lost_acknowledged_suffix() {
    // 60 acknowledged but the recovered state is the state after 55: acknowledged commits were lost.
    let inv = invariants(&full_state(55), &ledgers(60, 60));
    assert!(inv.contains(&"R1"), "expected R1, got {inv:?}");
}

#[test]
fn oracle_rejects_a_partially_applied_batch() {
    // Find a multi-item commit unit and recover it half-applied.
    for i in 0..60 {
        let op = gen_op(&P, 0, i);
        if op.len() < 2 {
            continue;
        }
        let mut m0 = model_prefix(&P, 0, i);
        let part: Vec<Item> = vec![op[0].clone()];
        apply(&mut m0, &part);
        let mut whole_before = m0.clone();
        let mut full_next = model_prefix(&P, 0, i);
        apply(&mut full_next, &op);
        if whole_before == full_next || whole_before == model_prefix(&P, 0, i) {
            continue; // the first item alone is indistinguishable from a legal state for this unit
        }
        whole_before.retain(|_, _| true);
        let mut d = Dump::new();
        d.extend(whole_before);
        d.extend(model_prefix(&P, 1, i));
        let mut l = ledgers(i, i + 1);
        l[1] = WriterLedger {
            submitted: i,
            acked: i,
            ack_seqs: (1..=i as u64).collect(),
            errored: 0,
        };
        let inv = invariants(&d, &l);
        assert!(
            inv.contains(&"R2"),
            "a half-applied batch at op {i} was not flagged R2: {inv:?}"
        );
        return;
    }
    panic!("no distinguishable multi-item unit for this seed; choose another");
}

#[test]
fn oracle_rejects_a_sequence_number_below_the_checkpoint() {
    // R6 / F-D0-5: after recovery the next write must be numbered above everything acknowledged and above the checkpoint.
    assert!(check_seq(3, 60, 40).is_some_and(|v| v.invariant == "R6"));
    assert!(check_seq(50, 60, 40).is_some());
    assert!(check_seq(61, 60, 61).is_some());
    assert!(check_seq(62, 60, 61).is_none());
}
