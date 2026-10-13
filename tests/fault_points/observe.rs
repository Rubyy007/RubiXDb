//! Observations for the maintainer's decision gate (F-D0-1/2/3/4/5). They are NOT part of the pass/fail matrix: the expected
//! state for these shapes is exactly what the maintainer has not yet decided, so a verdict here would be invented. Each test
//! writes what the real engine did to `target/fault-points-results/observation_*.txt`.
//!
//! Run: `cargo test --features fault-points --test fault_points observe -- --ignored --nocapture --test-threads=1`

use rubixdb::faults::Point;

use crate::results;
use crate::rig::{self, Stop};
use crate::scen::{self, Case, Scen};

/// F-D0-3 through the REAL call site: `faults::write_all` at `Manifest::append_sync` writes a 7-byte prefix of the frame and
/// returns `WriteZero` (what a short write followed by an error looks like). The flush thread retries the whole flush, which
/// appends the full frame AFTER the prefix. D0 predicted the file then cannot be opened (the probe built that byte pattern by
/// hand). This shows what the engine itself does. The premise - an OS `write` that returns `Err` after a partial write on
/// this machine - is still NOT shown reachable.
#[test]
#[ignore]
fn observe_f_d0_3_manifest_short_write() {
    let case = Case {
        point: Point::ManifestWrite,
        scen: Scen::Engine,
        trigger: "nth=3".into(),
        stop: Stop::RunToEnd,
        params: scen::DEFAULT,
        action: "shortwrite=7".into(),
    };
    let prep = scen::prepare(&case, "observe-f-d0-3");
    let run = rig::run_child(
        &prep.work,
        &prep.args,
        Some(&scen::spec_of(&case)),
        Stop::RunToEnd,
        Some("manifest.write"),
        false,
    );
    let mut text = String::new();
    text.push_str("F-D0-3 observation (class A: short write at manifest.write, process alive)\n");
    text.push_str(&format!(
        "rule: {}\nrun outcome: {:?}; {}\nchild stderr tail: {}\n",
        scen::spec_of(&case),
        run.outcome,
        run.exit_text(),
        run.stderr_tail
    ));
    text.push_str(&format!(
        "state after the child ended: {}\n",
        rig::snapshot(&prep.data)
    ));
    let manifest_len = std::fs::metadata(prep.data.join("MANIFEST"))
        .map(|m| m.len())
        .unwrap_or(0);
    text.push_str(&format!("MANIFEST length: {manifest_len} bytes\n"));
    let j = scen::judge(Scen::Engine, &prep);
    text.push_str(&format!(
        "recovery + oracle: accepted={}; {}\n",
        j.accepted, j.detail
    ));
    println!("{text}");
    std::fs::write(results::out_dir().join("observation_f_d0_3.txt"), text).unwrap();
}
