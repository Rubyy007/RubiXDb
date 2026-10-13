//! Crash class B - process kill - at every reachable fault point, judged by the independent oracle.
//!
//! **Every row produced here is "crash class B - process kill". Kill-9 is not power-loss evidence**: the OS keeps every byte
//! a returned `write()` gave it, synced or not, so a missing fsync is invisible to this test by construction.

use std::sync::Mutex;

use rubixdb::faults::Point;

use crate::results::{self, Row};
use crate::rig::{self, Outcome, Stop};
use crate::scen::{self, Case, Scen};
use crate::status::{self, Status};

fn stop_name(s: Stop) -> &'static str {
    match s {
        Stop::SelfAbort => "abort (self)",
        Stop::ParentKill => "pause + Child::kill()",
        Stop::RunToEnd => "run to end",
    }
}

/// Executes one case and judges it. `class` is "A" or "B".
pub fn exec(c: &Case, class: &'static str) -> Row {
    let label = format!(
        "{class}-{}-{}-w{}-{}",
        c.point.name(),
        c.trigger,
        c.params.writers,
        match c.stop {
            Stop::SelfAbort => "abort",
            Stop::ParentKill => "kill",
            Stop::RunToEnd => "end",
        }
    );
    let prep = scen::prepare(c, &label);
    let spec = scen::spec_of(c);
    let run = rig::run_child(
        &prep.work,
        &prep.args,
        Some(&spec),
        c.stop,
        Some(c.point.name()),
        false,
    );
    let state_at_stop = rig::snapshot(&prep.data);
    let mut row = Row {
        class,
        point: c.point.name().to_string(),
        scenario: c.scen.label(),
        writers: prep.params.writers,
        trigger: format!("{} -> {}", c.trigger, c.action),
        stop: stop_name(c.stop).to_string(),
        hit: run.fires.first().map(|f| f.hit).unwrap_or(0),
        verdict: "FAIL",
        distinguishable: String::new(),
        snapshot: state_at_stop,
        detail: String::new(),
    };
    match &run.outcome {
        Outcome::PointNotReached(why) => {
            row.detail = format!("POINT NOT REACHED: {why}");
            return row;
        }
        Outcome::KillTimeout => {
            row.detail = format!(
                "KILL TIMEOUT: child did not exit {:?} after Child::kill(); case dir kept: {}",
                rig::KILL_TIMEOUT,
                prep.work.display()
            );
            return row;
        }
        Outcome::Fired => {}
    }
    let (dist, dist_text) = if class == "B" {
        let (d, t) = scen::distinguishable_from_clean_stop(c.scen, &prep, &run);
        (d, t)
    } else {
        let markers: Vec<String> = std::fs::read_dir(&prep.ledger)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| !n.starts_with("ledger_"))
                    .collect()
            })
            .unwrap_or_default();
        (
            true,
            format!(
                "n/a (class A: the process lives); {}; markers {:?}; stderr: {}",
                run.exit_text(),
                markers,
                run.stderr_tail
            ),
        )
    };
    row.distinguishable = dist_text;
    let j = scen::judge(c.scen, &prep);
    row.detail = j.detail.clone();
    if let Some(r) = &j.report {
        row.detail.push_str(&format!(
            " || pre-recovery: wal_corrupted_segments={} wal_torn_tail={} physical_errors={} warnings={}; post_seq={:?} idempotent={:?} checkpoint={}; {}",
            r.pre_wal_corrupted_segments,
            r.pre_wal_truncated,
            r.pre_physical_errors,
            r.pre_physical_warnings,
            r.post_seq,
            r.idempotent,
            r.checkpoint_seq,
            r.recovery_stats
        ));
    }
    if j.accepted && dist {
        row.verdict = "PASS";
        let _ = std::fs::remove_dir_all(&prep.work);
    } else if j.accepted && !dist {
        row.detail = format!("NOT DISTINGUISHABLE FROM A CLEAN STOP (the rig did not stop the process mid-operation): {}", row.detail);
    }
    row
}

pub fn run_all(cases: Vec<(Case, &'static str)>, workers: usize) -> Vec<Row> {
    let queue = Mutex::new(cases.into_iter().enumerate().collect::<Vec<_>>());
    let out: Mutex<Vec<(usize, Row)>> = Mutex::new(Vec::new());
    std::thread::scope(|sc| {
        for _ in 0..workers {
            sc.spawn(|| loop {
                let next = queue.lock().unwrap_or_else(|p| p.into_inner()).pop();
                let Some((i, (c, class))) = next else { break };
                let row = exec(&c, class);
                out.lock().unwrap_or_else(|p| p.into_inner()).push((i, row));
            });
        }
    });
    let mut v = out.into_inner().unwrap_or_else(|p| p.into_inner());
    v.sort_by_key(|(i, _)| *i);
    v.into_iter().map(|(_, r)| r).collect()
}

pub fn reachable_points() -> Vec<(Point, Scen)> {
    let table = status::table();
    Point::ALL
        .iter()
        .filter(|p| {
            table
                .get(p.name())
                .is_some_and(|r| r.status == Status::Reachable)
        })
        .filter_map(|p| scen::scenario_of(*p).map(|s| (*p, s)))
        .collect()
}

/// Chooses a seed whose `prob=` schedule first fires inside the hits the workload really makes (the schedule is the same
/// pure function the registry evaluates).
fn prob_trigger(point: Point, hits: u64) -> Option<String> {
    if hits < 6 {
        return None;
    }
    let upto = hits * 2 / 3;
    let p = (4.0 / hits as f64).min(1.0);
    for seed in 1..2000u64 {
        let sched = rubixdb::faults::prob_schedule(p, seed, point, upto);
        if let Some(first) = sched.first() {
            if *first >= 2 {
                return Some(format!("prob={p},seed={seed}"));
            }
        }
    }
    None
}

pub fn build_b_cases() -> Vec<(Case, &'static str)> {
    let mut cases = Vec::new();
    for (point, sc) in reachable_points() {
        let name = point.name();
        let counts = scen::trace(sc, scen::DEFAULT);
        let total = scen::total(counts, name);
        let base = |trigger: String, stop: Stop, params, action: &str| Case {
            point,
            scen: sc,
            trigger,
            stop,
            params,
            action: action.into(),
        };
        // (a) first hit, the process kills itself there.
        cases.push((
            base("once".into(), Stop::SelfAbort, scen::DEFAULT, "abort"),
            "B",
        ));
        // (b) a later hit (a third of the way through the workload), self-kill.
        if total >= 4 {
            cases.push((
                base(
                    format!("nth={}", (total / 3).max(2)),
                    Stop::SelfAbort,
                    scen::DEFAULT,
                    "abort",
                ),
                "B",
            ));
        }
        // (c) first hit, paused there and killed from outside (the true external kill).
        cases.push((
            base("once".into(), Stop::ParentKill, scen::DEFAULT, "pause"),
            "B",
        ));
        // (d) four concurrent writers, seeded-probabilistic trigger (engine workload only).
        if sc == Scen::Engine {
            let h = scen::total(scen::trace(Scen::Engine, scen::CONCURRENT), name);
            if let Some(t) = prob_trigger(point, h) {
                cases.push((base(t, Stop::SelfAbort, scen::CONCURRENT, "abort"), "B"));
            }
        }
    }
    cases
}

fn judge_rows(name: &str, rows: &[Row]) {
    results::write(name, rows);
    let failed: Vec<&Row> = rows.iter().filter(|r| r.verdict != "PASS").collect();
    let msg: Vec<String> = failed
        .iter()
        .map(|r| {
            format!(
                "[{}] {} ({}, {}, w={}): {}",
                r.class, r.point, r.scenario, r.trigger, r.writers, r.detail
            )
        })
        .collect();
    assert!(
        failed.is_empty(),
        "{} of {} class B cases did not pass (case directories of failures are kept):\n  {}",
        failed.len(),
        rows.len(),
        msg.join("\n  ")
    );
}

/// Single writer: the commit order, the MemTable-apply order and the checkpoint all agree, so this column is expected to be
/// fully green and is the column D1a certifies.
#[test]
fn kill9_class_b_single_writer_every_reachable_point() {
    let cases: Vec<_> = build_b_cases()
        .into_iter()
        .filter(|(c, _)| c.params.writers == 1)
        .collect();
    assert!(cases.len() > 100, "suspiciously few cases: {}", cases.len());
    let rows = run_all(cases, 4);
    judge_rows("class_b_single_writer.tsv", &rows);
}

/// Four concurrent writers (seeded-probabilistic trigger). **Currently RED by design of the evidence, not by weakening:**
/// a pre-existing engine defect (F-D1a-1, `OPEN_ITEMS.md`) loses acknowledged writes under concurrent writers whenever a
/// MemTable freeze lands between a batch member's durable acknowledgement and its MemTable apply; it reproduces on a pristine
/// export of dde8ae9 without any fault rule and across a graceful stop. Every failing row here carries an automatic
/// attribution ("F-D1a-1 SIGNATURE"); a row that fails WITHOUT that signature is a different, unexplained problem. This test
/// stays strict (it is not marked ignored or expected-to-fail) so it turns green only when the defect is fixed.
#[test]
fn kill9_class_b_concurrent_writers_every_engine_point() {
    let cases: Vec<_> = build_b_cases()
        .into_iter()
        .filter(|(c, _)| c.params.writers > 1)
        .collect();
    assert!(!cases.is_empty(), "no concurrent cases were built");
    let rows = run_all(cases, 4);
    let unexplained: Vec<&Row> = rows
        .iter()
        .filter(|r| r.verdict != "PASS" && !r.detail.contains("F-D1a-1 SIGNATURE on all"))
        .collect();
    results::write("class_b_concurrent.tsv", &rows);
    let failed = rows.iter().filter(|r| r.verdict != "PASS").count();
    eprintln!(
        "concurrent class B: {failed} of {} rows failed; {} of those WITHOUT the F-D1a-1 signature",
        rows.len(),
        unexplained.len()
    );
    judge_rows("class_b_concurrent.tsv", &rows);
}

/// Controls: the same workloads without any fault run to completion, are accepted by the oracle, and are NOT
/// distinguishable-from-clean-stop in the rig's sense (so the rig's "distinguishable" criterion can say no).
#[test]
fn control_runs_are_accepted_and_not_mistaken_for_a_stop() {
    for sc in [Scen::Engine, Scen::Recover(scen::Fixture::AbortEnd)] {
        let c = Case {
            point: Point::WalFsync,
            scen: sc,
            trigger: "once".into(),
            stop: Stop::RunToEnd,
            params: scen::DEFAULT,
            action: "log".into(),
        };
        let prep = scen::prepare(&c, &format!("control-{}", sc.label()));
        let mut args = prep.args.clone();
        if sc == Scen::Engine {
            for a in args.iter_mut() {
                if a == "end=exit" {
                    *a = "end=clean".into();
                }
            }
        }
        let run = rig::run_child(&prep.work, &args, None, Stop::RunToEnd, None, false);
        assert!(
            run.status.is_some_and(|s| s.success()),
            "{}: control did not finish: {} {}",
            sc.label(),
            run.exit_text(),
            run.stderr_tail
        );
        let (dist, text) = scen::distinguishable_from_clean_stop(sc, &prep, &run);
        assert!(
            !dist,
            "{}: a run that completed was classified as stopped mid-operation ({text})",
            sc.label()
        );
        let j = scen::judge(sc, &prep);
        assert!(
            j.accepted,
            "{}: control run rejected by the oracle: {}",
            sc.label(),
            j.detail
        );
    }
}
