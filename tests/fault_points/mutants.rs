//! Deliberate-failure checks of the WHOLE pipeline (rig + registry + ledger + oracle), as the mission asks: important tests
//! must fail when a guarantee is intentionally broken. These are not tests of the engine.

use std::process::{Command, Stdio};

use rubixdb::faults::Point;

use crate::rig::{self, Outcome, Stop};
use crate::scen::{self, Case, Scen};

/// A child that acknowledges BEFORE the engine call (a lie about durability), stopped at `wal.record_write` (before the
/// append): the recovered state lacks operations the ledger claims were acknowledged, so the oracle has to reject it with R1.
/// If this passed, every "accepted" in the kill matrix would be meaningless.
#[test]
fn mutant_early_ack_is_rejected_with_r1() {
    let case = Case {
        point: Point::WalRecordWrite,
        scen: Scen::Engine,
        trigger: "nth=20".into(),
        stop: Stop::SelfAbort,
        params: scen::DEFAULT,
        action: "abort".into(),
    };
    let prep = scen::prepare(&case, "mutant-early-ack");
    let mut args = prep.args.clone();
    args.push("mutant=early_ack".into());
    let run = rig::run_child(
        &prep.work,
        &args,
        Some("wal.record_write=nth=20:abort"),
        Stop::SelfAbort,
        Some("wal.record_write"),
        false,
    );
    assert_eq!(
        run.outcome,
        Outcome::Fired,
        "the mutant run must stop at the point: {:?}",
        run.outcome
    );
    let j = scen::judge(Scen::Engine, &prep);
    assert!(
        !j.accepted,
        "a run that acknowledged before writing was ACCEPTED by the oracle: {}",
        j.detail
    );
    assert!(
        j.detail.contains("R1"),
        "expected an R1 (acknowledged commit lost) rejection, got: {}",
        j.detail
    );
}

/// The same mutant without a stop point is still consistent (everything it claimed was in fact written): the oracle must
/// accept it, which shows the previous rejection comes from the lie being caught and not from the mutant's mere presence.
#[test]
fn mutant_early_ack_without_a_stop_is_accepted() {
    let case = Case {
        point: Point::WalFsync,
        scen: Scen::Engine,
        trigger: "once".into(),
        stop: Stop::RunToEnd,
        params: scen::DEFAULT,
        action: "log".into(),
    };
    let prep = scen::prepare(&case, "mutant-early-ack-control");
    let mut args = prep.args.clone();
    for a in args.iter_mut() {
        if a == "end=exit" {
            *a = "end=clean".into();
        }
    }
    args.push("mutant=early_ack".into());
    let run = rig::run_child(&prep.work, &args, None, Stop::RunToEnd, None, false);
    assert!(
        run.status.is_some_and(|s| s.success()),
        "{} {}",
        run.exit_text(),
        run.stderr_tail
    );
    let j = scen::judge(Scen::Engine, &prep);
    assert!(j.accepted, "{}", j.detail);
}

/// An unparsable `RUBIXDB_FAULT_SPEC` is fatal (exit status 86) and the process does no work: a rule that can never fire must
/// not look like a passed test.
#[test]
fn invalid_fault_spec_is_fatal() {
    let work = rig::fresh_case("invalid-spec");
    for bad in [
        "no.such.point=once:abort",
        "wal.fsync=once:explode",
        "wal.record_written=once:error=other",
        "wal.fsync=nth=0:abort",
    ] {
        let status = Command::new(rig::child_exe())
            .args([
                "engine",
                work.join("d").to_str().unwrap(),
                work.join("l").to_str().unwrap(),
            ])
            .env("RUBIXDB_FAULT_SPEC", bad)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(86),
            "spec {bad:?} was not rejected fatally: {status}"
        );
    }
    assert!(
        !work.join("l").join("DONE").exists(),
        "a child with an invalid spec ran its workload"
    );
}
