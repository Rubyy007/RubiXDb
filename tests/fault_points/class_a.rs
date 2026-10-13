//! Crash class A - a returned I/O error with the process alive - at every reachable point whose call site can carry an error.
//! The injected `Err` flows into the same `Result` the real operation would have produced, so the engine's REAL failure
//! path runs (rollback, poisoning, retry, cleanup). Judged afterwards by the same oracle: nothing acknowledged is lost,
//! nothing malformed appears, the directory still recovers and a second recovery changes nothing.
//!
//! Class A is not a crash and not power loss. It supports error-path correctness (no false acknowledgement, no
//! unrecoverable state) only.

use rubixdb::faults::Point;

use crate::kill_b;
use crate::results;
use crate::rig::Stop;
use crate::scen::{self, Case};

#[test]
fn class_a_error_injection_every_error_capable_point() {
    let mut cases = Vec::new();
    for (point, sc) in kill_b::reachable_points() {
        if !point.supports_error() {
            continue;
        }
        let total = scen::total(scen::trace(sc, scen::DEFAULT), point.name());
        let trigger = if sc == scen::Scen::Engine && total >= 4 {
            format!("nth={}", (total / 3).max(2))
        } else {
            "once".to_string()
        };
        cases.push((
            Case {
                point,
                scen: sc,
                trigger,
                stop: Stop::RunToEnd,
                params: scen::DEFAULT,
                action: "error=other".into(),
            },
            "A",
        ));
    }
    assert!(
        cases.len() >= 30,
        "suspiciously few class A cases: {}",
        cases.len()
    );
    let rows = kill_b::run_all(cases, 4);
    results::write("class_a.tsv", &rows);
    let failed: Vec<&results::Row> = rows.iter().filter(|r| r.verdict != "PASS").collect();
    let msg: Vec<String> = failed
        .iter()
        .map(|r| {
            format!(
                "[{}] {} ({}, {}): {}",
                r.class, r.point, r.scenario, r.trigger, r.detail
            )
        })
        .collect();
    assert!(
        failed.is_empty(),
        "{} of {} class A cases did not pass:\n  {}",
        failed.len(),
        rows.len(),
        msg.join("\n  ")
    );
    // The points that cannot carry an error are listed explicitly rather than silently skipped.
    let skipped: Vec<&str> = Point::ALL
        .iter()
        .filter(|p| !p.supports_error())
        .map(|p| p.name())
        .collect();
    std::fs::write(
        results::out_dir().join("class_a_not_applicable.txt"),
        skipped.join("\n"),
    )
    .unwrap();
}
